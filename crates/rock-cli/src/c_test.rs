use crate::Result;
use crate::bone_backend::{categorize, short_path};
use crate::test_protocol::{FLOAT_REL_TOL, compare_value, parse_skull_markers, reason_histogram};
use colored::Colorize;
use skull_mir::mir::FuncRef;
use std::process::Command;
use std::time::Instant;

enum CUnitOutcome {
    Ran {
        passed: usize,
        fails: Vec<String>,
        skips: Vec<(String, String)>,
    },
    ExpectFailPass {
        tests: usize,
    },
    UnitFail {
        reason: String,
        rendered: String,
    },
}

pub struct CSuiteSummary {
    pub files: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
    pub unit_failures: usize,
    pub skip_reasons: Vec<(String, usize)>,
}

struct CheckedFile {
    source: String,
    ws: bone_semantic::inference::Workspace,
    user_mid: bone_semantic::prelude::ModuleId,
}

struct FileCheckError {
    reason: String,
    rendered: String,
    expected_failure_tests: Option<usize>,
}

struct CompiledC {
    _dir: tempfile::TempDir,
    binary: std::path::PathBuf,
}

pub fn c_test(paths: &[String]) -> Result<()> {
    let probe = Command::new("clang").arg("--version").output();
    if !matches!(probe, Ok(ref output) if output.status.success()) {
        anyhow::bail!("C backend requires `clang` on PATH");
    }

    let start = Instant::now();
    let summary = c_test_run(paths, true)?;
    println!(
        "\nc backend: {}/{} @tests passed ({} failed, {} skipped) across {} files ({:.2}s)",
        summary.tests_passed.to_string().green(),
        summary.tests_passed + summary.tests_failed + summary.tests_skipped,
        if summary.tests_failed == 0 {
            summary.tests_failed.to_string().green()
        } else {
            summary.tests_failed.to_string().red()
        },
        summary.tests_skipped.to_string().yellow(),
        summary.files,
        start.elapsed().as_secs_f64(),
    );
    if summary.unit_failures > 0 {
        println!(
            "  {} unit(s) failed to build/run (see FAILED lines above)",
            summary.unit_failures.to_string().red()
        );
    }
    if !summary.skip_reasons.is_empty() {
        println!("\n{}", "=== skip reasons (c) ===".bold());
        for (reason, count) in &summary.skip_reasons {
            println!("  {:>5}  {}", count.to_string().yellow(), reason);
        }
    }

    let ablation_fatals = crate::ablation_report::report("rock test --backend c");
    if summary.tests_failed > 0 || summary.unit_failures > 0 || ablation_fatals {
        std::process::exit(1);
    }
    Ok(())
}

pub fn c_test_run(paths: &[String], print: bool) -> Result<CSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;
    let mut summary = CSuiteSummary {
        files: 0,
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        unit_failures: 0,
        skip_reasons: Vec::new(),
    };
    let mut all_skip_reasons = Vec::new();

    for file in &files {
        record_unit(
            &mut summary,
            &mut all_skip_reasons,
            short_path(file),
            build_file(file),
            print,
        );
    }
    for dir in &package_dirs {
        record_unit(
            &mut summary,
            &mut all_skip_reasons,
            short_path(dir),
            build_package(dir),
            print,
        );
    }
    summary.skip_reasons = reason_histogram(all_skip_reasons.iter().map(String::as_str));
    Ok(summary)
}

fn record_unit(
    summary: &mut CSuiteSummary,
    all_skip_reasons: &mut Vec<String>,
    label: String,
    outcome: CUnitOutcome,
    print: bool,
) {
    summary.files += 1;
    match outcome {
        CUnitOutcome::Ran {
            passed,
            fails,
            skips,
        } => {
            summary.tests_passed += passed;
            summary.tests_failed += fails.len();
            summary.tests_skipped += skips.len();
            all_skip_reasons.extend(skips.iter().map(|(_, reason)| reason.clone()));
            if print && passed + fails.len() + skips.len() > 0 {
                let status = if fails.is_empty() {
                    "ok".green()
                } else {
                    "FAILED".red()
                };
                println!(
                    "{:.<60} {}  ({} pass, {} fail, {} skip)",
                    label,
                    status,
                    passed,
                    fails.len(),
                    skips.len()
                );
                for detail in &fails {
                    println!("  {} {}", "FAIL".red(), detail);
                }
            }
        }
        CUnitOutcome::ExpectFailPass { tests } => {
            summary.tests_passed += tests;
            if print {
                println!(
                    "{:.<60} {}  ({} pass, 0 fail, 0 skip)",
                    label,
                    "ok".green(),
                    tests
                );
            }
        }
        CUnitOutcome::UnitFail { reason, rendered } => {
            summary.unit_failures += 1;
            if print {
                println!("{:.<60} {}  {}", label, "FAILED".red(), reason.dimmed());
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    println!("{rendered}");
                }
            }
        }
    }
}

