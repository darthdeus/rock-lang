//! `inspect` of a value an opaque `inline(js)` body built: the renderer must
//! read the object it is given, whether or not the program also constructs the
//! same type itself. Exact renders of program-built values are
//! tests/strings/js_inspect.rock, which runs on both backends.

use crate::common::ws_with;

// Emit `root`, run it under node with the string + equality + inspect runtimes, return the
// single string `root()` produces (via JSON so newlines survive).
fn eval_str(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, root).expect("inspect emits");
    let program = format!(
        "{}{}{}{}\n{js}\nprocess.stdout.write(\"<<<\" + {root}() + \">>>\");",
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_INSPECT_RUNTIME,
    );
    let s = crate::common::run_node(&program);
    s.trim()
        .trim_start_matches("<<<")
        .trim_end_matches(">>>")
        .to_string()
}

#[test]
fn direct_child_inspect_is_independent_of_an_unrelated_constructor() {
    const PREFIX: &str = r#"
value Mask { r: bool, g: bool? }
struct Draw { mask: Mask }
struct Traced { draw: Draw }

inline(js) fn foreign_draw() -> Draw = """
return { __ty: "Draw", mask: { __ty: "Mask", r: true } };
"""
"#;
    let without_constructor =
        format!("{PREFIX}\nfn root() -> string {{ inspect(Traced {{ draw: foreign_draw() }}) }}");
    let with_constructor = format!(
        "{PREFIX}\n\
         fn local_draw() -> Draw {{ Draw {{ mask: Mask {{ r: false, g: null }} }} }}\n\
         fn root() -> string {{ let _ = local_draw(); inspect(Traced {{ draw: foreign_draw() }}) }}"
    );
    let direct_root = format!("{PREFIX}\nfn root() -> string {{ inspect(foreign_draw()) }}");
    let expected = "Traced { draw = Draw { mask = Mask { g = nil, r = true } } }";
    assert_eq!(eval_str(&without_constructor, "root"), expected);
    assert_eq!(eval_str(&with_constructor, "root"), expected);
    assert_eq!(
        eval_str(&direct_root, "root"),
        "Draw { mask = Mask { g = nil, r = true } }"
    );
}
