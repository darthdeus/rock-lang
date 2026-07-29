use datatest_stable::Utf8Path;

fn check_rock(_path: &Utf8Path, contents: String) -> datatest_stable::Result<()> {
    let sections = test_utils::parse_sections(&contents);

    if sections.is_empty() {
        // Bare fixture (no `//- name` markers) — whole file is one case.
        if let Err(e) = bone_semantic::test_support::check_types(&contents) {
            panic!("{}", e);
        }
    } else {
        for section in sections {
            let (name, expect_fail) = match section.name.strip_suffix(" @fail") {
                Some(n) => (n, true),
                None => (section.name, false),
            };
            match (bone_semantic::test_support::check_types(section.content), expect_fail) {
                (Ok(_), true) => {
                    panic!("section `{name}` was marked @fail but passed")
                }
                (Err(e), false) => panic!("section `{name}` failed:\n{e}"),
                // `check_types` reports a section with no `//^` marker as an
                // error, which an `@fail` section would otherwise absorb as its
                // expected failure -- so a rule that stopped being enforced
                // reads as pinned. Only a real type error counts as the fail.
                (Err(e), true) if e == "no //^ annotations in source" => panic!(
                    "section `{name}` is marked @fail but produced no type error \
                     (it only failed on the missing `//^` marker) -- the rule it \
                     claims to pin is not enforced"
                ),
                _ => {}
            }
        }
    }
    Ok(())
}

datatest_stable::harness! {
    { test = check_rock, root = "tests/fixtures", pattern = r"^.*\.rock$" },
}
