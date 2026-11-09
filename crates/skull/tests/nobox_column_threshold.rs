//! `[Opt]` where `Opt` is a `value` struct with a nullable `nobox` field, swept
//! by LEAF COUNT.
//!
//! The sweep is the point. `MAX_COLUMN_LEAVES`
//! (`skull-mir/src/lower/shape.rs`) makes the implicit STORAGE flip at 4 leaves
//! -- flat columns at or below it, a boxed element above -- and that flip is
//! invisible from any single repro: a 2-field `Opt` is 4 leaves (presence + x +
//! y + a) and column-stores, a 3-field `Opt` is 5 and boxes. Both must round-trip,
//! present and absent, and the count is what tells you which path a row exercised.
//! An element explicitly marked `nobox` bypasses this heuristic and stays flat
//! at any supported width -- columns up to the constant, one strided row
//! backing above it (`nobox_wide_strided.rs`); for unmarked values the constant
//! chooses between columns and a box.
//!
//! The GC-tier ctor itself (one field, no Vec) is pinned by
//! `nobox_nested_ctor.rs`.

use crate::common::{function_body, run_main, try_emit_module};

const WIDE_NOBOX_OVERWRITE: &str = r#"
nobox value Wide { a: int, b: int, c: int, d: int, e: int }

fn main() {
    let values: [Wide] = []
    values.push(Wide { a: 1, b: 2, c: 3, d: 4, e: 5 })
    values[0] = Wide { a: 6, b: 7, c: 8, d: 9, e: 10 }
    print("{values[0].a} {values[0].e}")
}
"#;

#[test]
fn wide_nobox_vec_stays_flat_past_the_implicit_threshold() {
    let lua = try_emit_module(WIDE_NOBOX_OVERWRITE)
        .expect("an explicit nobox guarantee must not fall back to boxed Vec elements");
    let body = function_body(&lua, "main");
    assert!(
        !body.contains("Wide_lct("),
        "wide nobox Vec materialized an element:\n{body}"
    );
    assert!(
        body.contains(r#"repr = "strided""#),
        "wide nobox Vec left flat row storage:\n{body}"
    );
    assert_eq!(run_main(&lua), "6 10");
}

/// `n` float fields after `pos`, so the element carries `n + 3` leaves
/// (presence, `pos.x`, `pos.y`, then the floats).
fn source(extra: usize) -> String {
    let names = ["a", "b", "c", "d"];
    let fields: Vec<String> = (0..extra)
        .map(|i| format!(", {}: float", names[i]))
        .collect();
    let inits: Vec<String> = (0..extra)
        .map(|i| format!(", {}: {}.0", names[i], i + 3))
        .collect();
    let reads: Vec<String> = (0..extra)
        .map(|i| format!(" {{V[0].{}}}", names[i]))
        .collect();
    format!(
        r#"
nobox value NV2 {{ x: float, y: float }}
value Opt {{ pos: NV2?{f} }}
global V: [Opt] = []
fn main() {{
    V.push(Opt {{ pos: NV2 {{ x: 1.0, y: 2.0 }}{i} }})
    V.push(Opt {{ pos: null{i} }})
    print("{{V[0].pos?.x ?? -1.0}} {{V[0].pos?.y ?? -1.0}}{r}")
    print("{{V[1].pos?.x ?? -1.0}} {{V[1] == V[0]}}")
}}
"#,
        f = fields.concat(),
        i = inits.concat(),
        r = reads.concat(),
    )
}

/// Column storage writes a leaf per column keyed by its dotted path; a boxed
/// element CALLS the leaf ctor and stores one table. So the tier is "is the leaf
/// ctor called at all", which is a property of the storage.
///
/// It deliberately does NOT match the ctor's ARGUMENTS. The previous form asked
/// for `Opt_lct((true)`, pinning the presence leaf as a literal constant at the
/// call -- and under instance-owned emission the same boxed store arrives as
/// `Opt_lct(r1, .., r5)` with the leaves in registers. That read as
/// "column-stored" for every row above the threshold and looked exactly like the
/// storage tier having moved, when `Opt_lct` was still being called and every
/// round-trip row above still passed. A spelling is not a tier.
///
/// The equality comparison line is excluded because the fixture's
/// `V[1] == V[0]` MATERIALIZES both operands through the same ctor whatever
/// the storage is -- at 3 leaves that comparison is the only `Opt_lct(` in
/// the chunk. Matching the bare name therefore reports "boxed" for every row,
/// which is the same failure from the other side. The tier is whether the
/// STORE builds an element, not whether a same-type equality compares two
/// materialized operands (same-named `value` struct equality is native `==`
/// now, not a `__op_eq` call -- see `value_struct_eq_dispatch.rs` -- so the
/// comparison line no longer names a callee; it is instead the only line
/// where `Opt_lct(` appears TWICE, once per operand, whereas a genuine
/// element store calls it exactly once).
fn is_column_stored(lua: &str) -> bool {
    !lua.lines().any(|l| l.matches("Opt_lct(").count() == 1)
}

#[test]
fn every_leaf_count_round_trips_and_the_storage_tier_flips_at_the_threshold() {
    let mut rows = Vec::new();
    for extra in 0..5 {
        let leaves = extra + 3;
        let src = source(extra);
        let lua = match try_emit_module(&src) {
            Ok(lua) => lua,
            Err(e) => panic!("{} fields / {leaves} leaves must emit: {e:?}", extra + 1),
        };
        let want_present: String = std::iter::once("1 2".to_string())
            .chain((0..extra).map(|i| format!(" {}", i + 3)))
            .collect();
        let got = run_main(&lua);
        let got: Vec<&str> = got.trim().lines().collect();
        assert_eq!(
            got,
            vec![want_present.as_str(), "-1 false"],
            "{} fields / {leaves} leaves read back wrong",
            extra + 1
        );
        rows.push((leaves, is_column_stored(&lua)));
    }
    // The flip itself: at or below 4 leaves the element is columns, above it a
    // box. Asserted as the whole sweep so a moved constant reads as one diff.
    assert_eq!(
        rows,
        vec![(3, true), (4, true), (5, false), (6, false), (7, false)],
        "the column/boxed storage flip moved off MAX_COLUMN_LEAVES = 4"
    );
}
