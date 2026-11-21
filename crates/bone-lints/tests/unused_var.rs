use crate::support::{lint_only, quoted};

fn names(src: &str) -> Vec<String> {
    let mut ns: Vec<String> = lint_only(&[("<unused_var>", src)], bone_lints::UNUSED_VAR)
        .iter()
        .map(quoted)
        .collect();
    ns.sort();
    ns
}

#[test]
fn used_or_exempt_bindings_are_not_flagged() {
    let cases: &[&str] = &[
        "enum Opt { Some { value: int } }\nfn f() -> Opt { let n = 1; Opt::Some { value: n } }",
        "fn f() -> int {\n    let x = 5\n    x\n}",
        "fn f() -> int {\n    let _x = 5\n    3\n}",
        "fn f() -> int {\n    let x = 0\n    x += 1\n    3\n}",
        // artifact-definition metadata consumes `key`
        r#"
@artifact_definition(catalog="widgets", key_param="key")
deco init widget(key: string) {}
"#,
        // lifecycle metadata consumes `stage`
        r#"
@lifecycle(phase="update", owner="process", stage_param="stage")
deco init update(stage: int = 100) {}
"#,
        "fn f(a: int) -> int { a }",
        "fn f(_a: int, b: int) -> int { b }",
        // Inline and extern parameters are used by the target code.
        "inline(lua) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"",
        "extern(lua) fn c_add(a: int, b: int) -> int",
        // Only the enclosing function's own parameters are candidates.
        "fn f() -> int {\n    let g = (a: int) => 1\n    g(5)\n}",
        // The bare `None` arm lowers to a Let binding.
        "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => n,\n        None => 0,\n    }\n}",
    ];
    for src in cases {
        assert!(names(src).is_empty(), "{src}: {:?}", names(src));
    }
}
