mod fixture_harness;
mod test_support;

type Result = std::result::Result<(), String>;

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
        if let Err(e) = check(contents) {
            panic!("{e}");
        }
        return Ok(());
    }
    let failures: Vec<String> = sections
        .iter()
        .filter_map(|section| section_failure(section.name, check(section.content)))
        .collect();
    if !failures.is_empty() {
        panic!("{}", failures.join("\n\n"));
    }
    Ok(())
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

fn main() -> std::process::ExitCode {
    fixture_harness::run("check_rock", "tests/fixtures", check_rock)
}
