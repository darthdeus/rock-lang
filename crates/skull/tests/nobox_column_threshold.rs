//! `[Opt]` where `Opt` is a `value` struct with a nullable `nobox` field, swept
//! by LEAF COUNT.
//!
//! The sweep is the point. `MAX_COLUMN_LEAVES` (`skull-mir/src/lower.rs`) makes
//! the STORAGE flip at 4 leaves -- flat columns at or below it, a boxed element
//! above -- and that flip is invisible from any single repro: a 2-field `Opt` is
//! 4 leaves (presence + x + y + a) and column-stores, a 3-field `Opt` is 5 and
//! boxes. Both must round-trip, present and absent, and the count is what tells
//! you which path a row exercised. This was long read as a leaf-count LIMIT on
//! the nobox guarantee; it is a storage tier, and the guarantee holds on both
//! sides of it. Raising the constant only moves the flip.
//!
//! The GC-tier ctor itself (one field, no Vec) is pinned by
//! `nobox_nested_ctor.rs`.

use crate::common::{run_luajit, try_emit_module};

/// `n` float fields after `pos`, so the element carries `n + 3` leaves
/// (presence, `pos.x`, `pos.y`, then the floats).
fn source(extra: usize) -> String {
    let names = ["a", "b", "c", "d"];
    let fields: Vec<String> = (0..extra).map(|i| format!(", {}: float", names[i])).collect();
    let inits: Vec<String> =
        (0..extra).map(|i| format!(", {}: {}.0", names[i], i + 3)).collect();
    let reads: Vec<String> = (0..extra).map(|i| format!(" {{V[0].{}}}", names[i])).collect();
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
/// element calls the leaf ctor and stores one table.
fn is_column_stored(lua: &str) -> bool {
    !lua.contains("Opt_lct((true)")
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
        let got = run_luajit(&format!("{lua}\nmain()\n"), "column_threshold");
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
