//! The front end the JS and GLSL `@test` executors share: check one test unit
//! (a single file or a `stick.toml` package built as one workspace) and
//! discover its `@test` fns. Emission and execution stay per backend.

use crate::bone_backend::{categorize, short_path};
use bone_semantic::inference::Workspace;
use rock_core::{CompilerError, SourceFiles};
use skull_mir::mir::FuncRef;

pub(crate) struct UnitFailure {
    pub reason: String,
    pub rendered: String,
}

impl UnitFailure {
    /// The unit's `FAILED` line, plus the rendered diagnostic under
    /// `ROCK_TEST_VERBOSE`.
    pub fn print(&self, label: &str) {
        use colored::Colorize;
        println!(
            "{:.<60} {}  {}",
            label,
            "FAILED".red(),
            self.reason.dimmed()
        );
        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
            println!("{}", self.rendered);
        }
    }

    fn compile(sources: &SourceFiles, errs: &[CompilerError]) -> Self {
        UnitFailure {
            reason: categorize(&errs[0].message),
            rendered: crate::skull_backend::render_project_errs(sources, errs),
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
    crate::comptime::stamp_workspace(ws, std::path::Path::new(unit)).map_err(|e| UnitFailure {
        reason: "build var resolution failed".to_string(),
        rendered: format!("build var resolution failed for {unit}: {e:#}"),
    })
}

/// Parse and bone-check one isolated test file with the stdlib.
pub(crate) fn check_file(file: &str, build_vars: bool) -> Result<UnitCheck, UnitFailure> {
    let src = fs_err::read_to_string(file).map_err(|e| UnitFailure {
        reason: e.to_string(),
        rendered: e.to_string(),
    })?;
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
        .map_err(|e| UnitFailure {
            reason: "cannot load project".to_string(),
            rendered: e.to_string(),
        })?;
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
    println!("{:.<60} {}  {}", label, "PANIC".red().bold(), msg.dimmed());
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

/// Run files in input order, optionally on a bounded worker pool, then run
/// package units serially. Catch panics per unit and return short labels in
/// files-before-packages order.
pub(crate) fn run_units<T: Send>(
    files: &[String],
    package_dirs: &[String],
    file_threads: Option<usize>,
    build: impl Fn(&str, bool) -> T + Sync,
) -> Vec<(String, std::thread::Result<T>)> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let run = |unit: &str, is_package| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(unit, is_package)))
    };
    let mut outcomes = match file_threads {
        Some(threads) => crate::parallel_runner::map_in_order(files, threads, |file| {
            (short_path(file), run(file, false))
        }),
        None => files
            .iter()
            .map(|file| (short_path(file), run(file, false)))
            .collect(),
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
        for file_threads in [None, Some(2)] {
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
