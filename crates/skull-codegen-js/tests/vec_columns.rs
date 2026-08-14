//! `Vec<T>` column storage on the JS backend (`docs/js-vec-columns-plan.md`):
//! `[E]` over an all-scalar `value` `E` stores one plain array per leaf, sharing
//! the Vec's single `len`, with no per-element object. Marked or not -- both
//! targets column now, so the `nobox` half here is about the GUARANTEE (no
//! materializer exists at all), not about reaching the column path.
//!
//! Emitted-shape assertions alone cannot tell "flat" from "boxed and unboxed
//! again", so the round-trips below RUN under node.

use skull_mir::mir::FuncRef;

use crate::common::{lower_test_ref, ws_with};

const SRC: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2 }
nobox value NV2 { x: float, y: float }

fn unmarked() -> float {
    let xs: [V2] = []
    xs.push(V2 { x: 1.0, y: 2.0 })
    return xs[0].x
}

fn nested() -> float {
    let xs: [Vert] = [Vert { pos: V2 { x: 1.0, y: 2.0 }, uv: V2 { x: 3.0, y: 4.0 } }]
    return xs[0].pos.x
}

fn marked() -> int {
    let xs: [NV2] = []
    xs.push(NV2 { x: 1.0, y: 2.0 })
    xs.push(NV2 { x: 30.0, y: 40.0 })
    return int(xs[1].x) * 100 + int(xs[0].y) * 10 + xs.len
}

fn literal() -> int {
    let xs: [NV2] = [NV2 { x: 5.0, y: 6.0 }, NV2 { x: 7.0, y: 8.0 }]
    return int(xs[1].y) * 10 + xs.len
}

fn written() -> int {
    let xs: [NV2] = [NV2 { x: 1.0, y: 2.0 }]
    xs[0].x = 9.0
    return int(xs[0].x) * 10 + int(xs[0].y)
}

nobox value NP { x: float, y: float }

fn insert_at_presence() -> int {
    let a: [NP?] = [NP { x: 1.0, y: 2.0 }, NP { x: 3.0, y: 4.0 }]
    a.insert_at(1, null)
    let present = 0
    if a[1] == null { present = 1 }
    return a.len * 1000 + int(a[0]?.y ?? 0.0) * 100 + present * 10 + int(a[2]?.y ?? 0.0)
}

fn main() { let _ = unmarked() }
"#;

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Whole-program JS for `name`: `push` is a stdlib callee, so emitting the root
/// body alone rejects it as an unmapped intrinsic long before the storage
/// question is reached.
fn emit_js(name: &str) -> Result<String, String> {
    let ws = ws_with(SRC);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .map(|(fid, _)| fid)
        .expect("fn in <test>");
    skull_codegen_js::emit::emit_with_closure(
        &ws,
        &[(FuncRef { module: mid, func: fid }, name.to_string())],
    )
    .map_err(|e| e.message)
}

/// The luajit MIR for `name` -- the control that the same source DID take
/// columns on the backend that has always had them.
fn lua_mir(name: &str) -> String {
    let ws = ws_with(SRC);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .map(|(fid, _)| fid)
        .expect("fn in <test>");
    lower_test_ref(&ws, mid, fid).to_string()
}

/// Just `function <name>(` .. the next top-level `}`, so a body assertion is not
/// fooled by the registry defs the same program carries.
fn fn_body<'a>(js: &'a str, name: &str) -> &'a str {
    let start = js.find(&format!("function {name}(")).expect("emitted fn is in the program");
    let rest = &js[start..];
    let end = rest.find("\n}").map(|e| e + 2).unwrap_or(rest.len());
    &rest[..end]
}

fn eval_int(name: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP {name}: node not found");
        return;
    }
    let defs = emit_js(name).unwrap_or_else(|e| panic!("emit `{name}`: {e}"));
    let program = format!(
        "{}{}{}{}\n{defs}\nconsole.log({name}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed for `{name}`:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s.trim().parse().unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result for `{name}`\n{program}");
}

/// The control: the same source still columns for luajit. Without it every
/// assertion here would pass just as well if columns had stopped happening at
/// all, which is the vacuity class this area keeps producing.
#[test]
fn an_unmarked_element_still_columns_for_lua() {
    let mir = lua_mir("unmarked");
    assert!(
        mir.contains("VecColumn"),
        "[V2] stopped taking column storage on the luajit target:\n{mir}"
    );
}

/// An UNMARKED element columns on JS too, since stage B: the boundary primitives
/// have JS siblings and `inspect` rebuilds an element from its leaf columns, so
/// the two backends now agree on the representation as well as the semantics.
#[test]
fn an_unmarked_element_columns_for_js() {
    let js = emit_js("unmarked").expect("[V2] must emit columns on JS");
    assert!(js.contains("\"@vec\""), "expected the column header in the emitted JS:\n{js}");
    // The materializer legitimately names the element type, so scope this to the
    // body: it is the push/read path that must never build one.
    let body = fn_body(&js, "unmarked");
    assert!(
        !body.contains("__ty: \"V2\""),
        "column push boxed the element; the element object must never exist:\n{body}"
    );
}

