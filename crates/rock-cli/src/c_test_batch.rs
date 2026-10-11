use super::*;
use std::os::unix::process::{CommandExt, ExitStatusExt};

struct PreparedTest {
    source: String,
    link: CLinkPlan,
}

pub(super) fn run(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
    stone_dir: Option<&std::path::Path>,
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
            run_file(path, ws, tests, indices, engine_builds, stone_dir)
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
    stone_dir: Option<&std::path::Path>,
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
    let batch_build = compile_batch(&prepared);
    for (index, error) in batch_build.failures {
        results.push((index, CTestRun::Ran(Err(error))));
    }
    match batch_build.compiled {
        Ok(Some(compiled)) => {
            let output_dir = compiled._dir.path();
            let timeout = C_NATIVE_TEST_TIMEOUT * batch_build.indices.len() as u32 + Duration::from_secs(5);
            let batch = run_supervisor(&compiled.binary, output_dir, None, timeout, stone_dir);
            if let Err(error) = &batch {
                log::warn!(target: "TEST", "C test supervisor failed: {error}");
            }
            for (slot, index) in batch_build.indices.iter().enumerate() {
                // A crashed supervisor may leave later tests unstarted. Reuse the
                // binary in single-test mode, retaining every completed result.
                if !output_dir.join(format!("{slot}.status")).exists() {
                    log::warn!(target: "TEST", "C test supervisor failed; retrying {}", tests[*index].1.name);
                    if let Err(error) = run_supervisor(
                        &compiled.binary,
                        output_dir,
                        Some(slot),
                        C_NATIVE_TEST_TIMEOUT + Duration::from_secs(5),
                        stone_dir,
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
        Ok(None) => {}
        Err(error) => {
            let error = format!("C test batch infrastructure failed: {error}");
            log::error!(target: "TEST", "{}",
                error.split("\n--- generated C ---").next().unwrap_or(&error));
            for index in batch_build.indices {
                results.push((index, CTestRun::Ran(Err(error.clone()))));
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

struct CompiledBatch {
    compiled: std::result::Result<Option<CompiledC>, String>,
    indices: Vec<usize>,
    failures: Vec<(usize, String)>,
}

fn compile_batch(tests: &[(usize, PreparedTest)]) -> CompiledBatch {
    let mut batch = CompiledBatch {
        compiled: Ok(None),
        indices: tests.iter().map(|(index, _)| *index).collect(),
        failures: Vec::new(),
    };
    batch.compiled = (|| {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let binary = dir.path().join("test-bin");
        let objects = tests.par_iter().enumerate().map(|(slot, (_, test))| {
            let source = format!("#define main rock_c_test_{slot}\n{}", test.source);
            let path = dir.path().join(format!("test-{slot}.c"));
            let object = dir.path().join(format!("test-{slot}.o"));
            fs_err::write(&path, &source)
                .map_err(|error| CObjectCompileError::Infrastructure(error.to_string()))?;
            clang_compile_object(&source, &path, &object, &test.link.include_dirs)?;
            Ok((slot, object, source))
        }).collect::<Vec<std::result::Result<_, CObjectCompileError>>>();
        batch.indices.clear();
        let mut sources = Vec::new();
        let mut entries = Vec::new();
        let mut link = CLinkPlan::default();
        let mut infrastructure = Vec::new();
        for ((index, test), object) in tests.iter().zip(objects) {
            match object {
                Ok((slot, object, source)) => {
                    batch.indices.push(*index);
                    entries.push(slot);
                    sources.push((object, source));
                    extend_unique(&mut link.include_dirs, &test.link.include_dirs);
                    extend_unique(&mut link.library_dirs, &test.link.library_dirs);
                    extend_unique(&mut link.libraries, &test.link.libraries);
                    extend_unique(&mut link.rpaths, &test.link.rpaths);
                }
                Err(CObjectCompileError::Test(error)) => batch.failures.push((*index, error)),
                Err(CObjectCompileError::Infrastructure(error)) => {
                    batch.indices.push(*index);
                    infrastructure.push(error);
                }
            }
        }
        if !infrastructure.is_empty() {
            return Err(infrastructure.join("\n"));
        }
        if sources.is_empty() {
            return Ok(None);
        }
        let mut supervisor = format!(
            "#define ROCK_TEST_COUNT {}\n#define ROCK_TEST_TIMEOUT_SECONDS {}\n",
            entries.len(), C_NATIVE_TEST_TIMEOUT.as_secs()
        );
        for slot in &entries {
            supervisor.push_str(&format!("extern int rock_c_test_{slot}(void);\n"));
        }
        supervisor.push_str("static int (*const rock_tests[])(void) = {\n");
        for slot in entries {
            supervisor.push_str(&format!("rock_c_test_{slot},\n"));
        }
        supervisor.push_str("};\n");
        supervisor.push_str(include_str!("c_test_supervisor.c"));
        let path = dir.path().join("supervisor.c");
        fs_err::write(&path, &supervisor).map_err(|error| error.to_string())?;
        sources.push((path, supervisor));
        clang_compile_sources(&sources, &binary, &link)?;
        Ok(Some(CompiledC { _dir: dir, binary }))
    })();
    batch
}

struct SupervisorProcess(std::process::Child);

impl Drop for SupervisorProcess {
    fn drop(&mut self) {
        // Tests may leave descendants running after their direct child exits.
        let group = self.0.id() as libc::pid_t;
        if unsafe { libc::kill(-group, libc::SIGKILL) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                log::error!(target: "TEST", "C supervisor process group cleanup failed: {error}");
            }
        }
        if let Err(error) = self.0.wait() {
            log::error!(target: "TEST", "C supervisor reap failed: {error}");
        }
    }
}

fn spawn_supervisor(
    binary: &std::path::Path,
    output_dir: &std::path::Path,
    slot: Option<usize>,
    stone_dir: Option<&std::path::Path>,
) -> std::result::Result<(SupervisorProcess, tempfile::TempDir), String> {
    let (mut command, scratch) = c_native_test_command(binary, stone_dir)?;
    command.process_group(0);
    command.arg(output_dir);
    if let Some(slot) = slot {
        command.arg(slot.to_string());
    }
    // Files let group cleanup precede reading diagnostics even if a descendant
    // inherits the supervisor's output descriptors.
    let stdout = fs_err::File::create(output_dir.join("supervisor.out"))
        .map_err(|error| format!("native test supervisor output failed: {error}"))?;
    let stderr_path = output_dir.join("supervisor.err");
    let stderr = fs_err::File::create(&stderr_path)
        .map_err(|error| format!("native test supervisor output failed: {error}"))?;
    let child = command
        .stdout(Stdio::from(stdout.into_parts().0))
        .stderr(Stdio::from(stderr.into_parts().0))
        .spawn()
        .map_err(|error| format!("native test supervisor could not start: {error}"))?;
    Ok((SupervisorProcess(child), scratch))
}

fn run_supervisor(
    binary: &std::path::Path,
    output_dir: &std::path::Path,
    slot: Option<usize>,
    timeout: Duration,
    stone_dir: Option<&std::path::Path>,
) -> std::result::Result<(), String> {
    let (mut child, _scratch) = spawn_supervisor(binary, output_dir, slot, stone_dir)?;
    let stderr_path = output_dir.join("supervisor.err");
    let run = crate::common::ProcessManager::wait_with_timeout(&mut child.0, timeout);
    drop(child);
    let run = run
        .map_err(|error| format!("native test supervisor wait failed: {error}"))?;
    let stderr = fs_err::read(&stderr_path)
        .map_err(|error| format!("native test supervisor diagnostic read failed: {error}"))?;
    if run.timed_out {
        return Err(format!(
            "native test supervisor timed out after {}s: {}",
            timeout.as_secs(),
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    if !run.output.status.success() {
        return Err(format!(
            "native test supervisor exited {}: {}",
            run.output.status,
            String::from_utf8_lossy(&stderr).trim()
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
    if raw == -1 || status.signal() == Some(libc::SIGALRM) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn supervisor_fixture(body: &str, timeout_seconds: u64) -> CompiledC {
        let source = format!(
            "#define _POSIX_C_SOURCE 200809L\n#define ROCK_TEST_COUNT 2\n#define ROCK_TEST_TIMEOUT_SECONDS {timeout_seconds}\n\
             #include <signal.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\n\
             static int wedged(void) {{ printf(\"%ld\\n\", (long)getpid()); fflush(stdout); {body} return 0; }}\n\
             static int survivor(void) {{ puts(\"finished\"); return 0; }}\n\
             static int (*const rock_tests[])(void) = {{wedged, survivor}};\n{}",
            include_str!("c_test_supervisor.c")
        );
        compile_source(&source).unwrap_or_else(|error| panic!("{error}"))
    }

    fn child_pid(dir: &std::path::Path) -> libc::pid_t {
        let start = Instant::now();
        loop {
            match fs_err::read_to_string(dir.join("0.out")) {
                Ok(output) => {
                    if let Some((line, _)) = output.split_once('\n') {
                        return line.parse().expect("native child pid");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("native child output: {error}"),
            }
            // Hang guards, not timing: a loaded host may be slow to start or reap.
            assert!(start.elapsed() < Duration::from_secs(120), "native child did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn child_is_gone(pid: libc::pid_t) -> bool {
        let start = Instant::now();
        loop {
            if unsafe { libc::kill(pid, 0) } < 0 {
                assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                return true;
            }
            if start.elapsed() >= Duration::from_secs(120) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Keep the RED run from leaving its reproducer alive.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        false
    }

    #[test]
    fn supervisor_enforces_deadline_when_child_ignores_alarm() {
        let compiled = supervisor_fixture("signal(SIGALRM, SIG_IGN); for (;;) {}", 1);
        let dir = compiled._dir.path();
        let start = Instant::now();
        // This is a hang guard; scheduling delay is not deadline behavior.
        let run = run_supervisor(&compiled.binary, dir, None, Duration::from_secs(60), None);
        let gone = child_is_gone(child_pid(dir));
        assert!(run.is_ok(), "{run:?}");
        assert!(gone, "timed-out child survived");
        assert!(start.elapsed() >= Duration::from_secs(1), "child was killed before its deadline");
        assert!(read_result(dir, 0).unwrap_err().contains("timed out"));
        assert_eq!(read_result(dir, 1).unwrap().trim(), "finished");
    }

    #[test]
    fn supervisor_death_cleans_up_running_child() {
        let compiled = supervisor_fixture("signal(SIGALRM, SIG_IGN); kill(getppid(), SIGKILL); for (;;) {}", 1);
        let dir = compiled._dir.path();
        let run = run_supervisor(&compiled.binary, dir, None, test_utils::HANG_BACKSTOP, None);
        let gone = child_is_gone(child_pid(dir));
        assert!(run.unwrap_err().contains("supervisor exited"));
        assert!(gone, "child outlived dead supervisor");
    }

    #[test]
    fn supervisor_watchdog_reports_timeout() {
        // Keep the internal deadline beyond the watchdog despite scheduling delay.
        let compiled = supervisor_fixture("signal(SIGALRM, SIG_IGN); for (;;) {}", 60);
        let dir = compiled._dir.path();
        let run = run_supervisor(&compiled.binary, dir, None, Duration::from_millis(200), None);
        assert!(run.unwrap_err().contains("supervisor timed out"));
    }

    #[test]
    fn supervisor_drop_reaps_running_child() {
        let compiled = supervisor_fixture("signal(SIGALRM, SIG_IGN); for (;;) {}", 60);
        let dir = compiled._dir.path();
        let (mut supervisor, _scratch) = spawn_supervisor(&compiled.binary, dir, None, None).unwrap();
        let pid = child_pid(dir);
        supervisor.0.kill().unwrap();
        supervisor.0.wait().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "child must still be running before Drop");
        drop(supervisor);
        assert!(child_is_gone(pid), "child outlived supervisor Drop");
    }

    #[test]
    fn supervisor_scratch_is_writable_unique_and_removed_after_success_and_abort() {
        let mut roots = std::collections::HashSet::new();
        for fail in [false, true] {
            let body = format!(r#"
                const char *root = getenv("ROCK_TEST_TMP");
                if (!root) {{ puts("missing scratch root"); return 1; }}
                char path[4096];
                int length = snprintf(path, sizeof(path), "%s/probe", root);
                if (length < 0 || (size_t)length >= sizeof(path)) return 2;
                FILE *probe = fopen(path, "w");
                if (!probe) return 3;
                if (fputs("scratch", probe) < 0 || fclose(probe)) return 4;
                puts(root);
                fflush(stdout);
                {}
            "#, if fail { "abort();" } else { "" });
            let compiled = supervisor_fixture(&body, test_utils::HANG_BACKSTOP.as_secs());
            let dir = compiled._dir.path();
            let run = run_supervisor(&compiled.binary, dir, None, test_utils::HANG_BACKSTOP, None);
            assert!(run.is_ok(), "{run:?}");
            let output = fs_err::read_to_string(dir.join("0.out")).unwrap();
            let root = output.lines().nth(1).expect("native scratch path");
            assert!(std::path::Path::new(root).is_absolute(), "{output}");
            assert!(roots.insert(root.to_owned()), "scratch roots were reused");
            assert!(!std::path::Path::new(root).exists(), "scratch survived: {root}");
            if fail {
                let raw: i32 = fs_err::read_to_string(dir.join("0.status")).unwrap().trim().parse().unwrap();
                assert_eq!(std::process::ExitStatus::from_raw(raw).signal(), Some(libc::SIGABRT));
                assert!(read_result(dir, 0).is_err());
            } else {
                assert!(read_result(dir, 0).is_ok());
            }
            assert_eq!(read_result(dir, 1).unwrap().trim(), "finished");
        }
    }
}
