use bone_lints::LintOptions;
use rock_core::errors::CompilerErrorLevel;

fn findings(src: &str) -> Vec<bone_lints::Finding> {
    crate::support::checked_lint(
        &[("<deprecated>", src)],
        &LintOptions::only(["deprecated".into()]),
    )
}

#[test]
fn honors_suppression_and_selection() {
    let src = "@deprecated\nfn old() {}\nfn main() {\n    // rock: allow(deprecated)\n    old()\n    old() // rock: allow(deprecated)\n}";
    assert!(findings(src).is_empty(), "{:?}", findings(src));
    assert!(crate::support::lint_only(&[("<deprecated>", src)], "unused_var").is_empty());
}

#[test]
fn backend_siblings_produce_one_warning_per_reference() {
    let src = "@deprecated(\"use fresh\")\ninline(lua) fn old(x: int) -> int = \"return x\"\n@deprecated(\"use fresh\")\ninline(js) fn old(x: int) -> int = \"return x;\"\nfn main() {\n    let _value = old(1)\n    let _callback = old\n}";
    let found = findings(src);
    assert_eq!(found.len(), 2, "{found:?}");
}