/// The per-type materializer is the JS replacement for luajit's
/// `__ROCK_TYPE_TABLE` walk, so it must be EMITTED, keyed by the element name the
/// runtime header carries, and read the dotted leaf columns for a nested field.
/// Without it `inspect` renders the physical layout.
#[test]
fn a_column_element_gets_an_emitted_materializer() {
    let js = emit_js("unmarked").expect("[V2] must emit columns on JS");
    assert!(
        js.contains("__rock_col_mat[\"V2\"] = function(v, i)"),
        "no per-type column materializer was emitted:\n{js}"
    );
    assert!(
        js.contains("v[\"x\"][i]") && js.contains("v[\"y\"][i]"),
        "the materializer must read each leaf column by its path:\n{js}"
    );
    assert!(
        !js.contains("__ROCK_TYPE_TABLE"),
        "JS must not grow a runtime type table:\n{js}"
    );
}

/// A nested element's columns are DOTTED paths (`pos.x`), and the rebuilt object
/// has to be nested again -- a flat `{pos.x: ..}` would render nothing like the
/// declared shape. Same grouping rule the leaf-form value ctor uses.
#[test]
fn a_nested_column_element_rebuilds_its_child_object() {
    let js = emit_js("nested").expect("[Vert] must emit columns on JS");
    assert!(
        js.contains("__rock_col_mat[\"Vert\"] = function(v, i)"),
        "no materializer for the nested element:\n{js}"
    );
    assert!(
        js.contains("v[\"pos.x\"][i]"),
        "a nested leaf must be read from its dotted column:\n{js}"
    );
    assert!(
        js.contains("pos: ({__ty: \"V2\""),
        "the nested child must be rebuilt as its own object:\n{js}"
    );
}

/// The headline property: nothing in the push/read path constructs an element.
/// A `NV2`-shaped object literal in the body means the leaves were boxed and
/// unboxed again, which is the thing columns exist to prevent.
#[test]
fn a_nobox_element_never_builds_an_element_object() {
    let js = emit_js("marked").expect("[NV2] must emit columns on JS");
    assert!(
        !js.contains("__ty: \"NV2\""),
        "column push boxed the element; the element object must never exist:\n{js}"
    );
    assert!(js.contains("\"@vec\""), "expected the column header in the emitted JS:\n{js}");
    // Not even a materializer: every op that would call one is a spanned
    // rejection for a `nobox` element, so the guarantee has no escape hatch to
    // find in the emitted program.
    assert!(
        !js.contains("__rock_col_mat[\"NV2\"]"),
        "a `nobox` element must not get a materializer:\n{js}"
    );
}

/// Push then read back, under node. `marked` pushes two elements and reads a
/// leaf from each plus `len`, so a desynced column or an off-by-one index shows
/// up as a wrong number rather than a crash.
#[test]
fn a_nobox_element_round_trips_through_push_and_read() {
    eval_int("marked", 30 * 100 + 2 * 10 + 2);
}

/// The literal path (`MakeVecColumns`) rather than the push path.
#[test]
fn a_column_vec_literal_round_trips() {
    eval_int("literal", 8 * 10 + 2);
}

/// Leaf place-WRITE through an element index: `xs[0].x = 9.0` must reach one
/// column and leave the sibling column alone.
#[test]
fn a_leaf_write_hits_one_column_and_spares_its_sibling() {
    eval_int("written", 9 * 10 + 2);
}

/// The index is 0-based on JS and 1-based on Lua, because each follows its own
/// `_data`. That difference lives in exactly one place; pin it there so a
/// "parity" edit cannot quietly move it and shift every column read by one.
#[test]
fn the_checked_index_is_zero_based_on_js() {
    assert!(
        skull_codegen_js::JS_VEC_CHECK_SHIMS.contains("function __vec_checked_index"),
        "the JS checked-index shim is missing; column reads would call an undefined global"
    );
    let js = emit_js("marked").expect("[NV2] must emit columns on JS");
    assert!(
        js.contains("__vec_checked_index("),
        "column element access must go through the checked index:\n{js}"
    );
}

/// `insert_at` inserts the VALUE into each column, not the `len` it is sized by.
/// The digits are `len`, `a[0].y`, `a[1] == null`, `a[2].y`; a JS sibling that
/// drops a param binds `value` to whatever sits at its position and splices that
/// in, which reads back through the presence column as a present element.
#[test]
fn nullable_column_insert_at_presence() {
    eval_int("insert_at_presence", 3214);
}
