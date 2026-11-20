//! The front end the JS and GLSL `@test` executors share: check one test unit
//! (a single file or a `stick.toml` package built as one workspace) and
//! discover its `@test` fns. Emission and execution stay per backend.

use crate::bone_backend::{categorize, short_path};
use bone_semantic::inference::Workspace;
use rock_core::{CompilerError, SourceFiles};
use skull_mir::mir::FuncRef;

#[derive(Debug)]
pub(crate) struct UnitFailure {
    pub reason: String,
    pub rendered: String,
}

impl UnitFailure {
    /// The unit's `FAILED` line, plus the rendered diagnostic under
    /// `ROCK_TEST_VERBOSE`.
    pub fn print(&self, label: &str) {
        use colored::Colorize;
        print_unit_line(label, "FAILED".red(), self.reason.dimmed());
        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
            println!("{}", self.rendered);
        }
    }

    pub fn compile(sources: &SourceFiles, errs: &[CompilerError]) -> Self {
        UnitFailure {
            reason: categorize(&errs[0].message),
            rendered: crate::skull_backend::render_project_errs(sources, errs),
        }
    }

    pub fn new(reason: impl Into<String>, rendered: impl std::fmt::Display) -> Self {
        UnitFailure {
            reason: reason.into(),
            rendered: rendered.to_string(),
        }
    }
}

impl From<crate::stone_runtime::RuntimeFailure> for UnitFailure {
    fn from(error: crate::stone_runtime::RuntimeFailure) -> Self {
        Self {
            reason: format!("{}: {}", error.reason, error.rendered),
            rendered: error.rendered,
        }
    }
}

pub(crate) fn prepare_unit(unit: &str) -> Result<(), UnitFailure> {
    crate::stone_runtime::prepare_unit(unit).map_err(|error| UnitFailure {
        reason: "sound bank preparation failed".to_string(),
        rendered: format!("sound bank preparation failed for {unit}: {error:#}"),
    })
}

pub(crate) fn acquire_compile_slot(
    unit: &str,
) -> Result<crate::compile_slots::CompileSlot, UnitFailure> {
    crate::compile_slots::acquire(unit)
        .map_err(|error| UnitFailure::new("no compile slot", format!("{error:#}")))
}

/// Prints the `FAILED` or `PANIC` line of a unit that produced no run.
pub(crate) fn unit_run<T>(
    label: &str,
    outcome: std::thread::Result<Result<T, UnitFailure>>,
) -> Option<T> {
    match outcome {
        Ok(Ok(run)) => Some(run),
        Ok(Err(failure)) => {
            failure.print(label);
            None
        }
        Err(panic) => {
            print_unit_panic(label, &*panic);
            None
        }
    }
}

/// A rejected `@test` annotation names the test and what is wrong with it,
/// so its first line is the reason, uncut.
pub(crate) fn discovery_reason(e: &CompilerError) -> String {
    e.message.lines().next().unwrap_or_default().to_string()
}

pub(crate) enum UnitCheck {
    Checked {
        ws: Box<Workspace>,
        tests: Vec<(FuncRef, skull::SkullTest)>,
        sources: SourceFiles,
    },
    /// bone rejected a file whose every `@test` expected the failure; each
    /// counts as passing.
    ExpectedFailure(usize),
}

fn resolve_build_vars(ws: &mut Workspace, unit: &str) -> Result<(), UnitFailure> {
    crate::comptime::stamp_workspace(ws, std::path::Path::new(unit)).map_err(|e| {
        UnitFailure::new(
            "build var resolution failed",
            format!("build var resolution failed for {unit}: {e:#}"),
        )
    })
}

