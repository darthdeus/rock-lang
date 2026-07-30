//! SENTINEL -- an absent nullable VALUE placed in a tuple reads as present.
//!
//! `let t = (m.get(k), 1)` on a MISSING key, then `t.0 == null`, answers `false`.
//! The whole body folds to `do return (not (true)) end`.
//!
//! Same root as the safe-nav receiver defect fixed in
//! `tests/safe_nav_absent_value_receiver.rock` -- a presence tag dropped on the
//! way to a box -- but a DIFFERENT sink, and it did not fall out of that fix.
//! There the consumer was `?.`, which now branches on the tag itself. Here the
//! value crosses into a tuple, and both `pack`'s `ValueRepr::Tuple` arm and
//! tuple PROJECTION discard `present` independently, so a fix has to cover both
//! and be re-verified rather than assumed.
//!
//! It is NOT fixable in `pack`'s Group arm, which is the obvious-looking move.
//! Measured: branching there puts the ctor behind a cell, SROA stops forwarding
//! it, the box survives to the residue check, and `nobox_coalesce_at_ctor_field`
//! regresses CORRECT -> LOUD REJECT -- the cost `docs/parked/inline-lua.md`
//! recorded for this exact attempt. Narrowing to a static presence does not help
//! either: at a `??` the presence is dynamic and the coalesce consumes it anyway.
//! `pack` cannot see whether its consumer needs nil-ness, so the fix belongs at
//! the consumer, as it did for safe-nav.
//!
//! Nothing here is `nobox` -- a plain unmarked `value` reproduces it, which is
//! the wider exposure. The `struct` twin is correct and bounds the class.
//!
//! PINNED BEHAVIOR (BUG): asserts `false false`, where the second should be
//! `true`. When the fix lands this goes RED -- flip to `false true`, matching the
//! struct control, and delete this paragraph.

mod common;

use std::process::Command;

fn run_capturing(prog: &str, tag: &str) -> (bool, String) {
    let path = std::env::temp_dir().join(format!("skull_sentinel_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, prog).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

fn run_module(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src).expect("must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), tag);
    assert!(ok, "program must run:\n{out}");
    out.trim().to_string()
}

const VALUE_IN_TUPLE: &str = r#"
value NV2 { x: float, y: float }

fn probe(m: Map<int, NV2>, k: int) -> bool {
    let t = (m.get(k), 1)
    return t.0 == null
}

fn main() {
    let m: Map<int, NV2> = map_new()
    m[1] = NV2 { x: 1.0, y: 2.0 }
    print("{probe(m, 1)} {probe(m, 9)}")
}
"#;

/// Bounds the class: a reference struct in the same position is correct, so this
/// is the value-struct presence tag, not tuples or `Map.get`.
const STRUCT_IN_TUPLE: &str = r#"
struct SV2 { x: float, y: float }

fn probe(m: Map<int, SV2>, k: int) -> bool {
    let t = (m.get(k), 1)
    return t.0 == null
}

fn main() {
    let m: Map<int, SV2> = map_new()
    m[1] = SV2 { x: 1.0, y: 2.0 }
    print("{probe(m, 1)} {probe(m, 9)}")
}
"#;

#[test]
fn an_absent_value_in_a_tuple_reads_as_present_sentinel() {
    let got = run_module(VALUE_IN_TUPLE, "nullable_value_tuple");
    assert_eq!(
        got, "false false",
        "PINNED BEHAVIOR (BUG): an absent nullable value in a tuple is expected \
         to read as PRESENT, giving `false false`. It answered `{got}`, so the \
         fix landed -- flip this to `false true`, matching the struct control."
    );
}

#[test]
fn a_struct_in_a_tuple_is_correct() {
    assert_eq!(
        run_module(STRUCT_IN_TUPLE, "nullable_struct_tuple"),
        "false true",
        "a reference struct in a tuple must report absence correctly"
    );
}
