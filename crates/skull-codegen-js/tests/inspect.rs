//! inspect emission (T8d): `inspect`/`inspect_formatted` map to the
//! `__rock_inspect_value` runtime port (`JS_INSPECT_RUNTIME`) -- structs (sorted
//! keys, ` = ` sep, compact/multiline threshold), Vec, Map, arrays, scalars,
//! fn-records. A GC-FREE VALUE STRUCT argument renders colon-format cdata-faithful
//! (`Name { f: v }`, declaration field order) via an emit-time recursive renderer
//! (its fields are bounded to primitives + nested value structs). An ENUM argument
//! still REJECTS (the scalarized box cannot reproduce the named variant). Node
//! execution is the correctness oracle; `tests/js_inspect.rock` cross-checks
//! against luajit.

use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

// Emit `root`, run it under node with the string + inspect runtimes, return the
// single string `root()` produces (via JSON so newlines survive).
fn eval_str(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    let js = emit_closure(&ws, root).expect("inspect emits");
    if !node_available() {
        eprintln!("SKIP: node not found");
        return String::new();
    }
    let program = format!(
        "{}{}{}\n{js}\nprocess.stdout.write(\"<<<\" + {root}() + \">>>\");",
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
        skull_codegen_js::JS_INSPECT_RUNTIME,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().trim_start_matches("<<<").trim_end_matches(">>>").to_string()
}

// A 2-field struct is compact, sorted, ` = `-separated.
#[test]
fn compact_struct_sorted() {
    let src = "struct Person { name: string, age: int }\n\
               fn root() -> string { inspect(Person { name: \"Alice\", age: 30 }) }";
    assert_eq!(eval_str(src, "root"), "Person { age = 30, name = \"Alice\" }");
}

// A 3-field struct is multi-line.
#[test]
fn multiline_struct() {
    let src = "struct Triple { a: int, b: int, c: int }\n\
               fn root() -> string { inspect(Triple { a: 1, b: 2, c: 3 }) }";
    assert_eq!(eval_str(src, "root"), "Triple {\n  a = 1,\n  b = 2,\n  c = 3\n}");
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
    assert_eq!(eval_str(src, "root"), "Nested { a: IV { x: 1, y: 2 }, b: 9 }");
}

// An embedded child is physically inline. Inspect must use the shared field
// plan to rebuild the logical child before the generic object renderer walks it.
#[test]
fn inspect_rebuilds_an_embed_field_from_inline_leaves() {
    let src = "struct Pt { x: int, y: int }\n\
               struct Holder { embed p: Pt, tag: int }\n\
               fn root() -> string { inspect(Holder { p: Pt { x: 1, y: 2 }, tag: 3 }) }";
    assert_eq!(eval_str(src, "root"), "Holder { p = Pt { x = 1, y = 2 }, tag = 3 }");
}

// An ENUM argument rejects. A payload-bearing enum scalarizes to the
// value-struct-like shape (`MakeValueStruct`), so it is caught by the same box
// gate -- either message is a correct honest reject (the numeric tag / colon
// render cannot be reproduced without the enum/value-struct repr work).
#[test]
fn enum_arg_rejects() {
    let src = "enum Color { Red, Custom { r: int, g: int } }\n\
               fn root() -> string { inspect(Color::Custom { r: 1, g: 2 }) }";
    let err = emit_closure(&ws_with(src), "root").unwrap_err();
    assert!(
        err.contains("enum") || err.contains("value struct"),
        "reject reason: {err}"
    );
}
