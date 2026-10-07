use crate::Result;
use crate::bone_backend::{categorize, short_path};
use crate::test_protocol::{FLOAT_REL_TOL, compare_value, parse_skull_markers, reason_histogram};
use colored::Colorize;
use skull_mir::mir::FuncRef;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "c_test_batch.rs"]
mod batch;

const C_NATIVE_TEST_TIMEOUT: Duration = Duration::from_secs(15);
const C_NATIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const C_TEST_DEFAULT_THREAD_CAP: usize = 16;

/// Thread count requested by `ROCK_NUM_THREADS`, else `RAYON_NUM_THREADS`.
/// It sizes both the compiler's global rayon pool and the C test pool.
pub fn requested_thread_count() -> Option<usize> {
    std::env::var("ROCK_NUM_THREADS")
        .ok()
        .or_else(|| std::env::var("RAYON_NUM_THREADS").ok())
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
}

// C test units are independent clang and test-process jobs that keep scaling
// past the compiler's global pool cap, so they run on their own wider pool.
fn test_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = requested_thread_count().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(C_TEST_DEFAULT_THREAD_CAP)
        });
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("the C test thread pool must start")
    })
}

enum CUnitOutcome {
    Ran {
        passed: usize,
        fails: Vec<String>,
        skips: Vec<(String, String)>,
    },
    ExpectFailPass {
        tests: usize,
        names: Vec<String>,
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
    pub required_failures: Vec<String>,
}

#[derive(Debug)]
enum RequiredTestOutcome {
    Passed,
    Failed(String),
    Skipped(String),
}

struct RequiredTestGate {
    required: HashSet<String>,
    seen: HashSet<String>,
    failures: Vec<String>,
}

impl RequiredTestGate {
    fn new(required: &[String]) -> Self {
        Self {
            required: required.iter().cloned().collect(),
            seen: HashSet::new(),
            failures: Vec::new(),
        }
    }

    fn record(&mut self, name: &str, outcome: RequiredTestOutcome) {
        if !self.required.contains(name) {
            return;
        }
        self.seen.insert(name.to_string());
        match outcome {
            RequiredTestOutcome::Passed => {}
            RequiredTestOutcome::Failed(detail) => self
                .failures
                .push(format!("required test `{name}` failed: {detail}")),
            RequiredTestOutcome::Skipped(reason) => self
                .failures
                .push(format!("required test `{name}` was skipped: {reason}")),
        }
    }

    fn finish(mut self) -> Vec<String> {
        for name in self.required.difference(&self.seen) {
            self.failures
                .push(format!("required test `{name}` was not discovered"));
        }
        self.failures.sort();
        self.failures
    }
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

#[derive(Clone, Debug, Default)]
struct CLinkPlan {
    include_dirs: Vec<std::path::PathBuf>,
    library_dirs: Vec<std::path::PathBuf>,
    libraries: Vec<String>,
    rpaths: Vec<std::path::PathBuf>,
}

/// Stone and a game's engine libs cannot change during one `rock test` run,
/// so each game path builds them at most once; a failed build replays its
/// error to every later program test that links them.
#[derive(Default)]
struct CEngineBuilds {
    outcomes: HashMap<String, std::result::Result<(), String>>,
}

impl CEngineBuilds {
    fn ensure(&mut self, game_path: &str, build: impl FnOnce(&str) -> Result<()>) -> Result<()> {
        self.outcomes
            .entry(game_path.to_string())
            .or_insert_with(|| build(game_path).map_err(|error| error.to_string()))
            .clone()
            .map_err(anyhow::Error::msg)
    }
}

struct CForeignProvider {
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
            header: "stone.h",
            include_dir: "stone-rs/include",
            build: CForeignBuild::Stone,
        }),
        _ => None,
    }
}

