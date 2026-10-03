use super::*;
use std::os::unix::process::ExitStatusExt;

struct PreparedTest {
    source: String,
    link: CLinkPlan,
}

pub(super) fn run(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
) -> Vec<CTestRun> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut files = HashMap::new();
    for (index, (func, _)) in tests.iter().enumerate() {
        let next = groups.len();
        let group = *files.entry(ws.module(func.module).file).or_insert(next);
        if group == next {
            groups.push(Vec::new());
        }
        groups[group].push(index);
    }
    let ablation = skull_mir::ablation::current_context();
    let batches = groups
        .par_iter()
        .map(|indices| {
            let _ablation = skull_mir::ablation::enter_context(ablation.clone());
            run_file(path, ws, tests, indices, engine_builds)
        })
        .collect::<Vec<_>>();
    let mut runs = (0..tests.len()).map(|_| None).collect::<Vec<_>>();
    for batch in batches {
        for (index, run) in batch {
            runs[index] = Some(run);
        }
    }
    runs.into_iter()
        .map(|run| run.expect("every C test belongs to a file"))
        .collect()
}

fn prepare(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    func: FuncRef,
    test: &skull::SkullTest,
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
) -> std::result::Result<PreparedTest, CTestRun> {
    if test.returns.is_some() {
        return skull_codegen_c::emit_value_test(ws, func, &test.name)
            .map(|emitted| PreparedTest {
                source: emitted.source,
                link: CLinkPlan::default(),
            })
            .map_err(|error| CTestRun::Skipped(error.message));
    }
    let emitted =
        skull_codegen_c::emit_test(ws, func).map_err(|error| CTestRun::Skipped(error.message))?;
    let link = {
        let mut builds = engine_builds.lock().expect("C engine builds lock");
        c_link_plan_with(path, &emitted, &mut builds)
    }
    .map_err(|error| CTestRun::Ran(Err(error.to_string())))?;
    Ok(PreparedTest {
        source: emitted.source,
        link,
    })
}

fn run_file(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    indices: &[usize],
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
) -> Vec<(usize, CTestRun)> {
    let mut results = Vec::new();
    let mut prepared = Vec::new();
    let ablation = skull_mir::ablation::current_context();
    let attempts = indices
        .par_iter()
        .map(|&index| {
            let _ablation = skull_mir::ablation::enter_context(ablation.clone());
            let (func, test) = &tests[index];
            (index, prepare(path, ws, *func, test, engine_builds))
        })
        .collect::<Vec<_>>();
    for (index, attempt) in attempts {
        match attempt {
            Ok(test) => prepared.push((index, test)),
            Err(run) => results.push((index, run)),
        }
    }
    if prepared.is_empty() {
        return results;
    }
    match compile_batch(&prepared) {
        Ok(compiled) => {
            let output_dir = compiled._dir.path();
            let timeout = C_NATIVE_TEST_TIMEOUT * prepared.len() as u32 + Duration::from_secs(5);
            let batch = run_supervisor(&compiled.binary, output_dir, None, timeout);
            for (slot, (index, _)) in prepared.iter().enumerate() {
                // A crashed supervisor may leave later tests unstarted. Reuse the
                // binary in single-test mode, retaining every completed result.
                if !output_dir.join(format!("{slot}.status")).exists() {
                    if let Err(error) = &batch {
                        log::warn!(target: "TEST", "C test supervisor failed; retrying {}: {error}", tests[*index].1.name);
                    }
                    if let Err(error) = run_supervisor(
                        &compiled.binary,
                        output_dir,
                        Some(slot),
                        C_NATIVE_TEST_TIMEOUT + Duration::from_secs(5),
                    ) {
                        results.push((*index, CTestRun::Ran(Err(error))));
                        continue;
                    }
                }
                let result = read_result(output_dir, slot).and_then(|stdout| {
                    let test = &tests[*index].1;
                    if test.returns.is_some() {
                        value_result(&test.name, &stdout)
                    } else {
                        Ok(stdout)
                    }
                });
                results.push((*index, CTestRun::Ran(result)));
            }
        }
        Err(error) => {
            // A clang rejection is per test, just like an emission rejection.
            // Isolate the failed translation unit without failing its siblings.
            log::warn!(target: "TEST", "C test batch did not build; retrying its tests individually: {}",
                error.split("\n--- generated C ---").next().unwrap_or(&error));
            for (index, test) in prepared {
                let result = if tests[index].1.returns.is_some() {
                    compile_and_run(&tests[index].1.name, &test.source)
                } else {
                    compile_and_run_program(&test.source, &test.link)
                };
                results.push((index, CTestRun::Ran(result)));
            }
        }
    }
    results
}

