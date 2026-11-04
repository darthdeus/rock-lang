//! A `value` struct that nests a `nobox` field must not construct that field as
//! a separate box. `E5_ct` takes nested fields WHOLE, so materializing the outer
//! struct materialized the inner one -- which the guarantee forbids. The outer
//! struct boxing is fine and expected; building it *through* a nobox box is not.
//!
//! Normalization flattens the nested ctor into its parent's leaves, so these
//! assert the flattened emit plus a luajit round-trip. A nullable field makes
//! the outer GC-tier, which has no cdata to write leaves into: it stores flat
//! instead (dotted leaf keys plus a presence key) through `<Name>_lct`. The
//! leaf-count sweep that separates this from the column threshold lives in
//! `nobox_column_threshold.rs`. Design + work list:

use crate::common::{function_body, mangled_name, run_luajit, try_emit_module};

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

/// The MINIMAL cell: a nullable nobox field makes the outer GC-tier, so it
/// materializes as a table rather than cdata. ONE field is enough -- the leaf
/// count is not the discriminant here (that is the `[Opt]` column threshold,
/// pinned separately in `nobox_column_threshold.rs`), and a global is what
/// forces the outer to materialize at all.
const NULLABLE_FIELD: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2? }
global G: Opt = Opt { pos: NV2 { x: 1.0, y: 2.0 } }
fn main() { print("{G.pos?.x ?? -1.0}") }
"#;

/// A leaf-table box read back OUT of a container. The declared field name holds
/// only the presence tag there, so a read that keys off it hands the consumer a
/// bool -- which indexes, and silently reads `nil` for every leaf.
const LEAF_TABLE_OUT_OF_A_MAP: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2?, a: float }
global M: Map<string, Opt> = map_new()
fn main() {
    M["k"] = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0 }
    let v = M["k"]
    print("{v.pos?.x ?? -1.0} {v.pos?.y ?? -1.0} {v.a}")
}
"#;

/// The two ways to spell an ABSENT payload -- an absent literal and a later
/// `= null` write -- must produce the same storage. `__eq` compares per key, so
/// a ctor spelling absence `false` while a store spells it `nil` makes two
/// logically-equal values compare unequal.
const ABSENT_SPELLINGS_AGREE: &str = r#"
nobox value NV2 { x: float, y: float }
value Opt { pos: NV2?, a: float }
global G: Opt = Opt { pos: null, a: 3.0 }
global H: Opt = Opt { pos: NV2 { x: 1.0, y: 2.0 }, a: 3.0 }
fn main() {
    H.pos = null
    print("{G == H} {H.pos == null}")
}
"#;

/// A LOCAL the outer struct is bound to, reassembled by a spread that escapes
/// somewhere unscalarized. SROA splits the local into per-field cells, and the
/// `pos` cell then holds a whole `NV2` -- the box. Normalization alone cannot
/// clear it: the reassembly reads `pos` through a `Load`, which is opaque to a
/// ctor-tree walk, so the outer ctor is not a target; and SROA alone cannot
/// split the `pos` cell while that whole-value read is its user. Only
/// interleaving the two reaches it.
///
/// Every ingredient is load-bearing and each was measured alone. The nested
/// field must be `nobox` (a plain `value` is no violation). The escape must be
/// unscalarized -- an enum payload, since the value-struct call ABI scalarizes a
/// direct call argument and the reassembly disappears. And the block boundary
/// must be there, or copy-prop forwards the ctor to the use and the tree walk
/// sees it whole.
const SROA_SPLIT_REASSEMBLY: &str = r#"
nobox value NV2 { x: float, y: float }
value E3 { pos: NV2, a: float }
enum Part { One(E3) }
global FLAG: bool = true
fn sink(p: Part) -> float {
    match p { Part::One(e) => return e.a + e.pos.x + e.pos.y }
}
fn main() {
    let e = E3 { pos: NV2 { x: 1.0, y: 2.0 }, a: 4.0 }
    if FLAG { print("boundary") }
    print("{sink(Part::One(E3 { ..e, a: 8.0 }))}")
}
"#;