/// Parse and bone-check one isolated test file with the stdlib.
pub(crate) fn check_file(file: &str, build_vars: bool) -> Result<UnitCheck, UnitFailure> {
    let src = fs_err::read_to_string(file).map_err(|e| UnitFailure::new(e.to_string(), e))?;
    let sources = crate::common::single_source(file, &src);
    let rejected =
        |errs: &[CompilerError]| match crate::skull_backend::all_tests_expect_compile_failure(
            file, &src,
        ) {
            Some(tests) => Ok(UnitCheck::ExpectedFailure(tests.len())),
            None => Err(UnitFailure::compile(&sources, errs)),
        };
    let user = match skull_parser::parse_module(&src, ustr::Ustr::from(file)) {
        Ok(user) => user,
        Err(e) => return rejected(&[e]),
    };
    let user_file = user.file;
    let mut ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => return rejected(&errs),
    };
    if build_vars {
        resolve_build_vars(&mut ws, file)?;
    }
    let module = crate::bone_backend::entry_module_id(&ws, user_file);
    // A malformed `@test` on a bone-accepted file is a rejection, never an
    // expected compile failure.
    let tests = crate::type_holes::module_tests(&ws, module, &sources)
        .map_err(|e| UnitFailure {
            reason: discovery_reason(&e),
            rendered: crate::skull_backend::render_project_errs(&sources, &[e]),
        })?
        .into_iter()
        .map(|(func, test)| (FuncRef { module, func }, test))
        .collect();
    Ok(UnitCheck::Checked {
        ws: Box::new(ws),
        tests,
        sources,
    })
}

/// Assemble and bone-check a package with its tests as one workspace.
pub(crate) fn check_package(dir: &str, build_vars: bool) -> Result<UnitCheck, UnitFailure> {
    let sources = rock_host::compilation_units::load_sources_for_project(std::path::Path::new(dir))
        .map_err(|e| UnitFailure::new("cannot load project", e))?;
    let (mut ws, _entry) = crate::skull_backend::check_project(&sources)
        .map_err(|errs| UnitFailure::compile(&sources, &errs))?;
    if build_vars {
        resolve_build_vars(&mut ws, dir)?;
    }
    let tests =
        crate::skull_backend::collect_package_test_fns(&ws, dir, &sources).map_err(|e| {
            UnitFailure {
                reason: discovery_reason(&e),
                rendered: e.message,
            }
        })?;
    Ok(UnitCheck::Checked {
        ws: Box::new(ws),
        tests,
        sources,
    })
}

/// The line for a unit whose build panicked.
pub(crate) fn print_unit_panic(label: &str, panic: &(dyn std::any::Any + Send)) {
    use colored::Colorize;
    let msg = crate::common::panic_message(panic);
    print_unit_line(label, "PANIC".red().bold(), msg.dimmed());
}

/// The one per-unit line every test runner prints: label, verdict, detail.
pub(crate) fn print_unit_line(
    label: &str,
    status: colored::ColoredString,
    detail: impl std::fmt::Display,
) {
    println!("{label:.<60} {status}  {detail}");
}

/// Reasons bucketed by exact message, most frequent first.
pub(crate) fn print_reasons(title: &str, reasons: &[String]) {
    use colored::Colorize;
    let rows = crate::test_protocol::reason_histogram(reasons.iter().map(String::as_str));
    if rows.is_empty() {
        return;
    }
    println!("\n{}", format!("=== {title} ===").bold());
    const SHOWN: usize = 50;
    for (reason, count) in rows.iter().take(SHOWN) {
        println!("  {:>5}  {}", count.to_string().yellow(), reason);
    }
    if rows.len() > SHOWN {
        println!(
            "  {:>5}  ({} more distinct reasons)",
            "...",
            rows.len() - SHOWN
        );
    }
}

/// What one unit of a JS or GLSL run produced.
#[derive(Default)]
pub(crate) struct UnitRun {
    pub passed: usize,
    /// FAIL detail lines.
    pub fails: Vec<String>,
    /// (test name, reasons) for tests the backend could not emit.
    pub skips: Vec<(String, Vec<String>)>,
    /// Tests outside this backend's reach, which are not skips.
    pub elsewhere: usize,
}

pub(crate) struct BackendTotals {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub elsewhere: usize,
    pub files: usize,
    unit_failures: usize,
    skip_reasons: Vec<String>,
}

impl BackendTotals {
    /// Prints what follows the backend's summary line, then exits 1 if any
    /// test or unit failed.
    pub fn finish(self, skip_title: &str) {
        use colored::Colorize;
        if self.unit_failures > 0 {
            println!(
                "  {} unit(s) failed to build/run (see FAILED lines above)",
                self.unit_failures.to_string().red()
            );
        }
        print_reasons(skip_title, &self.skip_reasons);
        if self.failed > 0 || self.unit_failures > 0 {
            crate::invocation_log::exit(1);
        }
    }
}