fn extend_unique<T: PartialEq + Clone>(target: &mut Vec<T>, source: &[T]) {
    for value in source {
        if !target.contains(value) {
            target.push(value.clone());
        }
    }
}

fn compile_batch(tests: &[(usize, PreparedTest)]) -> std::result::Result<CompiledC, String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let binary = dir.path().join("test-bin");
    let mut sources = Vec::new();
    let mut link = CLinkPlan::default();
    let mut supervisor = format!(
        "#define ROCK_TEST_COUNT {}\n#define ROCK_TEST_TIMEOUT_SECONDS {}\n",
        tests.len(),
        C_NATIVE_TEST_TIMEOUT.as_secs()
    );
    let objects = tests
        .par_iter()
        .enumerate()
        .map(|(slot, (_, test))| {
            let source = format!("#define main rock_c_test_{slot}\n{}", test.source);
            let path = dir.path().join(format!("test-{slot}.c"));
            let object = dir.path().join(format!("test-{slot}.o"));
            fs_err::write(&path, &source).map_err(|error| error.to_string())?;
            clang_compile_object(&source, &path, &object, &test.link.include_dirs)?;
            Ok((object, source))
        })
        .collect::<Vec<std::result::Result<_, String>>>();
    for object in objects {
        sources.push(object?);
    }
    for (slot, (_, test)) in tests.iter().enumerate() {
        supervisor.push_str(&format!("extern int rock_c_test_{slot}(void);\n"));
        extend_unique(&mut link.include_dirs, &test.link.include_dirs);
        extend_unique(&mut link.library_dirs, &test.link.library_dirs);
        extend_unique(&mut link.libraries, &test.link.libraries);
        extend_unique(&mut link.rpaths, &test.link.rpaths);
    }
    supervisor.push_str("static int (*const rock_tests[])(void) = {\n");
    for slot in 0..tests.len() {
        supervisor.push_str(&format!("rock_c_test_{slot},\n"));
    }
    supervisor.push_str("};\n");
    supervisor.push_str(include_str!("c_test_supervisor.c"));
    let path = dir.path().join("supervisor.c");
    fs_err::write(&path, &supervisor).map_err(|error| error.to_string())?;
    sources.push((path, supervisor));
    clang_compile_sources(&sources, &binary, &link)?;
    Ok(CompiledC { _dir: dir, binary })
}

fn run_supervisor(
    binary: &std::path::Path,
    output_dir: &std::path::Path,
    slot: Option<usize>,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let mut command = c_native_command(binary);
    command.arg(output_dir);
    if let Some(slot) = slot {
        command.arg(slot.to_string());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("native test supervisor could not start: {error}"))?;
    let run = crate::common::ProcessManager::wait_with_timeout(&mut child, timeout)
        .map_err(|error| format!("native test supervisor wait failed: {error}"))?;
    if run.timed_out {
        return Err(format!(
            "native test supervisor timed out after {}s",
            timeout.as_secs()
        ));
    }
    if !run.output.status.success() {
        return Err(format!(
            "native test supervisor exited {}: {}",
            run.output.status,
            String::from_utf8_lossy(&run.output.stderr).trim()
        ));
    }
    Ok(())
}

fn read_result(dir: &std::path::Path, slot: usize) -> std::result::Result<String, String> {
    let raw = fs_err::read_to_string(dir.join(format!("{slot}.status")))
        .map_err(|error| format!("native test produced no status: {error}"))?
        .trim()
        .parse::<i32>()
        .map_err(|error| format!("invalid native test status: {error}"))?;
    let status = std::process::ExitStatus::from_raw(raw);
    if status.signal() == Some(libc::SIGALRM) {
        return Err(format!(
            "native test timed out after {}s",
            C_NATIVE_TEST_TIMEOUT.as_secs()
        ));
    }
    let stderr =
        fs_err::read(dir.join(format!("{slot}.err"))).map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!(
            "native test exited {status}: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    fs_err::read(dir.join(format!("{slot}.out")))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| error.to_string())
}
