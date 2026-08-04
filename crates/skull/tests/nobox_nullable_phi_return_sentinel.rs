//! A `nobox` value option whose value comes from a PHI of a present arm and
//! `null`: `let v: P? = if present { P { x: 5, y: 6 } } else { null }`.
//!
//! The join's shape is widened from its ARMS, not read off the join
//! expression's own inferred type. `if c { P { .. } } else { null }` infers as
//! plain `P` -- the null arm does not widen it -- so reading nullability off the
//! expression planned a shape with no presence leaf. The null arm then split nil
//! into fields (`r0 = nil; r1 = r0.x`) and faulted, and the join returned a
//! CONSTANT `true` presence, so even without the crash the absent path could not
//! be reported. Both halves are asserted below.
//!
//! `materialize` was NOT the mechanism -- deleting it left an identical crash,
//! which is what ruled it out. It did carry a SECOND defect of its own: an
//! absent group was built through the ctor regardless of presence, so the leaves
//! arrived nil and the cdata conversion failed. It now branches, which is legal
//! exactly where the repack's branch is not: `materialize` is the authorized
//! box, so building one is the request rather than a violation of the mark.
//!
//! The controls bound the shape from three sides, and each was load-bearing
//! while this was a sentinel: a nullable marked local with NO merge, a
//! statement-position `return null`, and `materialize` on a non-nullable marked
//! value. The concrete-shape premises this belongs to are in
//! `tests/nobox_concrete_nullable_ops.rock`; the positive value coverage is
//! `tests/nobox_nullable_merge.rock`.


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

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn an_absent_nobox_option_from_a_phi_reports_absent_through_materialize() {
    assert_eq!(run(PHI_WITH_MATERIALIZE, "phi_materialize"), "absent=true");
}

/// The discriminant that was load-bearing while this was a sentinel: the same
/// absent path with `materialize` deleted. It crashed identically, which is how
/// `materialize` was ruled out as the mechanism -- and both are correct now for
/// the same reason, the join carrying a real presence slot.
#[test]
fn an_absent_nobox_option_from_a_phi_reports_absent_without_materialize() {
    assert_eq!(run(PHI_WITHOUT_MATERIALIZE, "phi_bare"), "absent=true");
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