/// Builds and runs every unit under `paths`, printing each unit's
/// line, its FAIL details and, under `ROCK_TEST_VERBOSE`, its skips.
pub(crate) fn run_backend(
    paths: &[String],
    elsewhere_label: &str,
    file_threads: usize,
    build: impl Fn(&str, bool) -> Result<UnitRun, UnitFailure> + Sync,
) -> crate::Result<BackendTotals> {
    use colored::Colorize;
    let (files, package_dirs) = crate::skull_backend::collect_test_units(paths)?;
    let outcomes = run_units(&files, &package_dirs, file_threads, build);
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let mut totals = BackendTotals {
        passed: 0,
        failed: 0,
        skipped: 0,
        elsewhere: 0,
        files: outcomes.len(),
        unit_failures: 0,
        skip_reasons: Vec::new(),
    };
    for (label, outcome) in outcomes {
        let Some(run) = unit_run(&label, outcome) else {
            totals.unit_failures += 1;
            continue;
        };
        totals.passed += run.passed;
        totals.failed += run.fails.len();
        totals.skipped += run.skips.len();
        totals.elsewhere += run.elsewhere;
        for (_, reasons) in &run.skips {
            totals.skip_reasons.extend(reasons.iter().cloned());
        }
        if run.passed + run.fails.len() + run.skips.len() + run.elsewhere == 0 {
            continue;
        }
        let status = if run.fails.is_empty() {
            "ok".green()
        } else {
            "FAILED".red()
        };
        let elsewhere = if run.elsewhere > 0 {
            format!(", {} {elsewhere_label}", run.elsewhere)
        } else {
            String::new()
        };
        let counts = format!(
            "({} pass, {} fail, {} skip{elsewhere})",
            run.passed,
            run.fails.len(),
            run.skips.len()
        );
        print_unit_line(&label, status, counts);
        for detail in &run.fails {
            println!("  {} {}", "FAIL".red(), detail);
        }
        if verbose {
            for (name, reasons) in &run.skips {
                println!("  {} {}: {}", "SKIP".yellow(), name, reasons.join(" | "));
            }
        }
    }
    Ok(totals)
}

/// Check either one file or one package with the backend's build-var policy.
pub(crate) fn check_unit(
    unit: &str,
    is_package: bool,
    build_vars: bool,
) -> Result<UnitCheck, UnitFailure> {
    if is_package {
        check_package(unit, build_vars)
    } else {
        check_file(unit, build_vars)
    }
}

pub(crate) fn file_threads(requested: Option<usize>) -> usize {
    if cfg!(feature = "sequential") {
        1
    } else {
        requested.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        })
    }
}

/// Collect file results in input order, optionally on a bounded worker pool, then run
/// package units serially. Catch panics per unit and return short labels in
/// files-before-packages order.
pub(crate) fn run_units<T: Send>(
    files: &[String],
    package_dirs: &[String],
    file_threads: usize,
    build: impl Fn(&str, bool) -> T + Sync,
) -> Vec<(String, std::thread::Result<T>)> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let run = |unit: &str, is_package| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(unit, is_package)))
    };
    let mut outcomes = if file_threads > 1 && files.len() > 1 {
        crate::parallel_runner::map_in_order(files, file_threads, |file| {
            (short_path(file), run(file, false))
        })
    } else {
        files
            .iter()
            .map(|file| (short_path(file), run(file, false)))
            .collect()
    };
    outcomes.extend(
        package_dirs
            .iter()
            .map(|dir| (short_path(dir), run(dir, true))),
    );
    std::panic::set_hook(prev_hook);
    outcomes
}

#[cfg(test)]
mod tests {
    #[test]
    fn schedules_preserve_order_and_contain_unit_panics() {
        let files = vec!["panic-file".to_string(), "file".to_string()];
        let packages = vec!["package".to_string()];
        for file_threads in [1, 2] {
            let outcomes = super::run_units(&files, &packages, file_threads, |unit, package| {
                if unit == "panic-file" {
                    panic!("isolated unit panic");
                }
                format!("{unit}:{package}")
            });
            assert_eq!(
                outcomes
                    .iter()
                    .map(|(label, _)| label.as_str())
                    .collect::<Vec<_>>(),
                ["panic-file", "file", "package"]
            );
            assert!(outcomes[0].1.is_err());
            assert_eq!(outcomes[1].1.as_ref().unwrap(), "file:false");
            assert_eq!(outcomes[2].1.as_ref().unwrap(), "package:true");
        }
    }
}
