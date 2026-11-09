//! inspect emission (T8d): `inspect`/`inspect_formatted` map to the
//! `__rock_inspect_value` runtime port (`JS_INSPECT_RUNTIME`) -- structs (sorted
//! keys, ` = ` sep, compact/multiline threshold), Vec, Map, arrays, scalars,
//! fn-records. A GC-FREE VALUE STRUCT argument renders colon-format cdata-faithful
//! (`Name { f: v }`, declaration field order) via an emit-time recursive renderer
//! (its fields are bounded to primitives + nested value structs). An enum stored
//! as a scalar tag box renders through a per-enum renderer that bakes in the
//! variant names. Node execution is the correctness oracle; `tests/strings/js_inspect.rock` cross-checks
//! against luajit.

use crate::common::ws_with;

// Emit `root`, run it under node with the string + equality + inspect runtimes, return the
// single string `root()` produces (via JSON so newlines survive).
fn eval_str(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, root).expect("inspect emits");
    if !crate::common::require_node() {
        return String::new();
    }
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

// A 2-field struct is compact, sorted, ` = `-separated.
#[test]
fn compact_struct_sorted() {
    let src = "struct Person { name: string, age: int }\n\
               fn root() -> string { inspect(Person { name: \"Alice\", age: 30 }) }";
    assert_eq!(
        eval_str(src, "root"),
        "Person { age = 30, name = \"Alice\" }"
    );
}

// A 3-field struct is multi-line.
#[test]
fn multiline_struct() {
    let src = "struct Triple { a: int, b: int, c: int }\n\
               fn root() -> string { inspect(Triple { a: 1, b: 2, c: 3 }) }";
    assert_eq!(
        eval_str(src, "root"),
        "Triple {\n  a = 1,\n  b = 2,\n  c = 3\n}"
    );
}

// A Vec renders in index order.
#[test]
fn vec_render() {
    let src = "fn root() -> string { inspect([10, 20, 30]) }";
    assert_eq!(eval_str(src, "root"), "Vec { 10, 20, 30 }");
}

// A scalar string is quoted.
#[test]
fn scalar_string() {
    let src = "fn root() -> string { inspect(\"hi\") }";
    assert_eq!(eval_str(src, "root"), "\"hi\"");
}

// A GC-free VALUE STRUCT renders colon-format, declaration field order (NOT
// sorted): matches the luajit cdata `__tostring` byte for byte.
#[test]
fn value_struct_colon_render() {
    let src = "value V2 { y: int, x: int }\n\
               fn root() -> string { inspect(V2 { y: 5, x: 3 }) }";
    // Declaration order is y then x (deliberately non-alphabetical).
    assert_eq!(eval_str(src, "root"), "V2 { y: 5, x: 3 }");
}

// A nested value struct recurses with the same colon rule.
#[test]
fn value_struct_nested_colon_render() {
    let src = "value IV { x: int, y: int }\n\
               value Nested { a: IV, b: int }\n\
               fn root() -> string { inspect(Nested { a: IV { x: 1, y: 2 }, b: 9 }) }";
    assert_eq!(
        eval_str(src, "root"),
        "Nested { a: IV { x: 1, y: 2 }, b: 9 }"
    );
}

// An embedded child is physically inline. Inspect must use the shared field
// plan to rebuild the logical child before the generic object renderer walks it.
#[test]
fn inspect_rebuilds_an_embed_field_from_inline_leaves() {
    let src = "struct Pt { x: int, y: int }\n\
               struct Holder { embed p: Pt, tag: int }\n\
               fn root() -> string { inspect(Holder { p: Pt { x: 1, y: 2 }, tag: 3 }) }";
    assert_eq!(
        eval_str(src, "root"),
        "Holder { p = Pt { x = 1, y = 2 }, tag = 3 }"
    );
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

// A payload-bearing enum scalarizes to a tag box (`MakeValueStruct`) whose
// numeric tag carries no variant name; the per-enum renderer supplies it, with
// record fields sorted as luajit's cdata `__tostring` renders them.
#[test]
fn scalar_enum_box_renders_its_variant() {
    let src = "enum Color { Red, Custom { r: int, g: int } }\n\
               fn root() -> string { inspect(Color::Custom { r: 1, g: 2 }) }";
    assert_eq!(eval_str(src, "root"), "Color::Custom { g = 2, r = 1 }");
}
