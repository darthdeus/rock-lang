use bone_lints::LintOptions;
use rock_core::errors::CompilerErrorLevel;

fn findings(src: &str) -> Vec<bone_lints::Finding> {
    crate::support::checked_lint(
        &[("<deprecated>", src)],
        &LintOptions::only(["deprecated".into()]),
    )
}

#[test]
fn warns_on_each_resolved_call_and_function_value() {
    let src = "@deprecated(\"use fresh instead\")\nfn old(x: int) -> int { x }\nfn apply(f: fn(int) -> int) -> int { f(1) }\nfn main() {\n    let _a = old(1)\n    let _b = 2.old()\n    let _c = old(x: 3)\n    let _d = apply(old)\n}";
    let found = findings(src);
    assert_eq!(found.len(), 4, "{found:?}");
    for finding in &found {
        assert_eq!(finding.level(), CompilerErrorLevel::Warning);
        assert!(finding.error.message.contains("`old`"), "{finding:?}");
        assert!(
            finding.error.message.contains("use fresh instead"),
            "{finding:?}"
        );
    }
    let default = crate::lint(&[("<deprecated>", src)], &LintOptions::default(), true);
    assert_eq!(default.iter().filter(|f| f.lint == "deprecated").count(), 4);
    assert!(
        LintOptions::only(["deprecated".into()])
            .unknown()
            .is_empty()
    );
}

#[test]
fn resolves_overloads_and_does_not_warn_at_the_declaration() {
    let src = "@deprecated\nfn old(x: int) -> int { x }\nfn old(x: string) -> string { x }\nfn main() {\n    let _a = old(1)\n    let _b = old(\"ok\")\n}";
    let found = findings(src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].line, 4);
    assert!(found[0].error.message.contains("deprecated"));
}

#[test]
fn honors_suppression_and_selection() {
    let src = "@deprecated\nfn old() {}\nfn main() {\n    // rock: allow(deprecated)\n    old()\n    old() // rock: allow(deprecated)\n}";
    assert!(findings(src).is_empty(), "{:?}", findings(src));
    assert!(crate::support::lint_only(&[("<deprecated>", src)], "unused_var").is_empty());
}

#[test]
fn warns_in_global_initializers_defaults_and_safe_navigation() {
    let src = "struct Item {}\n@deprecated\nfn old(x: Item) -> int { 1 }\nglobal INITIAL: int = old(Item {})\nstruct Holder { value: int = old(Item {}) }\nfn with_default(value: int = old(Item {})) {}\nfn main() {\n    let item: Item? = Item {}\n    let _value = item?.old()\n}";
    let found = findings(src);
    assert_eq!(found.len(), 4, "{found:?}");
}

#[test]
fn follows_import_aliases_and_ignores_a_user_decorator_with_the_same_name() {
    let src = "mod legacy {\n    pub @deprecated(\"use fresh\")\n    fn old() {}\n}\nmod custom {\n    deco init deprecated {}\n    @deprecated\n    fn fine() {}\n}\nuse legacy::old as alias;\nfn main() { alias() }";
    let found = findings(src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].error.message.contains("use fresh"));
}

#[test]
fn backend_siblings_produce_one_warning_per_reference() {
    let src = "@deprecated(\"use fresh\")\ninline(lua) fn old(x: int) -> int = \"return x\"\n@deprecated(\"use fresh\")\ninline(js) fn old(x: int) -> int = \"return x;\"\nfn main() {\n    let _value = old(1)\n    let _callback = old\n}";
    let found = findings(src);
    assert_eq!(found.len(), 2, "{found:?}");
}
