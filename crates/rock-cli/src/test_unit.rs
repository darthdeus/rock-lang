//! The front end the JS and GLSL `@test` executors share: check one test unit
//! (a single file or a `stick.toml` package built as one workspace) and
//! discover its `@test` fns. Emission and execution stay per backend.

use crate::bone_backend::categorize;
use bone_semantic::inference::Workspace;
use rock_core::{CompilerError, SourceFiles};
use skull_mir::mir::FuncRef;

pub(crate) struct UnitFailure {
    pub reason: String,
    pub rendered: String,
}

impl UnitFailure {
    fn compile(sources: &SourceFiles, errs: &[CompilerError]) -> Self {
        UnitFailure {
            reason: categorize(&errs[0].message),
            rendered: crate::skull_backend::render_project_errs(sources, errs),
        }
    }
}

pub(crate) enum UnitCheck {
    Checked {
        ws: Workspace,
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
    let tests = skull::module_tests_with_ids(&ws, module)
        .map_err(|e| UnitFailure::compile(&sources, &[e]))?
        .into_iter()
        .map(|(func, test)| (FuncRef { module, func }, test))
        .collect();
    Ok(UnitCheck::Checked { ws, tests, sources })
}

/// Assemble and bone-check a package with its tests as one workspace.
pub(crate) fn check_package(dir: &str, build_vars: bool) -> Result<UnitCheck, UnitFailure> {
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec).map_err(|e| UnitFailure {
        reason: "cannot load project".to_string(),
        rendered: e.to_string(),
    })?;
    let (mut ws, _entry) = crate::skull_backend::check_project(&sources)
        .map_err(|errs| UnitFailure::compile(&sources, &errs))?;
    if build_vars {
        resolve_build_vars(&mut ws, dir)?;
    }
    let tests =
        crate::skull_backend::collect_package_test_fns(&ws, dir).map_err(|e| UnitFailure {
            reason: categorize(&e.message),
            rendered: e.message,
        })?;
    Ok(UnitCheck::Checked { ws, tests, sources })
}

/// Build every unit with `build`, containing a panic to its unit: bone can
/// panic during inference on some inputs, and one bad unit must not end the
/// suite. Returns each unit's short label and outcome, files before packages.
pub(crate) fn build_units<T>(
    files: &[String],
    package_dirs: &[String],
    build: impl Fn(&str, bool) -> T,
) -> Vec<(String, std::thread::Result<T>)> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let units = files
        .iter()
        .map(|file| (file, false))
        .chain(package_dirs.iter().map(|dir| (dir, true)));
    let outcomes = units
        .map(|(unit, is_package)| {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(unit, is_package)));
            (crate::bone_backend::short_path(unit), outcome)
        })
        .collect();
    std::panic::set_hook(prev_hook);
    outcomes
}
