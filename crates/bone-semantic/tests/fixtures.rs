mod fixture_harness;
mod test_support;

type Result = std::result::Result<(), String>;

fn check_rock(file_name: &str, contents: &str) -> Result {
    if file_name == "modules_std.rock" {
        return check_std_fixture(contents);
    }
    let modules = file_name.starts_with("modules_");
    let check = |src: &str| {
        if modules {
            crate::test_support::check_types_embedded_with_cached_stdlib(
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

fn check_std_fixture(contents: &str) -> Result {
    let sections = test_utils::parse_sections(contents);
    let inventory = sections
        .iter()
        .find(|section| section.name == "prelude_inventory")
        .expect("modules_std.rock needs a prelude_inventory section");
    let expected: Vec<String> = inventory
        .content
        .lines()
        .filter_map(|line| line.strip_prefix("// prelude: "))
        .map(ToOwned::to_owned)
        .collect();
    assert!(!expected.is_empty(), "prelude inventory fixture is empty");
    assert!(
        expected.windows(2).all(|pair| pair[0] < pair[1]),
        "prelude inventory must be strictly sorted"
    );

    let cached_stdlib = skull_parser::stdlib::cached()
        .as_ref()
        .expect("stdlib parses");
    let mut std_modules = cached_stdlib.to_vec();
    let std_ws = bone_semantic::semantic_passes::semantic_pass(&mut std_modules)
        .map_err(|e| format!("stdlib semantic pass failed: {e}"))?;
    let std_package = std_ws
        .ns
        .package(u32::MAX)
        .expect("stdlib namespace package is missing");
    let root = std_package
        .node(bone_core::prelude::ustr(""))
        .expect("stdlib namespace root is missing");
    let mut actual: Vec<String> = root
        .owned_types
        .keys()
        .chain(root.owned_globals.keys())
        .chain(root.owned_callables.keys())
        .chain(root.owned_decorators.keys())
        .map(|name| name.as_str().to_owned())
        .collect();
    actual.sort();
    actual.dedup();
    assert_eq!(actual, expected, "default prelude inventory changed");

    let prelude = sections
        .iter()
        .find(|section| section.name == "v1_default_prelude")
        .expect("modules_std.rock needs a v1_default_prelude section");
    let user_files = crate::test_support::parse_embedded_files(prelude.content);
    assert!(
        !user_files.is_empty(),
        "v1_default_prelude has no embedded file"
    );
    crate::test_support::check_types_embedded_with_cached_stdlib(
        &user_files,
        stdlib_sources(),
        cached_stdlib,
    )
    .map_err(|e| format!("v1 default prelude failed: {e}"))
}

fn main() -> std::process::ExitCode {
    fixture_harness::run("check_rock", "tests/fixtures", check_rock)
}
