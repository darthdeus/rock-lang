//! GC-tier `value` construction spliced at the use site.
//!
//! `<Name>_vt(a, b)` is a call to a chunk-level function whose entire body is
//! `return setmetatable({ a = a, b = b, __ty = "Name" }, __rock_vt_Name)`. That
//! call frame is what lets LuaJIT root a trace at the constructor: `Entity_vt`
//! is the largest single `leaving loop in root trace` family on mag's combat
//! bench (106 aborts over three runs), and its `return setmetatable(..)` is one
//! `CALLT` against the 15-slot `loopunroll` budget in every trace that records
//! it. `ROCK_SKULL_VALUE_CTOR=splice` writes the body at the use site instead,
//! so there is no frame to root.
//!
//! The shape comes from `emit_full_with_value_ctor`, not the environment:
//! `ROCK_SKULL_VALUE_CTOR` is read once per PROCESS and these tests share a
//! binary with every other skull test. The runtime half -- a value type in a
//! record, equality across shapes, field evaluation order -- is
//! `tests/value_ctor_splice.rock`, which the `rock test tests/` gate runs under
//! both shapes.

use skull::mir_emit::emit_full_with_value_ctor;
use skull_codegen_lua::ValueCtorShape;

use crate::common::ws_with_stdlib;

/// `Bag` carries a `[int]`, so it is GC-carrying and materializes as a table
/// (`_vt`). `Pt` is FFI-clean, so it materializes as cdata (`_ct`) and must be
/// left alone: `ffi.typeof` has no Lua body to splice. Both are returned through
/// a nullable, which keeps the boxed ABI and forces materialization.
const SRC: &str = "\
value Bag { xs: [int], n: int }
value Pt { x: float, y: float }

fn find_bag(want: int) -> Bag? {
    if want == 0 { return null }
    return Bag { xs: [want, want * 2], n: want }
}

fn find_pt(want: int) -> Pt? {
    if want == 0 { return null }
    return Pt { x: 1.0, y: 2.0 }
}

fn main() {
    let b = find_bag(3)
    let p = find_pt(3)
    if b != null { print(b.n) }
    if p != null { print(p.y) }
}
";

fn emit(shape: ValueCtorShape) -> String {
    let ws = ws_with_stdlib(SRC);
    let mid = skull::user_module_id(&ws).unwrap();
    emit_full_with_value_ctor(&ws, mid, shape).expect("emit_full").0.lua
}

/// Lines carrying a construction, not the `<Name>_vt` definition itself.
fn construction_sites(lua: &str, needle: &str) -> usize {
    lua.lines().filter(|l| l.contains(needle) && !l.contains("_vt = function(")).count()
}

#[test]
fn splice_replaces_every_gc_table_construction_and_leaves_the_cdata_tier_alone() {
    let call = emit(ValueCtorShape::Call);
    let splice = emit(ValueCtorShape::Splice);

    assert!(
        construction_sites(&call, "Bag_vt(") > 0,
        "fixture builds no GC-table value; the test would prove nothing:\n{call}"
    );
    assert_eq!(
        construction_sites(&splice, "Bag_vt("),
        0,
        "a GC-table construction is still a call under the spliced shape"
    );
    assert!(
        splice.contains("Bag_vt = function("),
        "the ctor DEFINITION must survive: the const-hoist path still calls it, and it is \
         the reflection surface for the type"
    );
    assert!(
        splice.contains(r#"setmetatable({ xs = "#) && splice.contains(r#"__ty = "Bag" }, __rock_vt_Bag)"#),
        "the spliced site must build the same table with the same shared metatable:\n{splice}"
    );

    // The cdata tier is an `ffi.typeof` ctor with no Lua body, so the switch must
    // not touch it.
    assert_eq!(
        construction_sites(&call, "Pt_ct("),
        construction_sites(&splice, "Pt_ct("),
        "the cdata tier changed under the value-ctor switch"
    );
}

/// The switch touches CONSTRUCTION SITES and nothing else: the two artifacts
/// have the same lines everywhere a construction does not appear.
#[test]
fn the_two_shapes_differ_only_where_a_gc_table_value_is_built() {
    let call = emit(ValueCtorShape::Call);
    let splice = emit(ValueCtorShape::Splice);
    let (call_lines, splice_lines): (Vec<&str>, Vec<&str>) =
        (call.lines().collect(), splice.lines().collect());
    assert_eq!(
        call_lines.len(),
        splice_lines.len(),
        "the spliced shape changed the line count; it is an expression rewrite"
    );
    let mut differing = 0usize;
    for (index, (a, b)) in call_lines.iter().zip(splice_lines.iter()).enumerate() {
        if a == b {
            continue;
        }
        differing += 1;
        assert!(
            a.contains("_vt(") && b.contains("setmetatable({"),
            "line {} differs for a reason other than a spliced construction:\n  call:   {a}\n  splice: {b}",
            index + 1
        );
    }
    assert!(differing > 0, "the two shapes emitted identical text");
}
