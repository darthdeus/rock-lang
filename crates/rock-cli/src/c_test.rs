use crate::Result;
use crate::bone_backend::{categorize, short_path};
use crate::test_protocol::{FLOAT_REL_TOL, compare_value, parse_skull_markers, reason_histogram};
use colored::Colorize;
use skull_mir::mir::FuncRef;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const C_NATIVE_TEST_TIMEOUT: Duration = Duration::from_secs(15);
const C_NATIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

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

struct CheckedProject {
    sources: crate::SourceFiles,
    ws: bone_semantic::inference::Workspace,
    entry_mid: bone_semantic::prelude::ModuleId,
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

#[derive(Debug, Default)]
struct CLinkPlan {
    include_dirs: Vec<std::path::PathBuf>,
    library_dirs: Vec<std::path::PathBuf>,
    libraries: Vec<String>,
    rpaths: Vec<std::path::PathBuf>,
}

struct CForeignProvider {
    library: &'static str,
    header: &'static str,
    include_dir: &'static str,
    build: CForeignBuild,
}

#[derive(Clone, Copy)]
enum CForeignBuild {
    Stone,
}

fn c_foreign_provider(library: &str) -> Option<CForeignProvider> {
    match library {
        "stone" => Some(CForeignProvider {
            library: "stone",
            header: "stone.h",
            include_dir: "stone-rs/include",
            build: CForeignBuild::Stone,
        }),
        _ => None,
    }
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
    run_tests(file, &checked.ws, &tests)
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
    let checked = match check_project(dir, false) {
        Ok(checked) => checked,
        Err(error) => {
            return CUnitOutcome::UnitFail {
                reason: error.reason,
                rendered: error.rendered,
            };
        }
    };
    let tests = match crate::skull_backend::collect_package_test_fns(&checked.ws, dir) {
        Ok(tests) => tests,
        Err(error) => {
            return CUnitOutcome::UnitFail {
                reason: categorize(&error.message),
                rendered: error.message,
            };
        }
    };
    run_tests(dir, &checked.ws, &tests)
}

fn check_project(
    dir: &str,
    production_only: bool,
) -> std::result::Result<CheckedProject, FileCheckError> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources =
        crate::unified_compile::discover_sources(&spec).map_err(|error| FileCheckError {
            reason: "cannot load project".to_string(),
            rendered: error.to_string(),
            expected_failure_tests: None,
        })?;
    let game_project = production_only
        && rock_host::project::is_game_project(&std::path::PathBuf::from(dir)).map_err(
            |error| FileCheckError {
                reason: "cannot load project".to_string(),
                rendered: error.to_string(),
                expected_failure_tests: None,
            },
        )?;
    let sources = if game_project {
        sources.production_only()
    } else {
        sources
    };
    let (mut modules, entry_file) = crate::skull_backend::skull_assemble_project(&sources)
        .map_err(|errors| FileCheckError {
            reason: categorize(&errors[0].message),
            rendered: crate::skull_backend::render_project_errs(&sources, &errors),
            expected_failure_tests: None,
        })?;
    let mut ws =
        crate::bone_backend::check_modules(&mut modules).map_err(|errors| FileCheckError {
            reason: categorize(&errors[0].message),
            rendered: crate::skull_backend::render_project_errs(&sources, &errors),
            expected_failure_tests: None,
        })?;
    crate::comptime::stamp_workspace(&mut ws, std::path::Path::new(dir)).map_err(|error| {
        FileCheckError {
            reason: "build var resolution failed".to_string(),
            rendered: format!("build var resolution failed for {dir}: {error:#}"),
            expected_failure_tests: None,
        }
    })?;
    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);
    Ok(CheckedProject {
        sources,
        ws,
        entry_mid,
    })
}

