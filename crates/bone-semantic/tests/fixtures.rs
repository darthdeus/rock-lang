use datatest_stable::Utf8Path;

fn check_rock(_path: &Utf8Path, contents: String) -> datatest_stable::Result<()> {
    // Modules-v1 multi-file fixtures: sections hold embedded `//--` files.
    if _path.file_name().map_or(false, |n| n.starts_with("modules_")) {
        if _path.file_name() == Some("modules_std.rock") {
            return check_std_fixture(&contents);
        }
        return check_modules_fixture(&contents);
    }
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

const STDLIB_FILES: &[(&str, &str)] = &[
    ("<stdlib>/unified.rock", include_str!("../../../stdlib/unified.rock")),
    ("<stdlib>/logging.rock", include_str!("../../../stdlib/logging.rock")),
    ("<stdlib>/vec.rock", include_str!("../../../stdlib/vec.rock")),
    ("<stdlib>/mat.rock", include_str!("../../../stdlib/mat.rock")),
    ("<stdlib>/arena.rock", include_str!("../../../stdlib/arena.rock")),
    ("<stdlib>/scene.rock", include_str!("../../../stdlib/scene.rock")),
    ("<stdlib>/strings.rock", include_str!("../../../stdlib/strings.rock")),
    ("<stdlib>/json.rock", include_str!("../../../stdlib/json.rock")),
    ("<stdlib>/shader.rock", include_str!("../../../stdlib/shader.rock")),
    ("<stdlib>/js.rock", include_str!("../../../stdlib/js.rock")),
];

fn check_std_fixture(contents: &str) -> datatest_stable::Result<()> {
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

    let mut std_modules = Vec::new();
    for (file, source) in STDLIB_FILES {
        let module = skull_parser::parse_module_clean(source, bone_core::prelude::ustr(file))
            .map_err(|e| format!("{file}: {e}"))?;
        std_modules.push(module);
    }
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
    let user_files = bone_semantic::test_support::parse_embedded_files(prelude.content);
    assert!(!user_files.is_empty(), "v1_default_prelude has no embedded file");
    let mut modules = Vec::new();
    for (file, source) in STDLIB_FILES {
        let module = skull_parser::parse_module_clean(source, bone_core::prelude::ustr(file))
            .map_err(|e| format!("{file}: {e}"))?;
        modules.push(module);
    }
    for file in &user_files {
        let mut module = skull_parser::parse_module_with_dialect(
            file.src,
            bone_core::prelude::ustr(file.file),
            file.dialect,
        )
        .map_err(|e| format!("{}: {e}", file.file))?;
        module.package = file.package.map(|(name, id)| rock_core::source_code::PackageTag {
            id,
            self_name: name.to_owned(),
        });
        modules.push(module);
    }
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules)
        .map_err(|e| format!("stdlib + v1 semantic pass failed: {e}"))?;
    for file in &user_files {
        let module = ws
            .modules
            .iter()
            .find(|module| module.file == bone_core::prelude::ustr(file.file))
            .expect("v1 prelude fixture module missing");
        let body_error = ws
            .inference(module.id)
            .into_iter()
            .flat_map(|inf| inf.errors.values().flatten())
            .chain(module.lowering_errors.values().flatten())
            .chain(module.interface_errors.iter())
            .next();
        if let Some(error) = body_error {
            return Err(format!("v1 default prelude failed: {error}").into());
        }
    }
    Ok(())
}

fn check_modules_fixture(contents: &str) -> datatest_stable::Result<()> {
    let sections = test_utils::parse_sections(contents);
    if sections.is_empty() {
        if let Err(e) = bone_semantic::test_support::check_types_embedded_with_stdlib(
            &bone_semantic::test_support::parse_embedded_files(contents),
            STDLIB_FILES,
        ) {
            panic!("{}", e);
        }
        return Ok(());
    }
    let mut failures = Vec::new();
    for section in sections {
        // `@fail` expects any rejection; `@fail=<text>` also pins the reason,
        // so a case cannot pass by failing for an unrelated reason, and a
        // trailing ` @not=<text>` pins text the rejection must not contain.
        let (name, expect_fail, want, unwanted) = match section.name.split_once(" @fail") {
            Some((n, rest)) => {
                let (rest, unwanted) = match rest.split_once(" @not=") {
                    Some((rest, unwanted)) => (rest, Some(unwanted.trim().to_string())),
                    None => (rest, None),
                };
                (n.to_string(), true, rest.strip_prefix('=').map(|w| w.trim().to_string()), unwanted)
            }
            None => (section.name.to_string(), false, None, None),
        };
        let files = bone_semantic::test_support::parse_embedded_files(section.content);
        match (
            bone_semantic::test_support::check_types_embedded_with_stdlib(&files, STDLIB_FILES),
            expect_fail,
        ) {
            (Ok(_), true) => failures.push(format!("section `{name}` was marked @fail but passed")),
            (Err(e), false) => failures.push(format!("section `{name}` failed:\n{e}")),
            (Err(e), true) => {
                if let Some(want) = want {
                    if !e.contains(&want) {
                        failures.push(format!(
                            "section `{name}` failed for the wrong reason: \
                             expected the error to contain `{want}`, got:\n{e}"
                        ));
                    }
                }
                if let Some(unwanted) = unwanted {
                    if e.contains(&unwanted) {
                        failures.push(format!(
                            "section `{name}` failed with text it must not contain: `{unwanted}`, got:\n{e}"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    if !failures.is_empty() {
        panic!("{}", failures.join("\n\n"));
    }
    Ok(())
}

datatest_stable::harness! {
    { test = check_rock, root = "tests/fixtures", pattern = r"^.*\.rock$" },
}
