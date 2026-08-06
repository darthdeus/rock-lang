//! SENTINEL -- pins a live codegen bug.
//!
//! `a ?? b` yielding a SCALARIZED `value` struct, used directly as a struct
//! literal's field initializer, emits a field that is nil at runtime. The
//! literal's other fields are written correctly, so nothing fails at the
//! construction site; the nil surfaces later, wherever that field is first
//! read. Found from a `Vec2?` parameter defaulted through `??` in mag's
//! `spawn_ground_item`, where it died three call frames away inside the
//! renderer with `attempt to perform arithmetic on local 'r1'`.
//!
//! CORRECT behavior: `Thing { at: fallback ?? v(7, 8) }` sets `at` to (7, 8),
//! exactly as the hoisted `let launch = fallback ?? v(7, 8)` form does.
//!
//! DISCRIMINANT, measured -- three controls below narrow it to one cell:
//!   * hoisting the same `??` into a `let` first is CORRECT, so the coalesce
//!     lowering is fine on its own and the literal field position is the
//!     trigger;
//!   * a BOXED struct through the same inline `??` in the same position is
//!     CORRECT, so it is the value-struct scalarization -- the field is
//!     stored as separate `at.x` / `at.y` slots, and the coalesce writes
//!     neither;
//!   * `??` on a scalar (int) field is CORRECT, which rules out the literal
//!     field position on its own.
//!
//! This is a SILENT WRONG VALUE, not a rejection: the emit is valid Lua and
//! the crash lands somewhere unrelated to the mistake, which is why it is
//! pinned rather than left to be rediscovered.
//!
//! WHEN THIS GOES RED, the bug is fixed. Flip `BUGGY_NIL` to the correct
//! `"7 8"` in `coalesce_into_value_struct_literal_field_emits_nil`, rename it
//! to say it works, and update this header.

/// The bug: `??` into a `value`-struct field of a struct literal.
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

/// PINNED BEHAVIOR (BUG). The scalarized field is never written, so reading it
/// dies on nil. Asserting on the failure TEXT rather than on a value, because
/// there is no value to read -- that is the bug.
#[test]
fn coalesce_into_value_struct_literal_field_emits_nil() {
    let out = std::panic::catch_unwind(|| run(VALUE_FIELD_INLINE, "coalesce_value_field"));

    assert!(
        out.is_err(),
        "PINNED BEHAVIOR (BUG): `Thing {{ at: fallback ?? v(7, 8) }}` leaves the \
         scalarized `at` nil, so reading it dies at runtime. If this now \
         SUCCEEDS, the fix landed -- flip this to \
         `assert_eq!(run(VALUE_FIELD_INLINE, ..), \"7 8\")` and update the \
         header."
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