const NESTED_NOBOX_ACROSS_COROUTINE_FRAME: &str = r#"
nobox value NV2 { x: float, y: float }
value E3 { pos: NV2, a: float }
enum Part { One(E3) }
global FLAG: bool = true
fn sink(p: Part) -> float {
    match p { Part::One(e) => return e.a + e.pos.x + e.pos.y }
}
co fn build() -> Coroutine<float> {
    let e = E3 { pos: NV2 { x: 1.0, y: 2.0 }, a: 4.0 }
    yield 0.0
    if FLAG { print("boundary") }
    yield sink(Part::One(E3 { ..e, a: 8.0 }))
}
fn main() {
    let c = build()
    for n in c { print("{n as int}") }
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
    assert_eq!(
        run_luajit(
            &format!(
                "{lua}
main()
"
            ),
            "global_outer"
        )
        .trim(),
        "1 2 3 4 5"
    );
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
    assert_eq!(
        run_luajit(
            &format!(
                "{lua}
main()
"
            ),
            "two_level"
        )
        .trim(),
        "1 2 3 4 5 6"
    );
}

#[test]
fn an_opaque_nested_operand_is_decomposed_not_rejected() {
    let lua = emit_ok(MIXED_LITERAL_AND_OPAQUE, "a mixed literal/opaque ctor");
    assert!(
        !function_body(&lua, "setup").contains("NV2_ct("),
        "the nested nobox field materialized a cdata:
{lua}"
    );
    assert_eq!(
        run_luajit(
            &format!(
                "{lua}
main()
"
            ),
            "mixed"
        )
        .trim(),
        "6 7 1 2 8"
    );
}

#[test]
fn a_nullable_nobox_field_in_a_gc_tier_outer_carries_presence() {
    let lua = emit_ok(NULLABLE_FIELD, "a nullable nobox field in a GC-tier outer");
    assert!(
        !function_body(&lua, "__skull_ginit_G").contains("NV2_ct("),
        "the nullable nobox field materialized a cdata:
{lua}"
    );
    // The presence tag leads the leaves, so an absent payload is representable
    // without a box to be nil.
    assert!(
        function_body(&lua, "__skull_ginit_G").contains("Opt_lct((true), (1.0), (2.0))"),
        "the GC-tier outer did not take the presence-carrying leaf ctor:
{lua}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nullable_field").trim(),
        "1"
    );
}

#[test]
fn a_leaf_table_element_read_out_of_a_container_projects_its_leaves() {
    let lua = emit_ok(LEAF_TABLE_OUT_OF_A_MAP, "a leaf-table Map value");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "map_value").trim(),
        "1 2 3"
    );
}

#[test]
fn both_absent_spellings_store_the_same_tag() {
    let lua = emit_ok(
        ABSENT_SPELLINGS_AGREE,
        "an absent literal against a null write",
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "absent_spellings").trim(),
        "true true"
    );
}

#[test]
fn an_sroa_split_local_reassembled_into_an_escape_does_not_box_its_nested_field() {
    let lua = emit_ok(SROA_SPLIT_REASSEMBLY, "a spread of an SROA-split local");
    assert!(
        !function_body(&lua, "main").contains("NV2_ct("),
        "the split local's nested nobox field materialized a cdata:
{lua}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "sroa_reassembly").trim(),
        "boundary\n11"
    );
}

#[test]
fn a_coroutine_frame_carries_a_value_with_a_nested_nobox_field() {
    let lua = emit_ok(
        NESTED_NOBOX_ACROSS_COROUTINE_FRAME,
        "a nested nobox field carried across a coroutine frame",
    );
    let build = mangled_name(&lua, "build");
    assert!(
        !function_body(&lua, &format!("{build}__step")).contains("NV2_ct("),
        "the coroutine frame materialized its nested nobox field:\n{lua}"
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nested_nobox_coroutine").trim(),
        "0\nboundary\n11"
    );
}

#[test]
fn an_explicit_materialize_stays_legal() {
    let lua = emit_ok(EXPLICIT_MATERIALIZE, "an explicit materialize");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "materialize").trim(),
        "1 2"
    );
}

#[test]
fn an_explicitly_materialized_nested_operand_is_left_whole() {
    let lua = emit_ok(
        MATERIALIZED_OPERAND,
        "an explicitly materialized nested operand",
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "materialized_operand").trim(),
        "1 2 3"
    );
}
