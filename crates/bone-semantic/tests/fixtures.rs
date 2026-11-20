use bone_core::prelude::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

type Result = std::result::Result<(), String>;

/// Every file in `tests/fixtures`; `FIXTURE=<substring>` runs the matching ones.
#[test]
fn fixtures_type_check_as_annotated() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let filter = std::env::var("FIXTURE").unwrap_or_default();
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("fixture directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rock"))
        .filter(|path| path.to_string_lossy().contains(&filter))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixture matches `{filter}`");
    let failures: Vec<String> = files
        .par_iter()
        .filter_map(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            let contents = std::fs::read_to_string(path).expect("fixture contents");
            catch_unwind(AssertUnwindSafe(|| check_rock(&name, &contents)))
                .unwrap_or_else(|_| Err("panicked (message above)".to_string()))
                .err()
                .map(|e| format!("---- {name} ----\n{e}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} fixture files failed:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}

fn check_rock(file_name: &str, contents: &str) -> Result {
    let modules = file_name.starts_with("modules_");
    let check = |src: &str| {
        if modules {
            crate::test_support::check_types_embedded(
                &crate::test_support::parse_embedded_files(src),
                stdlib_sources(),
                skull_parser::stdlib::cached()
                    .as_ref()
                    .expect("stdlib parses"),
            )
        } else {
            crate::test_support::check_types(src)
        }
    };
    let sections = test_utils::parse_sections(contents);
    if sections.is_empty() {
        return check(contents);
    }
    let failures: Vec<String> = sections
        .iter()
        .filter_map(|section| section_failure(section.name, check(section.content)))
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n\n"))
    }
}

/// `@fail` expects a rejection, `@fail=<text>` one containing `<text>`, and a
/// trailing ` @not=<text>` one that does not contain `<text>`.
fn section_failure(header: &str, outcome: Result) -> Option<String> {
    let Some((name, rest)) = header.split_once(" @fail") else {
        return outcome
            .err()
            .map(|e| format!("section `{header}` failed:\n{e}"));
    };
    let (rest, unwanted) = match rest.split_once(" @not=") {
        Some((rest, unwanted)) => (rest, Some(unwanted.trim())),
        None => (rest, None),
    };
    let want = rest.strip_prefix('=').map(str::trim);
    let e = match outcome {
        Ok(()) => return Some(format!("section `{name}` was marked @fail but passed")),
        Err(e) => e,
    };
    if e == "no //^ annotations in source" {
        return Some(format!(
            "section `{name}` is marked @fail but produced no type error"
        ));
    }
    if let Some(want) = want.filter(|want| !e.contains(want)) {
        return Some(format!(
            "section `{name}` failed for the wrong reason: \
             expected the error to contain `{want}`, got:\n{e}"
        ));
    }
    if let Some(unwanted) = unwanted.filter(|unwanted| e.contains(unwanted)) {
        return Some(format!(
            "section `{name}` failed with text it must not contain: `{unwanted}`, got:\n{e}"
        ));
    }
    None
}

fn stdlib_sources() -> &'static [(&'static str, &'static str)] {
    static SOURCES: std::sync::OnceLock<Vec<(&'static str, &'static str)>> =
        std::sync::OnceLock::new();
    SOURCES.get_or_init(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|f| {
                (
                    &*Box::leak(rock_stdlib::stdlib_file_id(f.name).into_boxed_str()),
                    f.contents,
                )
            })
            .collect()
    })
}