fn run_tests(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
) -> CUnitOutcome {
    let mut passed = 0;
    let mut fails = Vec::new();
    let mut skips = Vec::new();
    for (func, test) in tests {
        let Some(expect) = &test.returns else {
            let emitted = match skull_codegen_c::emit_test(ws, *func) {
                Ok(emitted) => emitted,
                Err(error) => {
                    skips.push((test.name.clone(), error.message));
                    continue;
                }
            };
            match compile_and_run_program(path, &emitted) {
                Ok(stdout) => {
                    if let Some(expected) = &test.output {
                        let got = stdout.trim_end();
                        if got == expected {
                            passed += 1;
                        } else {
                            fails.push(format!(
                                "{}: output mismatch: expected <{:?}> got <{:?}>",
                                test.name, expected, got
                            ));
                        }
                    } else {
                        passed += 1;
                    }
                }
                Err(detail) => fails.push(format!("{}: {detail}", test.name)),
            }
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

fn compile_and_run_program(
    path: &str,
    emitted: &skull_codegen_c::CProgram,
) -> std::result::Result<String, String> {
    let link = c_link_plan(path, emitted).map_err(|error| error.to_string())?;
    let compiled = compile_source_with_link(&emitted.source, &link)?;
    let run = run_native_test(&compiled.binary, C_NATIVE_TEST_TIMEOUT)?;
    if !run.status.success() {
        return Err(format!(
            "native test exited {}: {}",
            run.status,
            String::from_utf8_lossy(&run.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).into_owned())
}

fn compile_and_run(test_name: &str, source: &str) -> std::result::Result<String, String> {
    let compiled = compile_source(source)?;
    let run = run_native_test(&compiled.binary, C_NATIVE_TEST_TIMEOUT)?;
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
    compile_source_with_link(source, &CLinkPlan::default())
}

fn compile_source_with_link(
    source: &str,
    link: &CLinkPlan,
) -> std::result::Result<CompiledC, String> {
    let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
    let c_path = dir.path().join("test.c");
    let binary = dir.path().join("test-bin");
    fs_err::write(&c_path, source).map_err(|error| error.to_string())?;
    clang_compile(source, &c_path, &binary, link)?;
    Ok(CompiledC { _dir: dir, binary })
}

fn clang_compile(
    source: &str,
    c_path: &std::path::Path,
    binary: &std::path::Path,
    link: &CLinkPlan,
) -> std::result::Result<(), String> {
    invalidate_artifact(binary)
        .map_err(|error| format!("could not invalidate {}: {error}", binary.display()))?;
    let mut command = Command::new("clang");
    command.args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"]);
    for dir in &link.include_dirs {
        command.arg("-I").arg(dir);
    }
    command.arg(c_path);
    for dir in &link.library_dirs {
        command.arg("-L").arg(dir);
    }
    for library in &link.libraries {
        command.arg(format!("-l{library}"));
    }
    for rpath in &link.rpaths {
        command.arg(format!("-Wl,-rpath,{}", rpath.display()));
    }
    let compile = command
        .arg("-lm")
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

fn c_link_plan(path: &str, emitted: &skull_codegen_c::CProgram) -> Result<CLinkPlan> {
    let mut providers = Vec::new();
    for library in &emitted.foreign_libraries {
        let provider = c_foreign_provider(library).ok_or_else(|| {
            anyhow::anyhow!(
                "C backend has no native header/link provider for foreign library `{library}`"
            )
        })?;
        if !emitted
            .foreign_headers
            .iter()
            .any(|header| header == provider.header)
        {
            anyhow::bail!(
                "C backend foreign metadata for `{library}` is missing `{}`",
                provider.header
            );
        }
        providers.push(provider);
    }
    if providers.is_empty() {
        return Ok(CLinkPlan::default());
    }
    let mut link = CLinkPlan::default();
    for provider in &providers {
        link.libraries.push(provider.library.to_string());
    }

    let build_stone = providers
        .iter()
        .any(|provider| matches!(provider.build, CForeignBuild::Stone));

    let cwd = std::env::current_dir()?;
    let root = rock_host::project::find_workspace_root(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "C backend cannot locate the Stone workspace from {}",
            cwd.display()
        )
    })?;
    if cwd != root {
        anyhow::bail!(
            "C backend native Stone builds currently run from the workspace root {}; current directory is {}",
            root.display(),
            cwd.display()
        );
    }
    if build_stone
        && !std::env::var("ROCK_SKIP_ENGINE_BUILD").is_ok_and(|value| value == "1")
    {
        let game_path = if std::path::Path::new(path).is_dir() {
            path
        } else {
            "."
        };
        rock_host::engine_build::build_engine_libs(game_path)?;
    }

    for provider in &providers {
        link.include_dirs.push(root.join(provider.include_dir));
    }
    let library = if rock_host::project::is_release_layout() {
        root.join("lib")
    } else {
        root.join("tmp")
    };
    link.library_dirs.push(library.clone());
    link.rpaths.push(library);
    Ok(link)
}

fn emit_main_program(path: &str) -> Result<skull_codegen_c::CProgram> {
    fn main_ref(
        ws: &bone_semantic::inference::Workspace,
        entry_mid: bone_semantic::prelude::ModuleId,
    ) -> Result<FuncRef> {
        let mut mains = ws
            .module(entry_mid)
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
        Ok(FuncRef {
            module: entry_mid,
            func: main,
        })
    }

    if std::path::Path::new(path).is_dir() {
        let checked = check_project(path, true).map_err(|error| anyhow::anyhow!(error.rendered))?;
        let main = main_ref(&checked.ws, checked.entry_mid)?;
        skull_codegen_c::emit_program(&checked.ws, main).map_err(|error| {
            anyhow::anyhow!(crate::skull_backend::render_project_err(
                &checked.sources,
                &error
            ))
        })
    } else {
        let checked = check_file(path).map_err(|error| anyhow::anyhow!(error.rendered))?;
        let main = main_ref(&checked.ws, checked.user_mid)?;
        skull_codegen_c::emit_program(&checked.ws, main).map_err(|error| {
            anyhow::anyhow!(crate::skull_backend::render_file_errs(
                path,
                &checked.source,
                &[error]
            ))
        })
    }
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

fn c_native_command(binary: &std::path::Path) -> Command {
    let mut command = Command::new(binary);
    command.env(
        rock_host::project::lib_path_var(),
        rock_host::project::lib_path_with_inherited(),
    );
    command
}

fn validate_c_frame_request(
    frames: usize,
    requested_renderer: Option<crate::RendererBackend>,
) -> Result<()> {
    if frames == 0 {
        anyhow::bail!("the C backend native frame runner requires --frames greater than zero");
    }
    if requested_renderer == Some(crate::RendererBackend::Wgpu) {
        anyhow::bail!(
            "the C backend native frame runner supports OpenGL only; `--renderer wgpu` is unsupported"
        );
    }
    Ok(())
}

fn c_native_frame_asset_environment(
    path: &str,
) -> Result<(std::path::PathBuf, std::ffi::OsString)> {
    let game_dir = fs_err::canonicalize(path)?;
    if !game_dir.is_dir() {
        anyhow::bail!(
            "C backend frame input is not a game directory: {}",
            game_dir.display()
        );
    }
    let config = rock_host::stick::StickConfig::find_from_dir(&game_dir)?
        .map(|(_, config)| config);
    let overlays = rock_host::release::asset_overlay_roots(&game_dir, config.as_ref())?;
    let overlay_env = std::env::join_paths(&overlays)
        .map_err(|error| anyhow::anyhow!("could not encode native asset overlays: {error}"))?;
    Ok((game_dir, overlay_env))
}

fn c_native_frame_command(
    binary: &std::path::Path,
    frames: usize,
    headless: bool,
    game_source_dir: &std::path::Path,
    asset_overlays: &std::ffi::OsStr,
) -> Command {
    let mut command = c_native_command(binary);
    command
        .env("FRAME_COUNT", frames.to_string())
        .env("STONE_NATIVE_FRAME_GATE", "1")
        .env("GAME_SOURCE_DIR", game_source_dir)
        .env(rock_host::release::ASSET_OVERLAYS_ENV, asset_overlays);
    if headless {
        command.env("STONE_HEADLESS", "1");
        crate::stone::apply_headless_audio_env(&mut command);
    } else {
        command.env_remove("STONE_HEADLESS");
    }
    command
}

fn run_native_test(
    binary: &std::path::Path,
    timeout: Duration,
) -> std::result::Result<std::process::Output, String> {
    let mut child = c_native_command(binary)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("native test could not start: {error}"))?;
    let run = crate::common::ProcessManager::wait_with_timeout(&mut child, timeout)
        .map_err(|error| format!("native test wait failed: {error}"))?;
    if run.timed_out {
        let elapsed = if timeout.subsec_nanos() == 0 {
            format!("{}s", timeout.as_secs())
        } else {
            format!("{}ms", timeout.as_millis())
        };
        return Err(format!("native test timed out after {elapsed}"));
    }
    Ok(run.output)
}

pub fn c_compile(path: &str, output: Option<String>) -> Result<()> {
    let (c_path, binary) = c_artifact_paths(path, output)?;
    reject_input_alias(path, &c_path)?;
    reject_input_alias(path, &binary)?;
    invalidate_artifact(&c_path)?;
    invalidate_artifact(&binary)?;
    let emitted = emit_main_program(path)?;
    let link = c_link_plan(path, &emitted)?;
    for artifact in [&c_path, &binary] {
        if let Some(parent) = artifact.parent()
            && !parent.as_os_str().is_empty()
        {
            fs_err::create_dir_all(parent)?;
        }
    }
    rock_host::project::write_atomic(&c_path, &emitted.source)?;
    clang_compile(&emitted.source, &c_path, &binary, &link).map_err(anyhow::Error::msg)?;
    log::info!(target: "C", "{} -> {}, {}", path, c_path.display(), binary.display());
    Ok(())
}

pub fn c_run(path: &str) -> Result<()> {
    let emitted = emit_main_program(path)?;
    let link = c_link_plan(path, &emitted)?;
    let dir = tempfile::tempdir()?;
    let c_path = dir.path().join("program.c");
    let binary = dir.path().join("program-bin");
    fs_err::write(&c_path, &emitted.source)?;
    clang_compile(&emitted.source, &c_path, &binary, &link).map_err(anyhow::Error::msg)?;
    let compiled = CompiledC { _dir: dir, binary };
    let status = c_native_command(&compiled.binary).status()?;
    if !status.success() {
        anyhow::bail!("native C program exited {status}");
    }
    Ok(())
}

pub fn c_frame(
    path: &str,
    frames: usize,
    headless: bool,
    requested_renderer: Option<crate::RendererBackend>,
) -> Result<()> {
    validate_c_frame_request(frames, requested_renderer)?;
    let _gl_renderer =
        rock_host::stick::install_renderer_override(Some(crate::RendererBackend::Gl));
    let (game_source_dir, asset_overlays) = c_native_frame_asset_environment(path)?;
    let emitted = emit_main_program(path)?;
    let link = c_link_plan(path, &emitted)?;
    let compiled = compile_source_with_link(&emitted.source, &link).map_err(anyhow::Error::msg)?;
    let mut child = c_native_frame_command(
        &compiled.binary,
        frames,
        headless,
        &game_source_dir,
        &asset_overlays,
    )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let run = crate::common::ProcessManager::wait_with_timeout(
        &mut child,
        C_NATIVE_FRAME_TIMEOUT,
    )?;
    if run.timed_out {
        anyhow::bail!(
            "native C frame run timed out after {}s",
            C_NATIVE_FRAME_TIMEOUT.as_secs()
        );
    }
    if !run.output.status.success() {
        let stderr = String::from_utf8_lossy(&run.output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&run.output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "no child diagnostics".to_owned()
        };
        anyhow::bail!(
            "native C frame run exited {}; {}",
            run.output.status,
            detail
        );
    }
    if frames == 1 {
        log::info!(target: "C", "One frame executed successfully");
    } else {
        log::info!(target: "C", "{} frames executed successfully", frames);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn c_link_plan_rejects_an_unregistered_foreign_provider() {
        let emitted = skull_codegen_c::CProgram {
            source: "#include <unknown.h>\n".to_string(),
            foreign_libraries: vec!["unknown".to_string()],
            foreign_headers: vec!["unknown.h".to_string()],
        };
        let error = super::c_link_plan("unused.rock", &emitted)
            .expect_err("unknown provider must reject");
        assert!(
            error.to_string().contains("no native header/link provider"),
            "{error}"
        );
    }

    #[test]
    fn stone_foreign_provider_uses_the_authoritative_header() {
        let provider = super::c_foreign_provider("stone").expect("Stone provider");
        assert_eq!(provider.header, "stone.h");
        assert_eq!(provider.include_dir, "stone-rs/include");
        assert!(super::c_foreign_provider("unknown").is_none());
    }

    #[test]
    fn c_backend_smoke_runs_ten_native_value_tests() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/c_backend_smoke.rock")
            .to_string_lossy()
            .into_owned();
        let summary = super::c_test_run(&[path], false).expect("run C backend smoke");
        assert_eq!(summary.files, 1);
        assert_eq!(summary.tests_passed, 10);
        assert_eq!(summary.tests_failed, 0);
        assert_eq!(summary.tests_skipped, 0);
        assert_eq!(summary.unit_failures, 0);
    }

    #[test]
    fn c_backend_smoke_runs_native_output_tests() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/c_backend_output_smoke.rock")
            .to_string_lossy()
            .into_owned();
        let summary = super::c_test_run(&[path], false).expect("run C backend output smoke");
        assert_eq!(summary.files, 1);
        assert_eq!(summary.tests_passed, 1);
        assert_eq!(summary.tests_failed, 0);
        assert_eq!(summary.tests_skipped, 0);
        assert_eq!(summary.unit_failures, 0);
    }

    #[test]
    fn c_native_commands_use_the_authoritative_library_path() {
        let command = super::c_native_command(std::path::Path::new("unused-native-program"));
        let actual = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new(rock_host::project::lib_path_var()))
            .and_then(|(_, value)| value);
        assert_eq!(
            actual,
            Some(std::ffi::OsStr::new(
                &rock_host::project::lib_path_with_inherited()
            ))
        );
    }

    #[test]
    fn native_test_timeout_kills_a_wedged_binary() {
        let compiled = super::compile_source("int main(void) { for (;;) {} }")
            .expect("wedged fixture must compile");
        let start = Instant::now();
        let error = super::run_native_test(&compiled.binary, Duration::from_millis(100))
            .expect_err("wedged native test must time out");
        assert!(error.contains("timed out after 100ms"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn native_frame_request_accepts_opengl_and_rejects_unbounded_or_wgpu() {
        assert!(super::validate_c_frame_request(1, None).is_ok());
        assert!(super::validate_c_frame_request(2, Some(crate::RendererBackend::Gl)).is_ok());
        let zero = super::validate_c_frame_request(0, None).expect_err("zero frames must reject");
        assert!(zero.to_string().contains("greater than zero"), "{zero}");
        let wgpu = super::validate_c_frame_request(
            1,
            Some(crate::RendererBackend::Wgpu),
        )
        .expect_err("WGPU must reject");
        assert!(wgpu.to_string().contains("OpenGL only"), "{wgpu}");
    }

    #[test]
    fn native_frame_command_sets_the_bounded_headless_gate() {
        let command = super::c_native_frame_command(
            std::path::Path::new("unused-native-program"),
            2,
            true,
            std::path::Path::new("/workspace/mag"),
            std::ffi::OsStr::new("/vault/mag"),
        );
        let env = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
        };
        assert_eq!(env("FRAME_COUNT").as_deref(), Some("2"));
        assert_eq!(env("STONE_NATIVE_FRAME_GATE").as_deref(), Some("1"));
        assert_eq!(env("STONE_HEADLESS").as_deref(), Some("1"));
        assert_eq!(env("GAME_SOURCE_DIR").as_deref(), Some("/workspace/mag"));
        assert_eq!(env("ROCK_ASSET_OVERLAYS").as_deref(), Some("/vault/mag"));
    }
}