fn build_file(file: &str) -> CUnitOutcome {
    let checked = match check_file(file) {
        Ok(checked) => checked,
        Err(error) => {
            if let Some(tests) = error.expected_failure_tests {
                return CUnitOutcome::ExpectFailPass { tests };
            }
            return CUnitOutcome::UnitFail {
                reason: error.reason,
                rendered: error.rendered,
            };
        }
    };
    let tests = match skull::module_tests_with_ids(&checked.ws, checked.user_mid) {
        Ok(tests) => tests
            .into_iter()
            .map(|(func, test)| {
                (
                    FuncRef {
                        module: checked.user_mid,
                        func,
                    },
                    test,
                )
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return CUnitOutcome::UnitFail {
                reason: categorize(&error.message),
                rendered: crate::skull_backend::render_file_errs(file, &checked.source, &[error]),
            };
        }
    };
    run_tests(&checked.ws, &tests)
}

fn check_file(file: &str) -> std::result::Result<CheckedFile, FileCheckError> {
    let source = fs_err::read_to_string(file).map_err(|error| FileCheckError {
        reason: error.to_string(),
        rendered: error.to_string(),
        expected_failure_tests: None,
    })?;
    let user = skull_parser::parse_module(&source, ustr::Ustr::from(file)).map_err(|error| {
        FileCheckError {
            reason: categorize(&error.message),
            rendered: crate::skull_backend::render_file_errs(file, &source, &[error]),
            expected_failure_tests: crate::skull_backend::all_tests_expect_compile_failure(
                file, &source,
            ),
        }
    })?;
    let user_file = user.file;
    let mut ws =
        crate::bone_backend::check_with_stdlib([user]).map_err(|errors| FileCheckError {
            reason: categorize(&errors[0].message),
            rendered: crate::skull_backend::render_file_errs(file, &source, &errors),
            expected_failure_tests: crate::skull_backend::all_tests_expect_compile_failure(
                file, &source,
            ),
        })?;
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(file)).map_err(|error| {
        FileCheckError {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {file}: {error:#}"),
            expected_failure_tests: None,
        }
    })?;
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    Ok(CheckedFile {
        source,
        ws,
        user_mid,
    })
}

fn build_package(dir: &str) -> CUnitOutcome {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(sources) => sources,
        Err(error) => {
            return CUnitOutcome::UnitFail {
                reason: "cannot load project".to_string(),
                rendered: error.to_string(),
            };
        }
    };
    let (mut modules, _) = match crate::skull_backend::skull_assemble_project(&sources) {
        Ok(result) => result,
        Err(errors) => {
            return CUnitOutcome::UnitFail {
                reason: categorize(&errors[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errors),
            };
        }
    };
    let mut ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errors) => {
            return CUnitOutcome::UnitFail {
                reason: categorize(&errors[0].message),
                rendered: crate::skull_backend::render_project_errs(&sources, &errors),
            };
        }
    };
    if let Err(error) = crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir)) {
        return CUnitOutcome::UnitFail {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {dir}: {error:#}"),
        };
    }
    let tests = match crate::skull_backend::collect_package_test_fns(&ws, dir) {
        Ok(tests) => tests,
        Err(error) => {
            return CUnitOutcome::UnitFail {
                reason: categorize(&error.message),
                rendered: error.message,
            };
        }
    };
    run_tests(&ws, &tests)
}

fn run_tests(
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
) -> CUnitOutcome {
    let mut passed = 0;
    let mut fails = Vec::new();
    let mut skips = Vec::new();
    for (func, test) in tests {
        let Some(expect) = &test.returns else {
            skips.push((
                test.name.clone(),
                "C backend currently runs value tests only".to_string(),
            ));
            continue;
        };
        let emitted = match skull_codegen_c::emit_value_test(ws, *func, &test.name) {
            Ok(emitted) => emitted,
            Err(error) => {
                skips.push((test.name.clone(), error.message));
                continue;
            }
        };
        match compile_and_run(&test.name, &emitted.source) {
            Ok(value) => match compare_value(expect, &value, FLOAT_REL_TOL) {
                Ok(()) => passed += 1,
                Err(detail) => fails.push(format!("{}: {detail}", test.name)),
            },
            Err(detail) => fails.push(format!("{}: {detail}", test.name)),
        }
    }
    CUnitOutcome::Ran {
        passed,
        fails,
        skips,
    }
}

