//! A `value` struct that nests a `nobox` field must not construct that field as
//! a separate box. `E5_ct` takes nested fields WHOLE, so materializing the outer
//! struct materialized the inner one -- which the guarantee forbids. The outer
//! struct boxing is fine and expected; building it *through* a nobox box is not.
//!
//! SENTINELS (`docs/test-sentinels.md`). The normalization pass that flattens a
//! nested nobox ctor into its parent's leaves is NOT built yet, so each case
//! below asserts TODAY'S REJECTION. Design + work list:
//! `docs/nobox-nested-ctor-plan.md`.
//!
//! WHEN NORMALIZATION LANDS these go red. Flip each one in the same commit:
//! swap `assert_rejects_today` for `emit_ok` + the `NV2_ct` absence assertion +
//! the runtime round-trip already spelled out in each test's comment, and
//! delete this note.

mod common;
use common::{function_body, run_luajit, try_emit_module};

/// A global forces the outer struct to materialize with no `Vec` in the program,
/// which is what separates this class from the `MAX_COLUMN_LEAVES` trigger.
const GLOBAL_OUTER: &str = r#"
nobox value NV2 { x: float, y: float }
value E5 { pos: NV2, a: float, b: float, c: float }
global G: E5
fn setup() { G = E5 { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0, b: 4.0, c: 5.0 } }
fn main() {
    setup()
    print("{G.pos.x} {G.pos.y} {G.a} {G.b} {G.c}")
}
"#;

/// Two levels: the outer's IMMEDIATE operand is a non-nobox `Box2` literal, so a
/// trigger that only matches "field operand is a literal nobox ctor" misses it.
const TWO_LEVEL: &str = r#"
nobox value NV2 { x: float, y: float }
value Box2 { lo: NV2, hi: NV2 }
value Outer { b: Box2, a: float, c: float }
global G: Outer
fn setup() {
    G = Outer { b: Box2 { lo: NV2 { x: 1.0, y: 2.0 }, hi: NV2 { x: 3.0, y: 4.0 } }, a: 5.0, c: 6.0 }
}
fn main() {
    setup()
    print("{G.b.lo.x} {G.b.lo.y} {G.b.hi.x} {G.b.hi.y} {G.a} {G.c}")
}
"#;

/// One literal nobox field alongside an OPAQUE nested operand read off an
/// existing cdata. The opaque sibling must be decomposed, not rejected.
const MIXED_LITERAL_AND_OPAQUE: &str = r#"
nobox value NV2 { x: float, y: float }
value Pair { lo: NV2, hi: NV2, w: float }
global G: Pair
global H: Pair
fn setup() {
    G = Pair { lo: NV2 { x: 1.0, y: 2.0 }, hi: NV2 { x: 3.0, y: 4.0 }, w: 5.0 }
    H = Pair { lo: NV2 { x: 6.0, y: 7.0 }, hi: G.lo, w: 8.0 }
}
fn main() {
    setup()
    print("{H.lo.x} {H.lo.y} {H.hi.x} {H.hi.y} {H.w}")
}
"#;

/// SENTINEL, and it pins a RESIDUAL CLASS rather than a gap in the pass: a
/// nullable field makes the outer GC-tier (it emits `Opt_vt`, a table), so there
/// is no cdata to write leaves into and allocate-then-write cannot apply. Flip
/// only if GC-tier outers ever learn to carry nobox leaves; the design's answer
/// today is an explicit `materialize`.
const NULLABLE_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2?, a: float, b: float, c: float }
global G: Opt
global H: Opt
fn setup() {
    G = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0, b: 4.0, c: 5.0 }
    H = Opt { pos: null, a: 6.0, b: 7.0, c: 8.0 }
}
fn main() {
    setup()
    let gp = if G.pos != null { G.pos.unwrap().x } else { -1.0 }
    let hp = if H.pos != null { H.pos.unwrap().x } else { -1.0 }
    print("{gp} {hp}")
}
"#;