pub fn c_test(paths: &[String], required_tests: &[String], show_passing: bool) -> Result<()> {
    let probe = Command::new("clang").arg("--version").output();
    if !matches!(probe, Ok(ref output) if output.status.success()) {
        anyhow::bail!("C backend requires `clang` on PATH");
    }

    let start = Instant::now();
    let summary = c_test_run_with_required_tests(paths, true, required_tests, show_passing)?;
    crate::invocation_log::note_phase("units", start.elapsed().as_millis() as u64);
    crate::invocation_log::note_counts(&[
        ("units_selected", summary.files),
        ("units_failed", summary.unit_failures),
        ("tests_total", summary.tests_passed + summary.tests_failed + summary.tests_skipped),
        ("tests_passed", summary.tests_passed),
        ("tests_failed", summary.tests_failed),
        ("tests_skipped", summary.tests_skipped),
    ]);
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
    if !summary.required_failures.is_empty() {
        println!("\n{}", "=== required test failures (c) ===".bold());
        for failure in &summary.required_failures {
            println!("  {} {}", "FAIL".red(), failure);
        }
    }

    let ablation_fatals = crate::ablation_report::report("rock test --backend c");
    if summary.tests_failed > 0
        || summary.unit_failures > 0
        || !summary.required_failures.is_empty()
        || ablation_fatals
    {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

pub fn c_test_run(paths: &[String], print: bool) -> Result<CSuiteSummary> {
    c_test_run_with_required_tests(paths, print, &[], false)
}

fn c_test_run_with_required_tests(
    paths: &[String],
    print: bool,
    required_tests: &[String],
    show_passing: bool,
) -> Result<CSuiteSummary> {
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;
    let mut summary = CSuiteSummary {
        files: 0,
        tests_passed: 0,
        tests_failed: 0,
        tests_skipped: 0,
        unit_failures: 0,
        skip_reasons: Vec::new(),
        required_failures: Vec::new(),
    };
    let mut all_skip_reasons = Vec::new();
    let mut required_gate = RequiredTestGate::new(required_tests);
    let engine_builds = std::sync::Mutex::new(CEngineBuilds::default());
    let units = files.iter().chain(&package_dirs).collect::<Vec<_>>();

    // Files check, emit and run concurrently; the reporter prints and records
    // units in order as soon as each prefix is complete.
    let ablation = skull_mir::ablation::current_context();
    let (sender, receiver) = std::sync::mpsc::channel();
    let (file_units, package_units) = units.split_at(files.len());
    std::thread::scope(|scope| {
        let file_sender = sender.clone();
        scope.spawn(|| test_pool().install(|| {
            file_units
                .par_iter()
                .enumerate()
                .for_each_with(file_sender, |sender, (index, path)| {
                    let _ablation = skull_mir::ablation::enter_context(ablation.clone());
                    let mut required = Vec::new();
                    let outcome = build_file(path, &mut required, &engine_builds);
                    sender
                        .send((index, outcome, required))
                        .expect("the C test reporter outlives its workers");
                });
        }));
        // Packages build one at a time off the rayon pool: each holds a
        // compile slot while its tests run on rayon workers, and a worker
        // waiting for that slot would be one the tests cannot use.
        let (ablation, engine_builds) = (&ablation, &engine_builds);
        scope.spawn(move || {
            let _ablation = skull_mir::ablation::enter_context(ablation.clone());
            for (offset, path) in package_units.iter().enumerate() {
                let mut required = Vec::new();
                let outcome = build_package(path, &mut required, &engine_builds);
                sender
                    .send((file_units.len() + offset, outcome, required))
                    .expect("the C test reporter outlives its workers");
            }
        });
        let mut finished = std::collections::BTreeMap::new();
        let mut next = 0;
        for (index, outcome, required) in receiver {
            finished.insert(index, (outcome, required));
            while let Some((outcome, required)) = finished.remove(&next) {
                record_unit(
                    &mut summary,
                    &mut all_skip_reasons,
                    short_path(units[next]),
                    outcome,
                    print,
                    show_passing,
                );
                for (name, outcome) in required {
                    if print && show_passing {
                        match &outcome {
                            RequiredTestOutcome::Passed => println!("  PASS {name}"),
                            RequiredTestOutcome::Skipped(reason) => println!("  SKIP {name}: {reason}"),
                            RequiredTestOutcome::Failed(detail) => println!("  FAIL {name}: {detail}"),
                        }
                    }
                    required_gate.record(&name, outcome);
                }
                next += 1;
            }
        }
    });
    summary.skip_reasons = reason_histogram(all_skip_reasons.iter().map(String::as_str));
    summary.required_failures = required_gate.finish();
    Ok(summary)
}

fn record_unit(
    summary: &mut CSuiteSummary,
    all_skip_reasons: &mut Vec<String>,
    label: String,
    outcome: CUnitOutcome,
    print: bool,
    show_passing: bool,
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
                if !show_passing {
                    for detail in &fails {
                        println!("  {} {}", "FAIL".red(), detail);
                    }
                }
            }
        }
        CUnitOutcome::ExpectFailPass { tests, names } => {
            summary.tests_passed += tests;
            if print {
                println!(
                    "{:.<60} {}  ({} pass, 0 fail, 0 skip)",
                    label,
                    "ok".green(),
                    tests
                );
                if show_passing {
                    for name in names { println!("  PASS {name}"); }
                }
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

fn build_file(
    file: &str,
    required: &mut Vec<(String, RequiredTestOutcome)>,
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
) -> CUnitOutcome {
    if let Err(error) = crate::package::prepare_test_sound_bank(file) {
        return CUnitOutcome::UnitFail {
            reason: "sound bank preparation failed".to_string(),
            rendered: format!("sound bank preparation failed for {file}: {error:#}"),
        };
    }
    let checked = match check_file(file) {
        Ok(checked) => checked,
        Err(error) => {
            if let Some(tests) = error.expected_failure_tests {
                let mut sources = crate::SourceFiles::empty();
                let source = match fs_err::read_to_string(file) {
                    Ok(source) => source,
                    Err(error) => return CUnitOutcome::UnitFail {
                        reason: "cannot reread expected-failure test".to_string(),
                        rendered: error.to_string(),
                    },
                };
                sources.add_or_update_file(crate::source_file_from_path_and_contents(
                    file.to_string(), file.to_string(), source,
                ));
                let names = crate::test::find_test_functions_from_sources(&sources)
                    .into_iter().map(|test| test.name).collect();
                return CUnitOutcome::ExpectFailPass { tests, names };
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
    run_tests(file, &checked.ws, &tests, required, engine_builds)
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

fn build_package(
    dir: &str,
    required: &mut Vec<(String, RequiredTestOutcome)>,
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
) -> CUnitOutcome {
    if let Err(error) = crate::package::prepare_test_sound_bank(dir) {
        return CUnitOutcome::UnitFail {
            reason: "sound bank preparation failed".to_string(),
            rendered: format!("sound bank preparation failed for {dir}: {error:#}"),
        };
    }
    // Held through the tests: each test's program is emitted from the
    // checked workspace, which stays resident until the unit ends.
    let _slot = match crate::compile_slots::acquire(dir) {
        Ok(slot) => slot,
        Err(error) => {
            return CUnitOutcome::UnitFail {
                reason: "no compile slot".to_string(),
                rendered: format!("{error:#}"),
            };
        }
    };
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
    run_tests(dir, &checked.ws, &tests, required, engine_builds)
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
        sources
            .production_only(std::path::Path::new(dir))
            .map_err(|error| FileCheckError {
                reason: "cannot classify production sources".to_string(),
                rendered: error.to_string(),
                expected_failure_tests: None,
            })?
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
    drop(modules);
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

enum CTestRun {
    Skipped(String),
    Ran(std::result::Result<String, String>),
}

fn run_tests(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    tests: &[(FuncRef, skull::SkullTest)],
    required: &mut Vec<(String, RequiredTestOutcome)>,
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
) -> CUnitOutcome {
    let stone_dir = match crate::skull_backend::configured_stone_dir(std::path::Path::new(path)) {
        Ok(stone_dir) => stone_dir,
        Err(error) => return CUnitOutcome::UnitFail {
            reason: "Stone root resolution failed".to_string(),
            rendered: error.to_string(),
        },
    };
    let ablation = skull_mir::ablation::current_context();
    #[cfg(unix)]
    let runs = test_pool().install(|| {
        let _ablation = skull_mir::ablation::enter_context(ablation);
        batch::run(path, ws, tests, engine_builds, stone_dir.as_deref())
    });
    // The fork supervisor requires Unix. Other native hosts retain fresh
    // processes for test isolation until they have a native supervisor.
    #[cfg(not(unix))]
    let runs = test_pool().install(|| {
        tests.par_iter().map_init(
            || skull_mir::ablation::enter_context(ablation.clone()),
            |_, (func, test)| run_test(path, ws, *func, test, engine_builds, stone_dir.as_deref()),
        ).collect::<Vec<_>>()
    });

    let mut passed = 0;
    let mut fails = Vec::new();
    let mut skips = Vec::new();
    for ((_, test), run) in tests.iter().zip(runs) {
        let result = match run {
            CTestRun::Skipped(reason) => {
                required.push((test.name.clone(), RequiredTestOutcome::Skipped(reason.clone())));
                skips.push((test.name.clone(), reason));
                continue;
            }
            CTestRun::Ran(result) => result,
        };
        let verdict = match (&test.returns, result) {
            (_, Err(detail)) => Err(detail),
            (Some(expect), Ok(value)) => compare_value(expect, &value, FLOAT_REL_TOL),
            (None, Ok(stdout)) => match &test.output {
                Some(expected) => {
                    let got = stdout.trim_end();
                    if got == expected {
                        Ok(())
                    } else {
                        Err(format!(
                            "output mismatch: expected <{:?}> got <{:?}>",
                            expected, got
                        ))
                    }
                }
                None => Ok(()),
            },
        };
        match verdict {
            Ok(()) => {
                passed += 1;
                required.push((test.name.clone(), RequiredTestOutcome::Passed));
            }
            Err(detail) => {
                required.push((test.name.clone(), RequiredTestOutcome::Failed(detail.clone())));
                fails.push(format!("{}: {detail}", test.name));
            }
        }
    }
    CUnitOutcome::Ran {
        passed,
        fails,
        skips,
    }
}

#[cfg(not(unix))]
fn run_test(
    path: &str,
    ws: &bone_semantic::inference::Workspace,
    func: FuncRef,
    test: &skull::SkullTest,
    engine_builds: &std::sync::Mutex<CEngineBuilds>,
    stone_dir: Option<&std::path::Path>,
) -> CTestRun {
    if test.returns.is_some() {
        return match skull_codegen_c::emit_value_test(ws, func, &test.name) {
            Ok(emitted) => CTestRun::Ran(compile_and_run(&test.name, &emitted.source, stone_dir)),
            Err(error) => CTestRun::Skipped(error.message),
        };
    }
    let emitted = match skull_codegen_c::emit_test(ws, func) {
        Ok(emitted) => emitted,
        Err(error) => return CTestRun::Skipped(error.message),
    };
    let link = {
        let mut engine_builds = engine_builds
            .lock()
            .expect("a C test worker panicked while building engine libs");
        c_link_plan_with(path, &emitted, &mut engine_builds)
    };
    CTestRun::Ran(
        link.map_err(|error| error.to_string())
            .and_then(|link| compile_and_run_program(&emitted.source, &link, stone_dir)),
    )
}

#[cfg(not(unix))]
fn compile_and_run_program(
    source: &str,
    link: &CLinkPlan,
    stone_dir: Option<&std::path::Path>,
) -> std::result::Result<String, String> {
    let compiled = compile_source_with_link(source, link)?;
    let run = run_native_test(&compiled.binary, C_NATIVE_TEST_TIMEOUT, stone_dir)?;
    if !run.status.success() {
        return Err(format!(
            "native test exited {}: {}",
            run.status,
            String::from_utf8_lossy(&run.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).into_owned())
}

#[cfg(not(unix))]
fn compile_and_run(
    test_name: &str,
    source: &str,
    stone_dir: Option<&std::path::Path>,
) -> std::result::Result<String, String> {
    let compiled = compile_source(source)?;
    let run = run_native_test(&compiled.binary, C_NATIVE_TEST_TIMEOUT, stone_dir)?;
    if !run.status.success() {
        return Err(format!(
            "native test exited {}: {}",
            run.status,
            String::from_utf8_lossy(&run.stderr).trim()
        ));
    }
    value_result(test_name, &String::from_utf8_lossy(&run.stdout))
}

fn value_result(test_name: &str, stdout: &str) -> std::result::Result<String, String> {
    let markers = parse_skull_markers(stdout).ok_or_else(|| {
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

#[cfg(any(not(unix), test))]
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
    clang_compile_sources(&[(c_path.to_path_buf(), source.to_string())], binary, link)
}

fn clang_compile_sources(
    sources: &[(std::path::PathBuf, String)],
    binary: &std::path::Path,
    link: &CLinkPlan,
) -> std::result::Result<(), String> {
    invalidate_artifact(binary)
        .map_err(|error| format!("could not invalidate {}: {error}", binary.display()))?;
    let mut command = clang_command(&link.include_dirs);
    command.args(sources.iter().map(|(path, _)| path));
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
            sources.iter().map(|(_, source)| source.as_str()).collect::<Vec<_>>().join("\n")
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

fn clang_command(include_dirs: &[std::path::PathBuf]) -> Command {
    let mut command = Command::new("clang");
    command.args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"]);
    for dir in include_dirs { command.arg("-I").arg(dir); }
    command
}

#[cfg(unix)]
enum CObjectCompileError {
    Test(String),
    Infrastructure(String),
}

#[cfg(unix)]
fn clang_compile_object(
    source: &str,
    c_path: &std::path::Path,
    object: &std::path::Path,
    include_dirs: &[std::path::PathBuf],
) -> std::result::Result<(), CObjectCompileError> {
    let compile = clang_command(include_dirs).arg("-c").arg(c_path).arg("-o").arg(object)
        .output().map_err(|error| CObjectCompileError::Infrastructure(format!("could not run clang: {error}")))?;
    if !compile.status.success() {
        return Err(CObjectCompileError::Test(format!("generated C did not compile:\n{}\n--- generated C ---\n{}",
            String::from_utf8_lossy(&compile.stderr), source)));
    }
    if !object.is_file() {
        return Err(CObjectCompileError::Infrastructure(format!("clang succeeded without creating {}", object.display())));
    }
    Ok(())
}

fn c_link_plan(path: &str, emitted: &skull_codegen_c::CProgram) -> Result<CLinkPlan> {
    c_link_plan_with(path, emitted, &mut CEngineBuilds::default())
}

fn c_link_plan_with(
    path: &str,
    emitted: &skull_codegen_c::CProgram,
    engine_builds: &mut CEngineBuilds,
) -> Result<CLinkPlan> {
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
        link.libraries.push(match provider.build {
            CForeignBuild::Stone => rock_host::engine_build::c_libstone_link_name().to_string(),
        });
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
        engine_builds.ensure(game_path, rock_host::engine_build::build_engine_libs_for_c)?;
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

fn main_ref(
    ws: &bone_semantic::inference::Workspace,
    entry_mid: bone_semantic::prelude::ModuleId,
) -> Result<FuncRef> {
    let mut mains = ws
        .module(entry_mid)
        .functions
        .iter()
        .filter(|(_, function)| {
            skull_mir::mangle::declared_name(function.name.as_str()) == "main"
                && function.param_decls.is_empty()
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

fn emit_main_program(path: &str) -> Result<skull_codegen_c::CProgram> {
    if std::path::Path::new(path).is_dir() {
        let _slot = crate::compile_slots::acquire(path)?;
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
    rock_host::compiler_thp::configure_runtime_command(&mut command);
    command.env(
        rock_host::project::lib_path_var(),
        rock_host::project::lib_path_with_inherited(),
    );
    command
}

fn c_native_test_command(
    binary: &std::path::Path,
    stone_dir: Option<&std::path::Path>,
) -> std::result::Result<(Command, tempfile::TempDir), String> {
    let mut command = c_native_command(binary);
    // Match Lua @tests: explicit GL setup is hidden and may use offscreen SDL.
    command.env("STONE_HEADLESS", "1");
    crate::common::apply_test_rock_dir(&mut command);
    // Match Lua's preamble priority without mutating the concurrent runner's environment.
    if std::env::var_os("STONE_SOURCE_DIR").is_none() {
        if let Some(stone_dir) = stone_dir {
            command.env("STONE_SOURCE_DIR", stone_dir);
        }
    }
    let scratch = crate::common::test_scratch_dir(&mut command)
        .map_err(|error| format!("native test scratch directory failed: {error}"))?;
    Ok((command, scratch))
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

#[cfg(any(not(unix), test))]
fn run_native_test(
    binary: &std::path::Path,
    timeout: Duration,
    stone_dir: Option<&std::path::Path>,
) -> std::result::Result<std::process::Output, String> {
    let (mut command, _scratch) = c_native_test_command(binary, stone_dir)?;
    let mut child = command
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

pub fn c_rejections(path: &str, output: &str) -> Result<()> {
    let out = std::path::Path::new(output);
    reject_input_alias(path, out)?;
    let report = if std::path::Path::new(path).is_dir() {
        let _slot = crate::compile_slots::acquire(path)?;
        let checked = check_project(path, true).map_err(|error| anyhow::anyhow!(error.rendered))?;
        let main = main_ref(&checked.ws, checked.entry_mid)?;
        skull_codegen_c::diagnose_program(&checked.ws, main)
    } else {
        let checked = check_file(path).map_err(|error| anyhow::anyhow!(error.rendered))?;
        let main = main_ref(&checked.ws, checked.user_mid)?;
        skull_codegen_c::diagnose_program(&checked.ws, main)
    };
    if let Some(parent) = out.parent() && !parent.as_os_str().is_empty() {
        fs_err::create_dir_all(parent)?;
    }
    rock_host::project::write_atomic(out, &report.to_tsv())?;
    for row in &report.rejections {
        let location = &row.source_location;
        log::error!(target: "C-REJECTIONS", "{}:{}:{} [{} / {}] {} (symbol {}; in {})",
            location.file, location.line_range.0 + 1, location.col_range.0 + 1, row.stage, row.status,
            row.diagnostic.message, row.symbol, row.enclosing_function);
    }
    for limit in &report.coverage_limits {
        log::warn!(target: "C-REJECTIONS", "coverage limit: {limit}");
    }
    log::info!(target: "C-REJECTIONS",
        "{} rows; graph {} instances / {} concrete plans; visited {}; typed {} bodies / {} globals; no artifact emitted; report {}",
        report.rejections.len(), report.graph_instances, report.concrete_plans, report.visited_instances,
        report.typed_bodies, report.typed_globals, out.display());
    if !report.rejections.is_empty() || !report.coverage_limits.is_empty() {
        anyhow::bail!("C diagnostic traversal found {} rejection/continuation rows and {} coverage limits; no artifact emitted",
            report.rejections.len(), report.coverage_limits.len());
    }
    Ok(())
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

    fn slot_probe_package(source: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("package");
        std::fs::write(dir.path().join("stick.toml"),
            "[package]\nname = \"slot-probe\"\ntype = \"lib\"\n").unwrap();
        std::fs::write(dir.path().join("probe.rock"), source).unwrap();
        dir
    }

    #[test]
    fn package_tests_wait_for_a_compile_slot() {
        let package = slot_probe_package("@test fn probe() {}\n");
        let dir = package.path().to_str().unwrap().to_owned();
        crate::compile_slots::assert_compile_waits_for_slot(move || {
            let engine_builds = std::sync::Mutex::new(super::CEngineBuilds::default());
            super::build_package(&dir, &mut Vec::new(), &engine_builds);
        });
    }

    #[test]
    fn project_emission_waits_for_a_compile_slot() {
        let package = slot_probe_package("fn main() {}\n");
        let dir = package.path().to_str().unwrap().to_owned();
        let emitted = crate::compile_slots::assert_compile_waits_for_slot(move || {
            super::emit_main_program(&dir).map(|_| ()).map_err(|error| format!("{error:#}"))
        });
        emitted.unwrap();
    }

    #[test]
    fn required_test_gate_rejects_missing_names() {
        let required = vec!["mag_world::missing_test".to_string()];
        let failures = super::RequiredTestGate::new(&required).finish();
        assert_eq!(
            failures,
            ["required test `mag_world::missing_test` was not discovered"]
        );
    }

    #[test]
    fn required_test_gate_fails_required_skips_but_ignores_unrequired_skips() {
        let required = vec!["mag_world::required_test".to_string()];
        let mut gate = super::RequiredTestGate::new(&required);
        gate.record(
            "mag_world::required_test",
            super::RequiredTestOutcome::Skipped("unsupported emitter case".to_string()),
        );
        gate.record(
            "mag_world::optional_test",
            super::RequiredTestOutcome::Skipped("unsupported emitter case".to_string()),
        );
        assert_eq!(
            gate.finish(),
            ["required test `mag_world::required_test` was skipped: unsupported emitter case"]
        );
    }

    #[test]
    fn required_test_gate_keeps_failures_and_accepts_passes() {
        let required = vec![
            "mag_world::failed_test".to_string(),
            "mag_world::passed_test".to_string(),
        ];
        let mut gate = super::RequiredTestGate::new(&required);
        gate.record(
            "mag_world::failed_test",
            super::RequiredTestOutcome::Failed("assertion mismatch".to_string()),
        );
        gate.record("mag_world::passed_test", super::RequiredTestOutcome::Passed);
        assert_eq!(
            gate.finish(),
            ["required test `mag_world::failed_test` failed: assertion mismatch"]
        );
    }

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
    fn engine_builds_run_once_per_game_path_and_replay_failures() {
        let mut builds = super::CEngineBuilds::default();
        let mut calls = Vec::new();
        for game_path in ["mag-world", "mag-world", "broken", "broken", "mag-world"] {
            let outcome = builds.ensure(game_path, |path| {
                calls.push(path.to_string());
                if path == "broken" {
                    anyhow::bail!("stone build failed");
                }
                Ok(())
            });
            if game_path == "broken" {
                let error = outcome.expect_err("a failed build must fail every later caller");
                assert_eq!(error.to_string(), "stone build failed");
            } else {
                outcome.expect("a built game path stays built");
            }
        }
        assert_eq!(calls, ["mag-world", "broken"]);
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
        assert!(summary.required_failures.is_empty());
    }

    #[test]
    fn parallel_c_tests_report_into_the_callers_ablation_census() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/c_backend_smoke.rock")
            .to_string_lossy()
            .into_owned();
        let _census = skull_mir::ablation::scope(true);
        let summary = super::c_test_run(&[path], false).expect("run C backend smoke");
        assert_eq!(summary.tests_passed, 10);
        assert!(
            skull_mir::ablation::examined() > 0,
            "C test emission on rayon workers must record into the caller's census"
        );
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
        assert!(summary.required_failures.is_empty());
    }

    #[test]
    fn c_backend_nullable_identity_field_unwrap_mutates_the_owner() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/c_nullable_identity_field_place.rock")
            .to_string_lossy()
            .into_owned();
        let summary = super::c_test_run(&[path], false).expect("run nullable identity field test");
        assert_eq!(summary.files, 1);
        assert_eq!(summary.tests_passed, 1);
        assert_eq!(summary.tests_failed, 0);
        assert_eq!(summary.tests_skipped, 0);
        assert_eq!(summary.unit_failures, 0);
        assert!(summary.required_failures.is_empty());
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
        let error = super::run_native_test(&compiled.binary, Duration::from_millis(100), None)
            .expect_err("wedged native test must time out");
        assert!(error.contains("timed out after 100ms"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn native_test_scratch_is_writable_unique_and_removed_after_success_and_abort() {
        let mut roots = std::collections::HashSet::new();
        for fail in [false, true] {
            let source = format!(r#"
                #include <stdio.h>
                #include <stdlib.h>
                int main(void) {{
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
                    return 0;
                }}
            "#, if fail { "abort();" } else { "" });
            let compiled = super::compile_source(&source).expect("scratch fixture compiles");
            let output = super::run_native_test(&compiled.binary, Duration::from_secs(3), None)
                .expect("scratch fixture finishes");
            let text = String::from_utf8(output.stdout).unwrap();
            let root = text.trim();
            assert!(std::path::Path::new(root).is_absolute(), "{text}");
            assert!(roots.insert(root.to_owned()), "scratch roots were reused");
            assert!(!std::path::Path::new(root).exists(), "scratch survived: {root}");
            if fail {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    assert_eq!(output.status.signal(), Some(libc::SIGABRT));
                }
                assert!(!output.status.success());
            } else {
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            }
        }
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
