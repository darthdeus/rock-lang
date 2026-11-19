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
fn unread_bindings_are_flagged() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "flags_an_unused_let",
            "fn f() -> int {\n    let x = 5\n    3\n}",
            "x",
        ),
        (
            "write_only_local_is_flagged",
            "fn f() -> int {\n    let y = 5\n    y = 6\n    3\n}",
            "y",
        ),
        (
            "unused_for_loop_var_is_flagged",
            "fn f() -> int {\n    for i in 0..3 {\n    }\n    3\n}",
            "i",
        ),
        (
            "destructure_flags_only_the_unused_leg",
            "fn f() -> int {\n    let (a, b) = (1, 2)\n    a\n}",
            "b",
        ),
        (
            "unused_param_is_flagged",
            "fn f(a: int, b: int) -> int { a }",
            "b",
        ),
        (
            "ordinary_decorator_param_still_requires_a_body_read",
            "deco init tagged(value: int) {}",
            "value",
        ),
        (
            "unused_match_payload_bind_is_flagged",
            "enum Opt {\n    Some(int),\n    None,\n}\n\
               fn f(o: Opt) -> int {\n    \
               match o {\n        Some(n) => 5,\n        None => 0,\n    }\n}",
            "n",
        ),
        (
            "unused_if_let_bind_is_flagged",
            "fn f(o: int?) -> int {\n    if let x = o {\n        7\n    } else {\n        0\n    }\n}",
            "x",
        ),
    ];
    for (name, src, unused) in cases {
        assert_eq!(names(src), [*unused], "{name}");
    }
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
