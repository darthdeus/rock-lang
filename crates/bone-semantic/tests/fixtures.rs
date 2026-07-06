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
                _ => {}
            }
        }
    }
    Ok(())
}

datatest_stable::harness! {
    { test = check_rock, root = "tests/fixtures", pattern = r"^.*\.rock$" },
}
