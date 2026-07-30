//! SENTINEL: iterating a nullable column at an UNMARKED value element dies on
//! the absent row.
//!
//! `[BV2?]` at an unmarked `value BV2` is column-stored -- column eligibility is
//! not the `nobox` mark -- so the element read branches on the absent row and
//! yields nil leaves. `VecIter.next` then packs those leaves unconditionally,
//! calling `BV2_ct(nil, nil)`, which fails the cdata conversion. The read carries
//! NO presence part at all on the unmarked side, so this is not the parked
//! "`pack` honors `present`" item: there is no `present` to honor. The unmarked
//! nullable column read needs one, the way the marked one has.
//!
//! Discriminants, all measured (`indexed_reads_are_correct` below pins the second
//! and third):
//!   - iteration only: `a[1]`, `a.get(1)` and `a.count()` are all correct.
//!   - the mark: the same program at a `nobox` element is CORRECT
//!     (`tests/nobox_nullable_column_combinators.rock`).
//!   - the `?`: an unmarked non-nullable column iterates fine
//!     (`unmarked_value_pipeline_still_works` in `nobox_column_combinators.rock`).
//!
//! ASSERTION IS INVERTED: `iterating_an_absent_row_dies` asserts the program
//! FAILS, which passes today because the bug exists. When the read grows a
//! presence part this goes red -- flip it to assert success with output `3`
//! (1.0 + 2.0, the two present rows' `x`), delete the inversion note, and move
//! the case into `nobox_nullable_column_combinators.rock` where its marked twin
//! already lives.

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

const ITERATE: &str = r#"
value BV2 { x: float, y: float }

fn main() {
    let a: [BV2?] = [BV2 { x: 1.0, y: 1.0 }, null, BV2 { x: 2.0, y: 2.0 }]
    let t = 0.0
    for v in a { if v != null { t = t + v.x } }
    print("{t as int}")
}
"#;

/// The bound on the defect: the same storage read by INDEX is correct, so the
/// absent row is stored right and only the iteration protocol loses it.
const INDEXED: &str = r#"
value BV2 { x: float, y: float }

fn main() {
    let a: [BV2?] = [BV2 { x: 1.0, y: 1.0 }, null, BV2 { x: 2.0, y: 2.0 }]
    print("{a[1] == null} {a.get(1) == null} {a[2] != null} {a.count()}")
}
"#;

#[test]
fn iterating_an_absent_row_dies() {
    let lua = common::try_emit_module(ITERATE).expect("an unmarked nullable column must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), "unmarked_nullable_iter");
    assert!(
        !ok && out.contains("cannot convert 'nil'"),
        "PINNED BEHAVIOR (BUG): iterating `[BV2?]` at an unmarked value element \
         must still die packing `BV2_ct(nil, nil)` on the absent row. If this now \
         succeeds, the fix landed -- flip to `assert!(ok)` + output `3`, drop the \
         inversion note in the module docs, and fold the case into \
         `tests/nobox_nullable_column_combinators.rock`.\n{out}"
    );
}

#[test]
fn indexed_reads_are_correct() {
    let lua = common::try_emit_module(INDEXED).expect("an unmarked nullable column must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), "unmarked_nullable_indexed");
    assert!(ok, "indexed reads of an unmarked nullable column must work:\n{out}");
    assert!(
        out.contains("true true true 3"),
        "the absent row must read back absent by index and by `.get`:\n{out}"
    );
}