/// An explicit `materialize` must stay legal -- it is the sanctioned escape.
const EXPLICIT_MATERIALIZE: &str = r#"
nobox value NV2 { x: float, y: float }
fn main() {
    let m = materialize(NV2 { x: 1.0, y: 2.0 })
    print("{m.x} {m.y}")
}
"#;

/// An EXPLICIT box as a nested operand is legal and must be left WHOLE by
/// normalization -- the trigger is implicit nested ctors only. Not a sentinel:
/// this must pass before and after the pass lands.
const MATERIALIZED_OPERAND: &str = r#"
nobox value NV2 { x: float, y: float }
value E5 { pos: NV2, a: float, b: float, c: float }
global G: E5
fn setup() { G = E5 { pos: materialize(NV2 { x: 1.0, y: 2.0 }), a: 3.0, b: 4.0, c: 5.0 } }
fn main() {
    setup()
    print("{G.pos.x} {G.pos.y} {G.a}")
}
"#;

fn emit_ok(src: &str, what: &str) -> String {
    match try_emit_module(src) {
        Ok(lua) => lua,
        Err(e) => panic!("{what} must emit, got a rejection: {e:?}"),
    }
}

/// INVERTED: pins the rejection the missing normalization pass causes today.
/// Flip to `emit_ok` + the assertions in each caller's comment when it lands.
fn assert_rejects_today(src: &str, what: &str) {
    let Err(e) = try_emit_module(src) else {
        panic!(
            "{what} now EMITS -- the nested-ctor normalization has landed. Flip \
             this sentinel: see the module header."
        )
    };
    let msg = format!("{e:?}");
    assert!(
        msg.contains("survived lowering as an implicit materialization"),
        "{what} must fail on the nobox guarantee, not something else: {msg}"
    );
}

/// Every leaf gets a DISTINCT value, so an ordering drift between the shape walk
/// and the emitted writes cannot hide behind symmetric literals.
#[test]
fn a_nested_nobox_field_does_not_box_when_the_outer_materializes() {
    let lua = emit_ok(GLOBAL_OUTER, "a global outer with a nested nobox field");
    assert!(
        !function_body(&lua, "setup").contains("NV2_ct("),
        "the nested nobox field materialized a cdata:
{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}
main()
"), "global_outer").trim(), "1 2 3 4 5");
}

#[test]
fn a_two_level_nesting_flattens_through_the_intermediate() {
    let lua = emit_ok(TWO_LEVEL, "a two-level nesting");
    assert!(
        !function_body(&lua, "setup").contains("NV2_ct("),
        "the nested nobox field materialized a cdata:
{lua}"
    );
    // Flattening happens at the OUTERMOST materialization: a bottom-up rewrite
    // would allocate an intermediate Box2 per construction.
    assert!(
        !function_body(&lua, "setup").contains("Box2_ct("),
        "an intermediate cdata was allocated instead of flattening at the outermost ctor:
{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}
main()
"), "two_level").trim(), "1 2 3 4 5 6");
}

#[test]
fn an_opaque_nested_operand_is_decomposed_not_rejected() {
    let lua = emit_ok(MIXED_LITERAL_AND_OPAQUE, "a mixed literal/opaque ctor");
    assert!(
        !function_body(&lua, "setup").contains("NV2_ct("),
        "the nested nobox field materialized a cdata:
{lua}"
    );
    assert_eq!(run_luajit(&format!("{lua}
main()
"), "mixed").trim(), "6 7 1 2 8");
}

#[test]
fn a_nullable_nobox_field_round_trips_both_ways() {
    assert_rejects_today(NULLABLE_FIELD, "a nullable nobox field in a GC-tier outer");
}

#[test]
fn an_explicit_materialize_stays_legal() {
    let lua = emit_ok(EXPLICIT_MATERIALIZE, "an explicit materialize");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "materialize").trim(), "1 2");
}

#[test]
fn an_explicitly_materialized_nested_operand_is_left_whole() {
    let lua = emit_ok(MATERIALIZED_OPERAND, "an explicitly materialized nested operand");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "materialized_operand").trim(),
        "1 2 3"
    );
}