fn compile_and_run(test_name: &str, source: &str) -> std::result::Result<String, String> {
    let compiled = compile_source(source)?;
    let run = Command::new(&compiled.binary)
        .output()
        .map_err(|error| format!("native test could not start: {error}"))?;
    if !run.status.success() {
        return Err(format!(
            "native test exited {}: {}",
            run.status,
            String::from_utf8_lossy(&run.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&run.stdout);
    let markers = parse_skull_markers(&stdout).ok_or_else(|| {
        format!(
            "native test produced no result marker: stdout=<{}>",
            stdout.trim()
        )
    })?;
    if markers.passed != 0 || !markers.fails.is_empty() || markers.values.len() != 1 {
        return Err(format!(
            "native test produced an invalid result marker: <{}>",
            stdout.trim()
        ));
    }
    let (name, value) = &markers.values[0];
    if name != test_name {
        return Err(format!(
            "native result named `{name}`, expected `{test_name}`"
        ));
    }
    Ok(value.clone())
}

fn compile_source(source: &str) -> std::result::Result<CompiledC, String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let c_path = dir.path().join("test.c");
    let binary = dir.path().join("test-bin");
    fs_err::write(&c_path, source).map_err(|error| error.to_string())?;
    clang_compile(source, &c_path, &binary)?;
    Ok(CompiledC { _dir: dir, binary })
}

fn clang_compile(
    source: &str,
    c_path: &std::path::Path,
    binary: &std::path::Path,
) -> std::result::Result<(), String> {
    invalidate_artifact(binary)
        .map_err(|error| format!("could not invalidate {}: {error}", binary.display()))?;
    let compile = Command::new("clang")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(c_path)
        .arg("-o")
        .arg(binary)
        .output()
        .map_err(|error| format!("could not run clang: {error}"))?;
    if !compile.status.success() {
        return Err(format!(
            "generated C did not compile:\n{}\n--- generated C ---\n{}",
            String::from_utf8_lossy(&compile.stderr),
            source
        ));
    }
    if !binary.is_file() {
        return Err(format!(
            "clang succeeded without creating {}",
            binary.display()
        ));
    }
    Ok(())
}

fn emit_main_program(path: &str) -> Result<skull_codegen_c::CProgram> {
    if std::path::Path::new(path).is_dir() {
        anyhow::bail!("C backend currently accepts one .rock file, not a project directory");
    }
    let checked = check_file(path).map_err(|error| anyhow::anyhow!(error.rendered))?;
    let mut mains = checked
        .ws
        .module(checked.user_mid)
        .functions
        .iter()
        .filter(|(_, function)| {
            function.name.as_str() == "main" && function.param_decls.is_empty()
        });
    let Some((main, _)) = mains.next() else {
        anyhow::bail!("C backend requires a nullary `main` in the entry file");
    };
    if mains.next().is_some() {
        anyhow::bail!("C backend found more than one nullary `main` in the entry file");
    }
    let main = FuncRef {
        module: checked.user_mid,
        func: main,
    };
    skull_codegen_c::emit_program(&checked.ws, main).map_err(|error| {
        anyhow::anyhow!(crate::skull_backend::render_file_errs(
            path,
            &checked.source,
            &[error]
        ))
    })
}

fn c_artifact_paths(
    path: &str,
    output: Option<String>,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let binary = match output {
        Some(output) => std::path::PathBuf::from(output),
        None => {
            let stem = std::path::Path::new(path)
                .file_stem()
                .ok_or_else(|| anyhow::anyhow!("C backend input has no file name: {path}"))?;
            std::path::Path::new("tmp").join(stem)
        }
    };
    let mut c_name = binary.as_os_str().to_os_string();
    c_name.push(".c");
    Ok((std::path::PathBuf::from(c_name), binary))
}

fn reject_input_alias(path: &str, artifact: &std::path::Path) -> Result<()> {
    let input = std::path::Path::new(path);
    let aliases = input == artifact
        || match (fs_err::canonicalize(input), fs_err::canonicalize(artifact)) {
            (Ok(input), Ok(artifact)) => input == artifact,
            _ => false,
        };
    if aliases {
        anyhow::bail!("C backend output must not refer to the input file");
    }
    Ok(())
}

fn invalidate_artifact(path: &std::path::Path) -> std::io::Result<()> {
    match fs_err::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn c_compile(path: &str, output: Option<String>) -> Result<()> {
    let (c_path, binary) = c_artifact_paths(path, output)?;
    reject_input_alias(path, &c_path)?;
    reject_input_alias(path, &binary)?;
    invalidate_artifact(&c_path)?;
    invalidate_artifact(&binary)?;
    let emitted = emit_main_program(path)?;
    for artifact in [&c_path, &binary] {
        if let Some(parent) = artifact.parent()
            && !parent.as_os_str().is_empty()
        {
            fs_err::create_dir_all(parent)?;
        }
    }
    rock_host::project::write_atomic(&c_path, &emitted.source)?;
    clang_compile(&emitted.source, &c_path, &binary).map_err(anyhow::Error::msg)?;
    log::info!(target: "C", "{} -> {}, {}", path, c_path.display(), binary.display());
    Ok(())
}

pub fn c_run(path: &str) -> Result<()> {
    let emitted = emit_main_program(path)?;
    let compiled = compile_source(&emitted.source).map_err(anyhow::Error::msg)?;
    let status = Command::new(&compiled.binary).status()?;
    if !status.success() {
        anyhow::bail!("native C program exited {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn c_backend_smoke_runs_five_native_value_tests() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/c_backend_smoke.rock")
            .to_string_lossy()
            .into_owned();
        let summary = super::c_test_run(&[path], false).expect("run C backend smoke");
        assert_eq!(summary.files, 1);
        assert_eq!(summary.tests_passed, 5);
        assert_eq!(summary.tests_failed, 0);
        assert_eq!(summary.tests_skipped, 0);
        assert_eq!(summary.unit_failures, 0);
    }
}
