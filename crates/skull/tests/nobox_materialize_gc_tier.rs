//! `materialize(v)` of a `nobox` value struct builds the ORDINARY box for that
//! struct's tier, whatever the tier is. Any string, closure, array or flat-enum
//! field puts the struct at the GC-carrying tier, where the box is the value
//! table (`<Name>_vt`, shared metatable, declared keys) -- exactly what a plain
//! `value` struct with the same fields builds, so inline Lua, reflection and
//! serialization see an ordinary value.
//!
//! This file was `nobox_materialize_gc_tier_sentinel.rs`, which pinned the
//! rejection. Two mechanisms produced it and both are fixed here:
//! `flatten_nobox_nested_ctors` demanded CData storage of every ctor a `nobox`
//! name triggered on, and `materialize_repr` was a second recursion that had
//! never grown `pack`'s flat-enum and leaf-table arms.
//!
//! The mark only ever widened the EXPLICIT box. An implicit one is still
//! rejected at the sink -- `an_implicit_box_of_a_gc_tier_value_is_still_rejected`
//! is the control, and it fails if the fix leaked into the implicit path.

use crate::common::{emit_module, function_body, run_main, try_emit_module};

/// A string field. The minimal GC-tier cell.
const STRING_FIELD: &str = r#"
nobox value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(materialize(v)))
}
"#;

/// The same fields WITHOUT the mark. The two boxes must be the same shape: that
/// is what "the explicit box is an ordinary value" means, and a value assertion
/// alone cannot tell a `_vt` table from anything else that answers `.kind`.
const STRING_FIELD_UNMARKED: &str = r#"
value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(v))
}
"#;

const ARRAY_FIELD: &str = r#"
nobox value ARow { id: int, xs: [int] }
inline(lua) fn show(v: any) -> string = "return tostring(v.id) .. ':' .. tostring(#v.xs._data) .. ':' .. tostring(v.xs._data[2])"
fn main() {
    let v = ARow { id: 1, xs: [10, 20, 30] }
    print(show(materialize(v)))
}
"#;

/// The LEAF-TABLE tier, the third storage a value struct can select: a nullable
/// nobox field makes the outer GC-carrying but keeps it scalarizable, so its box
/// is the dotted-key `<Name>_lct` table. Packing its DECLARED fields instead
/// builds a one-argument call against a four-leaf ctor, which codegen rejects as
/// "declared-form constructor selects a leaf-table plan".
const LEAF_TABLE_TIER: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Opt { pos: NV2?, a: float }
inline(lua) fn show(v: any) -> string = "return tostring(v['pos']) .. ':' .. tostring(v['pos.x']) .. ':' .. tostring(v['pos.y']) .. ':' .. tostring(v.a)"
fn main() {
    let o = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0 }
    print(show(materialize(o)))
}
"#;

/// The control. Same struct, same sink, no `materialize` -- still rejected, at
/// the argument's own span.
const IMPLICIT_BOX: &str = r#"
nobox value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "return tostring(v.kind) .. v.label"
fn main() {
    let v = SRow { kind: 1, label: "a" }
    print(show(v))
}
"#;

/// A `nobox` field of a GC-tier parent stored in a global. The parent's box
/// keys the field's leaves by dotted path, so no box of `NV2` exists to report.
/// This pinned a rejection while the GC tier keyed every field whole.
const NOBOX_FIELD_UNDER_A_GC_TIER_PARENT: &str = r#"
nobox value NV2 { x: float, y: float }
value Outer { label: string, pos: NV2 }
global G: Outer
fn setup() { G = Outer { label: "L", pos: NV2 { x: 1.0, y: 2.0 } } }
fn main() {
    setup()
    print("{G.pos.x} {G.label}")
}
"#;

/// The CDATA tier, with a NESTED field: the explicit box flattens to one
/// `<Name>_lct` allocation rather than building an inner cdata per field. This
/// is the pass's second iteration -- the rewritten ctor keys are `lo.x`, not
/// `lo`, so a re-resolution of the dotted path against them cannot work.
const NESTED_CDATA_TIER: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Box2 { lo: NV2, hi: NV2 }
inline(lua) fn show(v: any) -> string = "return tostring(v.lo.x) .. ' ' .. tostring(v.hi.y)"
fn main() {
    let b = Box2 { lo: NV2 { x: 1.0, y: 2.0 }, hi: NV2 { x: 3.0, y: 4.0 } }
    print(show(materialize(b)))
}
"#;

#[test]
fn a_string_field_boxes_through_the_value_table() {
    let lua = emit_module(STRING_FIELD);
    assert!(
        function_body(&lua, "main").contains("SRow_vt("),
        "the explicit box did not take the value-table ctor:\n{lua}"
    );
    assert_eq!(run_main(&lua), "1a");
}

