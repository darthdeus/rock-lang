//! `a ?? b` yielding a SCALARIZED `value` struct, used directly as a struct
//! literal's field initializer, must thread the selected value as leaves. The
//! parent stores a `nobox` child in dotted physical slots, so lowering the join
//! through one boxed cell and then reading leaves from it used to leave those
//! slots nil.
//!
//! Three controls keep the discriminant narrow:
//!   * hoisting the same `??` into a `let` first is CORRECT, so the coalesce
//!     lowering is fine on its own and the literal field position is the
//!     trigger;
//!   * a BOXED struct through the same inline `??` in the same position is
//!     CORRECT, so it is the value-struct scalarization -- the field is
//!     stored as separate `at.x` / `at.y` slots, and the coalesce writes
//!     neither before the fix;
//!   * `??` on a scalar (int) field is CORRECT, which rules out the literal
//!     field position on its own.

/// `??` into a `value`-struct field of a struct literal.
const VALUE_FIELD_INLINE: &str = r#"
struct Thing { at: Vec2 = v(0, 0) }

fn main() {
    let fallback: Vec2? = null
    let t = Thing { at: fallback ?? v(7, 8) }
    print("{t.at.x as int} {t.at.y as int}")
}
"#;

/// Control: the same coalesce, hoisted into a `let` first. CORRECT today --
/// this is the shape the mag call site was rewritten to.
const VALUE_FIELD_HOISTED: &str = r#"
struct Thing { at: Vec2 = v(0, 0) }

fn main() {
    let fallback: Vec2? = null
    let launch = fallback ?? v(7, 8)
    let t = Thing { at: launch }
    print("{t.at.x as int} {t.at.y as int}")
}
"#;

const VALUE_FIELD_DIVERGING_RHS: &str = r#"
struct Thing { at: Vec2 = v(0, 0) }

fn make(fallback: Vec2?) -> Thing? {
    return Thing { at: fallback ?? return null }
}

fn main() {
    let t = make(v(13, 17)).unwrap()
    print("{t.at.x as int} {t.at.y as int}")
    print("{make(null) == null}")
}
"#;

/// Control: a BOXED struct through the identical inline `??`. CORRECT today,
/// which is what points at scalarization rather than at the literal.
const BOXED_FIELD_INLINE: &str = r#"
struct Boxed { a: int, b: int }
struct Thing { at: Boxed = Boxed { a: 0, b: 0 } }

fn main() {
    let fallback: Boxed? = null
    let t = Thing { at: fallback ?? Boxed { a: 7, b: 8 } }
    print("{t.at.a} {t.at.b}")
}
"#;

/// Control: a SCALAR field through the identical inline `??`. CORRECT today,
/// so the literal field position alone is not the trigger.
const SCALAR_FIELD_INLINE: &str = r#"
struct Thing { n: int = 0 }

fn main() {
    let fallback: int? = null
    let t = Thing { n: fallback ?? 7 }
    print("{t.n}")
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn coalesce_into_value_struct_literal_field_is_correct() {
    assert_eq!(
        run(VALUE_FIELD_INLINE, "coalesce_value_field").trim(),
        "7 8"
    );
}

#[test]
fn a_diverging_coalesce_arm_does_not_build_the_parent_from_nil_leaves() {
    assert_eq!(
        run(VALUE_FIELD_DIVERGING_RHS, "coalesce_value_diverging_rhs").trim(),
        "13 17\ntrue"
    );
}

#[test]
fn hoisting_the_coalesce_into_a_let_is_correct() {
    assert_eq!(
        run(VALUE_FIELD_HOISTED, "coalesce_value_hoisted").trim(),
        "7 8",
        "the coalesce lowering is fine once it is not a literal field initializer"
    );
}

#[test]
fn a_boxed_struct_through_the_same_coalesce_is_correct() {
    assert_eq!(
        run(BOXED_FIELD_INLINE, "coalesce_boxed_field").trim(),
        "7 8",
        "boxed fields take the coalesce inline correctly -- scalarization is the discriminant"
    );
}

#[test]
fn a_scalar_through_the_same_coalesce_is_correct() {
    assert_eq!(
        run(SCALAR_FIELD_INLINE, "coalesce_scalar_field").trim(),
        "7",
        "a literal field initializer is not itself the trigger"
    );
}
