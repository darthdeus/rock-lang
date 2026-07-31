//! GC-carrying value-struct equality on JS (T8g). A `value` struct with a
//! non-C-mappable field (Vec, string, generic `T`, a reference struct, a nested
//! value struct) is GC-CARRYING: it emits as a plain `{__ty, ...}` object (same
//! as a GC-free value struct) but its per-field eq split -- SCALAR / nested-value
//! fields structural, GC-REF fields BY REFERENCE -- cannot be reproduced by the
//! generic contents walk. `build_vt_eq_defs` emits a `__rock_vt_eq["<N>"]` entry
//! per referenced type mirroring the Lua `_vt.__eq`; `__op_eq` consults it before
//! the generic key walk (luajit's metatable-authority rule).
//!
//! The node-executed checks are the correctness oracle; the load-bearing case is
//! `diff_ref_equal_contents_ne` -- equal-CONTENTS but different-identity Vec
//! fields compare != (the divergence from a reference struct's contents-walk).

use std::process::Command;

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let scx = ScalarCtx::new(ws);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn preamble() -> String {
    format!(
        "{}{}{}{}{}",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_ENUM_RUNTIME,
    ) + skull_codegen_js::JS_STRING_RUNTIME
}

fn eval_int(src: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let ws = ws_with(src);
    let js = emit_closure(&ws, root).unwrap_or_else(|e| panic!("emit `{root}`: {e}"));
    let program = format!("{}\n{js}\nconsole.log({root}());", preamble());
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let n: i64 = got.parse().unwrap_or_else(|_| panic!("not an int: {got:?}"));
    assert_eq!(n, expect, "wrong result for `{root}`");
}

const DECLS: &str = "value VecBox { xs: [int], n: int }\n\
    value SBox { s: string, n: int }\n\
    struct Ref { v: int }\n\
    value HasRef { r: Ref, n: int }\n\
    value Pair<T> { a: T, b: T }\n\
    fn mk_pair(x: int, y: int) -> Pair<int> { return Pair { a: x, b: y } }\n";

// Same GC-ref identity + equal scalar -> equal.
#[test]
fn same_ref_vec_field_eq() {
    let src = format!(
        "{DECLS}fn root() -> int {{\n\
             let s = [1, 2, 3]\n\
             let a = VecBox {{ xs: s, n: 5 }}\n\
             let b = VecBox {{ xs: s, n: 5 }}\n\
             if a == b {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    eval_int(&src, "root", 1);
}

// THE DIVERGENCE: equal CONTENTS, different Vec identity -> NOT equal. A generic
// contents walk would call these equal (green-but-wrong); the by-reference `===`
// on the Vec field is what makes this correct.
#[test]
fn diff_ref_equal_contents_ne() {
    let src = format!(
        "{DECLS}fn root() -> int {{\n\
             let a = VecBox {{ xs: [1, 2, 3], n: 5 }}\n\
             let b = VecBox {{ xs: [1, 2, 3], n: 5 }}\n\
             if a != b {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    eval_int(&src, "root", 1);
}

// Same Vec ref but differing scalar -> not equal (the scalar field is structural).
#[test]
fn same_ref_diff_scalar_ne() {
    let src = format!(
        "{DECLS}fn root() -> int {{\n\
             let s = [1, 2, 3]\n\
             let a = VecBox {{ xs: s, n: 5 }}\n\
             let b = VecBox {{ xs: s, n: 6 }}\n\
             if a != b {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    eval_int(&src, "root", 1);
}

// A string field compares BY VALUE (JS `===` on strings; luajit interning).
#[test]
fn string_field_by_value_eq() {
    let src = format!(
        "{DECLS}fn root() -> int {{\n\
             let w = \"wor\"\n\
             let a = SBox {{ s: \"{{w}}ld\", n: 1 }}\n\
             let b = SBox {{ s: \"world\", n: 1 }}\n\
             if a == b {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    eval_int(&src, "root", 1);
}

// A reference-struct field compares BY REFERENCE (the `rawequal`/`===` arm): a
// shared `Ref` is equal, an equal-contents but distinct `Ref` is NOT.
#[test]
fn ref_struct_field_by_reference() {
    let src = format!(
        "{DECLS}fn root() -> int {{\n\
             let sh = Ref {{ v: 7 }}\n\
             let a = HasRef {{ r: sh, n: 1 }}\n\
             let b = HasRef {{ r: sh, n: 1 }}\n\
             let c = HasRef {{ r: Ref {{ v: 7 }}, n: 1 }}\n\
             if a == b && a != c {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    eval_int(&src, "root", 1);
}

// A generic value struct (generic => GC-tier); `int` payloads compare correctly
// through the by-reference arm (`===` is value-eq for numbers).
#[test]
fn generic_pair_eq() {
    let src = format!(
        "{DECLS}fn root() -> int {{\n\
             let a = mk_pair(3, 4)\n\
             let b = mk_pair(3, 4)\n\
             let c = mk_pair(3, 5)\n\
             if a == b && a != c {{ 1 }} else {{ 0 }}\n\
         }}"
    );
    eval_int(&src, "root", 1);
}

// The registry entry mirrors the Lua `_vt.__eq` field-for-field: a Vec field is
// `===` (reference), a scalar field is `__op_eq` (structural). Generic `Pair`
// keys on the BARE post-mono name (`Pair`), and its generic fields are by
// reference.
#[test]
fn registry_entries_emitted() {
    let src = format!("{DECLS}fn root() -> int {{ let a = VecBox {{ xs: [1], n: 2 }}\n\
        let p = mk_pair(1, 2)\n if a == a && p == p {{ 1 }} else {{ 0 }} }}");
    let ws = ws_with(&src);
    let js = emit_closure(&ws, "root").expect("emits");
    assert!(
        js.contains("__rock_vt_eq[\"VecBox\"] = function(a, b) { return a.xs === b.xs && __op_eq(a.n, b.n); };"),
        "VecBox registry entry (Vec ref, scalar structural):\n{js}"
    );
    assert!(
        js.contains("__rock_vt_eq[\"Pair\"] = function(a, b) { return a.a === b.a && a.b === b.b; };"),
        "generic Pair keys on the bare name, generic fields by reference:\n{js}"
    );
}
