//! SENTINEL -- `Arena<P>.random()` at a `nobox` P compiles clean and CRASHES.
//!
//! Every other arena op works at a marked element (insert / get / contains /
//! remove / len / clear / pairs / values / keys / enumerate), and the bookkeeping
//! SoA is what made them reachable at all -- `Arena<P>` did not compile before
//! `8a6008e11`. So this is a newly REACHABLE hole rather than a regression of
//! something that used to work, and it is invisible to the suite because every
//! `arena_random_*` test uses `Arena<int>` / `Arena<string>`, which are boxed.
//!
//! The defect: `random` reads `arena.data[i]` through the BOXED accessor
//! (`__vec_checked_get`) on a COLUMN-stored `data`, which has no `_data` field --
//! `attempt to index field '_data' (a nil value)`. It is specific to the MARK:
//! the same op over an unmarked-but-column-stored element
//! (`value CV { x: float, y: float }`) returns correctly, which is the control
//! below and is what scopes this to the nobox lowering rather than to the SoA
//! layout or to `random` itself.
//!
//! PINNED BEHAVIOR (BUG): the marked program is asserted to FAIL at runtime.
//! When the fix lands this test goes RED. Flip it -- assert success and the
//! right value, exactly as the control does -- and delete this paragraph.

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

const MARKED: &str = r#"
nobox value NP { x: float, y: float }

fn main() {
    let a: Arena<NP> = arena_new()
    a.insert(NP { x: 1.0, y: 1.0 })
    a.insert(NP { x: 2.0, y: 2.0 })
    print("{a.random() != null}")
}
"#;

/// The control: identical program, element column-stored but NOT marked.
const UNMARKED: &str = r#"
value CV { x: float, y: float }

fn main() {
    let a: Arena<CV> = arena_new()
    a.insert(CV { x: 1.0, y: 1.0 })
    a.insert(CV { x: 2.0, y: 2.0 })
    print("{a.random() != null}")
}
"#;

#[test]
fn arena_random_at_a_nobox_element_crashes_sentinel() {
    let lua = common::try_emit_module(MARKED)
        .expect("PINNED BEHAVIOR (BUG): this compiles clean -- the failure is at RUNTIME");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), "arena_random_marked");
    assert!(
        !ok,
        "PINNED BEHAVIOR (BUG): `Arena<P>.random()` at a marked element is \
         expected to die reading `_data` off a column Vec. It SUCCEEDED, so the \
         fix landed -- flip this to assert success and `true`, matching the \
         control below.\n{out}"
    );
    assert!(
        out.contains("_data"),
        "expected the column/boxed-accessor mismatch (`_data` nil), got a \
         different failure -- diagnose before flipping:\n{out}"
    );
}

/// Bounds the sentinel above: without this, a fix that broke `random` for EVERY
/// column element would leave the sentinel green and look like progress.
#[test]
fn arena_random_at_an_unmarked_column_element_works() {
    let lua = common::try_emit_module(UNMARKED).expect("unmarked column element must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), "arena_random_unmarked");
    assert!(ok, "`Arena<CV>.random()` must work for a column-stored element:\n{out}");
    assert_eq!(out.trim(), "true", "expected a live entry to be selected:\n{out}");
}
