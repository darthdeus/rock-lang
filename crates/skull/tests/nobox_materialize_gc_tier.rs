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

use crate::common::{function_body, run_luajit, try_emit_module};

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

const CLOSURE_FIELD: &str = r#"
nobox value CRow { id: int, f: fn(int) -> int }
inline(lua) fn show(v: any) -> string = "return tostring(v.id) .. ':' .. tostring(v.f(4))"
fn twice(n: int) -> int { return n * 2 }
fn main() {
    let v = CRow { id: 2, f: twice }
    print(show(materialize(v)))
}
"#;

/// A flat enum -- `Say` carries a `string`, so the enum is neither payload-free
/// (an int) nor cdata-eligible. Its part of the value `Group` is the flattened
/// `[tag, _0]` cell list, not a struct's declared fields, which is the arm the
/// second recursion never had. Both variant kinds are covered: a UNIT variant
/// nil-pads the payload slot, a PAYLOAD variant fills it.
const FLAT_ENUM_FIELD: &str = r#"
enum Note { Silent, Say(string) }
nobox value NRow { id: int, note: Note }
inline(lua) fn show(v: any) -> string = "return tostring(v.id) .. ':' .. tostring(v.note.__variant) .. ':' .. tostring(v.note[1])"
fn main() {
    let a = NRow { id: 1, note: Note::Silent }
    let b = NRow { id: 2, note: Note::Say("hi") }
    print(show(materialize(a)))
    print(show(materialize(b)))
}
"#;

/// A nested `nobox` struct inside a GC-tier outer. The outer's `pos` key holds a
/// value, so the inner cdata is built -- the same thing an unmarked outer does
/// with an unmarked inner. The explicit request authorizes the boxes its own box
/// is made of, and nothing beyond them.
const NESTED_NOBOX_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
nobox value Outer { label: string, pos: NV2 }
inline(lua) fn show(v: any) -> string = "return v.label .. ':' .. tostring(v.pos.x) .. ':' .. tostring(v.pos.y)"
fn main() {
    let v = Outer { label: "L", pos: NV2 { x: 1.0, y: 2.0 } }
    print(show(materialize(v)))
}
"#;

