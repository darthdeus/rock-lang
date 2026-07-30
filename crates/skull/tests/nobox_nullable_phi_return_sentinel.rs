//! SENTINEL -- returning a `nobox` value option whose value came from a PHI of
//! a present arm and `null` crashes on the absent path, and hardcodes presence
//! on the present one.
//!
//! `let v: P? = if present { P { x: 5, y: 6 } } else { null }` emits an
//! unconditional leaf splat in the null arm:
//!
//! ```text
//! ::bb2::
//!   r0 = nil
//!   r1 = r0.x     -- attempt to index local 'r0' (a nil value)
//!   r0 = r0.y
//! ```
//!
//! and the merge block returns a CONSTANT presence: `do return (true), r0, r1
//! end`. So even without the crash the absent path could not be reported.
//!
//! `materialize` is NOT the mechanism. The reported repro wrapped the return in
//! `materialize(v)`; deleting the call leaves an identical crash at an identical
//! line in an identical function, which the first control below pins. What is
//! load-bearing is the PHI: `let v: P? = null; return v` (no merge) is correct,
//! and a statement-position `return null` is correct -- both controlled here.
//!
//! Nor is it mechanism K (embed-field slot keying), where an unconditional read
//! is argued correct: there the slot is always backed, here the arm the read
//! sits in is exactly the arm where it is not.
//!
//! Nothing covered this because every existing `q6_materialize_*` probe
//! materializes a NON-null value, and the marked-option shapes elsewhere in the
//! suite return `null` from statement position.
//!
//! PINNED BEHAVIOR (BUG): the two sentinels assert that the absent path DIES at
//! runtime. When the fix lands they go RED -- flip each to
//! `common::run_luajit(...)` and assert `"absent=true"`, and delete this
//! paragraph.

mod common;

use std::process::Command;

/// The reported repro, `materialize` and all.
const PHI_WITH_MATERIALIZE: &str = r#"
nobox value P { x: int, y: int }

fn mk(present: bool) -> P? {
    let v: P? = if present { P { x: 5, y: 6 } } else { null }
    return materialize(v)
}

fn main() { print("absent={mk(false) == null}") }
"#;

/// The same program with `materialize` deleted -- the discriminant test for the
/// reported diagnosis. It crashes identically, so `materialize` is a passenger.
const PHI_WITHOUT_MATERIALIZE: &str = r#"
nobox value P { x: int, y: int }

fn mk(present: bool) -> P? {
    let v: P? = if present { P { x: 5, y: 6 } } else { null }
    return v
}

fn main() { print("absent={mk(false) == null}") }
"#;

/// Control: the PRESENT path of the reported repro. The crashing arm is never
/// entered, so the constant `true` presence happens to be right.
const PHI_PRESENT_PATH: &str = r#"
nobox value P { x: int, y: int }

fn mk(present: bool) -> P? {
    let v: P? = if present { P { x: 5, y: 6 } } else { null }
    return materialize(v)
}

fn main() { print("present={mk(true)?.x ?? -1}") }
"#;

/// Control: `materialize` on a non-nullable marked value. Reconstruction across
/// `materialize` itself is fine.
const MATERIALIZE_NON_NULLABLE: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> P { return materialize(P { x: 5, y: 6 }) }

fn main() { let p = mk(); print("{p.x} {p.y}") }
"#;

/// Control: a nullable local with NO merge. Isolates the phi from nullability.
const NULL_LOCAL_NO_PHI: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> P? {
    let v: P? = null
    return v
}

fn main() { print("absent={mk() == null}") }
"#;

/// Control: the same absent answer reached by a statement-position `return
/// null`. Isolates the phi from "returning null at a marked option" at large.
const EARLY_RETURN_NULL: &str = r#"
nobox value P { x: int, y: int }

fn mk(present: bool) -> P? {
    if !present { return null }
    return P { x: 5, y: 6 }
}

fn main() {
    print("absent={mk(false) == null}")
    print("present={mk(true)?.x ?? -1}")
}
"#;

/// Run under luajit WITHOUT asserting success -- these cases die on purpose.
fn run_allowing_death(src: &str, tag: &str) -> (bool, String) {
    let lua = common::try_emit_module(src).expect("must emit");
    let path = std::env::temp_dir().join(format!("skull_phi_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, format!("{lua}\nmain()\n")).expect("write program");
    let out = Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

fn run(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src).expect("must emit");
    common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn an_absent_nobox_option_from_a_phi_crashes_through_materialize_sentinel() {
    let (ok, out) = run_allowing_death(PHI_WITH_MATERIALIZE, "phi_materialize");
    assert!(
        !ok && out.contains("attempt to index local 'r0'"),
        "PINNED BEHAVIOR (BUG): the absent path is expected to die indexing a \
         nil leaf holder. It produced ok={ok}:\n{out}\nIf it now prints \
         `absent=true`, the fix landed -- flip this to assert the output."
    );
}

/// The discriminant: identical crash with `materialize` gone, so a fix aimed at
/// `materialize` alone would leave this one red-handed.
#[test]
fn an_absent_nobox_option_from_a_phi_crashes_without_materialize_sentinel() {
    let (ok, out) = run_allowing_death(PHI_WITHOUT_MATERIALIZE, "phi_bare");
    assert!(
        !ok && out.contains("attempt to index local 'r0'"),
        "PINNED BEHAVIOR (BUG): the same absent path with no `materialize` is \
         expected to die identically. It produced ok={ok}:\n{out}\nIf it now \
         prints `absent=true`, the fix landed -- flip this to assert the output."
    );
}

#[test]
fn the_present_path_of_the_same_phi_is_correct() {
    assert_eq!(run(PHI_PRESENT_PATH, "phi_present"), "present=5");
}

#[test]
fn materialize_on_a_non_nullable_nobox_value_is_correct() {
    assert_eq!(run(MATERIALIZE_NON_NULLABLE, "materialize_nonnull"), "5 6");
}

/// Bounds the sentinel: a nullable marked local with no merge reports absent
/// correctly, so the defect is the phi and not the option itself.
#[test]
fn a_nullable_nobox_local_with_no_phi_reports_absent() {
    assert_eq!(run(NULL_LOCAL_NO_PHI, "phi_none"), "absent=true");
}

/// Bounds it the other way: the same absent answer through statement-position
/// `return null` is correct on BOTH paths.
#[test]
fn a_statement_position_return_null_reports_absent() {
    assert_eq!(run(EARLY_RETURN_NULL, "phi_early_return"), "absent=true\npresent=5");
}