/// The mark is the only variable, so the emitted constructor must be the same
/// one. Read off `main` rather than restated here: a copy of the naming rule
/// would keep agreeing with itself after the rule changed.
#[test]
fn the_marked_and_unmarked_boxes_are_the_same_shape() {
    let marked = emit_module(STRING_FIELD);
    let plain = emit_module(STRING_FIELD_UNMARKED);
    let ctor = |lua: &str| -> String {
        let body = function_body(lua, "main");
        let at = body.find("SRow_").expect("a SRow constructor in main");
        body[at..]
            .split('(')
            .next()
            .expect("a constructor name")
            .to_string()
    };
    assert_eq!(
        ctor(&marked),
        ctor(&plain),
        "the explicit box selected a different constructor than the unmarked \
         struct's:\nmarked:\n{marked}\nplain:\n{plain}"
    );
    assert_eq!(run_main(&plain), run_main(&marked),);
}

#[test]
fn an_array_field_boxes_through_the_value_table() {
    let lua = emit_module(ARRAY_FIELD);
    assert!(
        function_body(&lua, "main").contains("ARow_vt("),
        "the explicit box did not take the value-table ctor:\n{lua}"
    );
    assert_eq!(run_main(&lua), "1:3:20");
}

#[test]
fn a_leaf_table_value_boxes_through_its_dotted_key_constructor() {
    let lua = emit_module(LEAF_TABLE_TIER);
    assert!(
        function_body(&lua, "main").contains("Opt_lct("),
        "the explicit box did not take the leaf-table ctor:\n{lua}"
    );
    assert_eq!(run_main(&lua), "true:1:2:3");
}

#[test]
fn a_nested_cdata_value_boxes_as_one_leaf_constructor() {
    let lua = emit_module(NESTED_CDATA_TIER);
    let main = function_body(&lua, "main");
    assert!(
        main.contains("Box2_lct("),
        "the explicit box did not flatten to the leaf ctor:\n{lua}"
    );
    assert!(
        !main.contains("NV2_ct("),
        "the explicit box built an intermediate cdata per nested field:\n{lua}"
    );
    assert_eq!(run_main(&lua), "1 4");
}

#[test]
fn an_implicit_box_of_a_gc_tier_value_is_still_rejected() {
    let err = try_emit_module(IMPLICIT_BOX)
        .expect_err("an implicit box of a GC-tier nobox value must still be rejected");
    assert!(
        err.message.contains("cannot cross boxed call boundary"),
        "the implicit box was rejected for an unrelated reason: {err:?}"
    );
    assert!(
        err.location.line > 0,
        "the rejection lost its span: {err:?}"
    );
}

#[test]
fn a_nobox_field_under_a_gc_tier_parent_stores_as_leaves() {
    let lua = emit_module(NOBOX_FIELD_UNDER_A_GC_TIER_PARENT);
    let setup = function_body(&lua, "setup");
    assert!(
        !setup.contains("NV2_ct("),
        "the stored parent boxed its nobox field:\n{setup}"
    );
    assert_eq!(run_main(&lua), "1 L");
}

#[test]
fn hoisted_closure_does_not_validate_discarded_parent_blocks() {
    let source = r#"
nobox value Tint { r: float, g: float, b: float, a: float }
value Row { key: string, count: int, color: Tint }
fn row(index: int) -> Row {
    return Row { key: "item", count: index, color: Tint { r: 1.0, g: 2.0, b: 3.0, a: 4.0 } }
}
fn with_check(check: fn() -> ()) { check() }
fn main() {
    let offset = 7
    with_check(() => {
        with_check(() => {
            let total = offset
            for index in 0..3 {
                let item = row(index)
                total += item.count
            }
            print(total)
        })
    })
}
"#;
    let lua = try_emit_module(source).expect("a scalarized closure local must emit");
    assert_eq!(run_main(&lua), "10");
}

// The live-body tests need a box that is still real: a tuple holding a `nobox`
// value, interpolated whole, is the permanent sink (`nobox_values.rs`,
// `NOBOX_TWO_SINKS_IN_ONE_BODY`). A GC-tier parent stored in a global was the
// sink until its box keyed `nobox` fields as leaves.
#[test]
fn live_implicit_materialization_in_a_hoisted_closure_is_rejected() {
    let source = r#"
nobox value NV2 { x: float, y: float }
fn with_check(check: fn() -> ()) { check() }
fn main() {
    with_check(() => {
        let t = (NV2 { x: 1.0, y: 2.0 }, 1.0)
        print("{t}")
    })
}
"#;
    let err = try_emit_module(source).expect_err("live closure boxes must still reject");
    assert!(
        err.message
            .contains("survived lowering as an implicit materialization"),
        "{err:?}"
    );
    assert!(
        err.message.contains("lambda"),
        "the rejection must name the live body: {err:?}"
    );
    assert!(
        err.location.line > 0,
        "the rejection lost its span: {err:?}"
    );
}

#[test]
fn live_implicit_materialization_in_a_coroutine_hook_is_rejected() {
    let source = r#"
nobox value NV2 { x: float, y: float }
co fn worker() -> Coroutine<int> {
    on_killed(() => {
        let t = (NV2 { x: 1.0, y: 2.0 }, 1.0)
        print("{t}")
    })
    yield 1
}
fn main() {
    let c = worker()
    let _ = c.resume()
    c.kill()
}
"#;
    let err = try_emit_module(source).expect_err("live coroutine hook boxes must still reject");
    assert!(
        err.message
            .contains("survived lowering as an implicit materialization"),
        "{err:?}"
    );
    assert!(
        err.location.line > 0,
        "the rejection lost its span: {err:?}"
    );
}