/// The `present: Some(..)` branch: an ABSENT value has no leaves to build a ctor
/// from, so it reaches the sink as `nil` rather than as a table of nils.
const NULLABLE_GC_TIER: &str = r#"
nobox value SRow { kind: int, label: string }
inline(lua) fn show(v: any) -> string = "if v == nil then return 'absent' end return tostring(v.kind) .. v.label"
fn pick(on: bool) -> SRow? {
    if on { return SRow { kind: 7, label: "z" } }
    return null
}
fn main() {
    print(show(materialize(pick(true))))
    print(show(materialize(pick(false))))
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

/// An implicit box one level down, inside a GC-tier parent the guarantee does
/// not cover. The parent's `pos` key needs a value and flattening cannot reach
/// it, so the box is real and must be reported -- with a span, at the ctor.
const IMPLICIT_BOX_UNDER_A_GC_TIER_PARENT: &str = r#"
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

fn emit_ok(src: &str, what: &str) -> String {
    match try_emit_module(src) {
        Ok(lua) => lua,
        Err(e) => panic!("{what} must emit, got a rejection: {e:?}"),
    }
}

fn run(src: &str, what: &str, tag: &str) -> String {
    let lua = emit_ok(src, what);
    run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn a_string_field_boxes_through_the_value_table() {
    let lua = emit_ok(STRING_FIELD, "a GC-tier nobox materialize");
    assert!(
        function_body(&lua, "main").contains("SRow_vt("),
        "the explicit box did not take the value-table ctor:\n{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "gc_tier_string"), "1a");
}

/// The mark is the only variable, so the emitted constructor must be the same
/// one. Read off `main` rather than restated here: a copy of the naming rule
/// would keep agreeing with itself after the rule changed.
#[test]
fn the_marked_and_unmarked_boxes_are_the_same_shape() {
    let marked = emit_ok(STRING_FIELD, "a GC-tier nobox materialize");
    let plain = emit_ok(STRING_FIELD_UNMARKED, "the unmarked twin");
    let ctor = |lua: &str| -> String {
        let body = function_body(lua, "main");
        let at = body.find("SRow_").expect("a SRow constructor in main");
        body[at..].split('(').next().expect("a constructor name").to_string()
    };
    assert_eq!(
        ctor(&marked),
        ctor(&plain),
        "the explicit box selected a different constructor than the unmarked \
         struct's:\nmarked:\n{marked}\nplain:\n{plain}"
    );
    assert_eq!(
        run_luajit(&format!("{plain}\nmain()\n"), "gc_tier_plain"),
        run_luajit(&format!("{marked}\nmain()\n"), "gc_tier_marked_twin"),
    );
}

#[test]
fn an_array_field_boxes_through_the_value_table() {
    let lua = emit_ok(ARRAY_FIELD, "an array field at the GC tier");
    assert!(
        function_body(&lua, "main").contains("ARow_vt("),
        "the explicit box did not take the value-table ctor:\n{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "gc_tier_array"), "1:3:20");
}

#[test]
fn a_closure_field_boxes_through_the_value_table() {
    assert_eq!(run(CLOSURE_FIELD, "a closure field at the GC tier", "gc_tier_closure"), "2:8");
}

#[test]
fn a_flat_enum_field_boxes_both_variant_kinds() {
    assert_eq!(
        run(FLAT_ENUM_FIELD, "a flat-enum field at the GC tier", "gc_tier_flat_enum"),
        "1:Silent:nil\n2:Say:hi"
    );
}

#[test]
fn a_nested_nobox_field_boxes_inside_a_gc_tier_outer() {
    assert_eq!(
        run(NESTED_NOBOX_FIELD, "a nested nobox field at the GC tier", "gc_tier_nested"),
        "L:1:2"
    );
}

#[test]
fn a_nullable_gc_tier_value_reaches_the_sink_as_nil_when_absent() {
    assert_eq!(
        run(NULLABLE_GC_TIER, "a nullable GC-tier nobox value", "gc_tier_nullable"),
        "7z\nabsent"
    );
}

#[test]
fn a_leaf_table_value_boxes_through_its_dotted_key_constructor() {
    let lua = emit_ok(LEAF_TABLE_TIER, "a leaf-table nobox materialize");
    assert!(
        function_body(&lua, "main").contains("Opt_lct("),
        "the explicit box did not take the leaf-table ctor:\n{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "leaf_table_tier"), "true:1:2:3");
}

#[test]
fn a_nested_cdata_value_boxes_as_one_leaf_constructor() {
    let lua = emit_ok(NESTED_CDATA_TIER, "a nested cdata materialize");
    let main = function_body(&lua, "main");
    assert!(
        main.contains("Box2_lct("),
        "the explicit box did not flatten to the leaf ctor:\n{lua}"
    );
    assert!(
        !main.contains("NV2_ct("),
        "the explicit box built an intermediate cdata per nested field:\n{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "cdata_tier_nested"), "1 4");
}

#[test]
fn an_implicit_box_of_a_gc_tier_value_is_still_rejected() {
    let err = try_emit_module(IMPLICIT_BOX)
        .expect_err("an implicit box of a GC-tier nobox value must still be rejected");
    assert!(
        err.message.contains("cannot cross boxed call boundary"),
        "the implicit box was rejected for an unrelated reason: {err:?}"
    );
    assert!(err.location.line > 0, "the rejection lost its span: {err:?}");
}

#[test]
fn an_implicit_box_under_a_gc_tier_parent_is_rejected_with_a_span() {
    let err = try_emit_module(IMPLICIT_BOX_UNDER_A_GC_TIER_PARENT)
        .expect_err("an implicit nobox box inside a GC-tier parent must be rejected");
    assert!(
        err.message.contains("survived lowering as an implicit materialization"),
        "the implicit box was rejected for an unrelated reason: {err:?}"
    );
    assert!(err.location.line > 0, "the rejection lost its span: {err:?}");
}
