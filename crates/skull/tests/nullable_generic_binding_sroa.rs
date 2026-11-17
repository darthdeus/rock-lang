//! A generic instantiated at `V?` for an unmarked `value` must not license SROA
//! to rematerialize an absent row.
//!
//! Was a sentinel filed as "iterating a nullable COLUMN at an unmarked element".
//! Both halves of that title were wrong, and measuring is what showed it:
//!
//!   - `[BV2?]` at an unmarked value is NOT column-stored. It emits
//!     `{_data = {BV2_ct(1,1), nil, ..}, len, __ty}` -- a boxed vec. (Its
//!     non-nullable twin `[BV2]` IS columns, and the MARKED `[MV2?]` is columns
//!     plus `["@p"]`. So "column eligibility is not the nobox mark" holds for
//!     non-nullable only; unmarked nullable drops out of columns entirely.)
//!   - It is not an iteration bug. `fn first<T>(a: [T], i: int) -> T { let v =
//!     a[i]  return v }` reproduces it with no loop and no iterator. Iteration
//!     only fired it because `VecIter<T>.next` is a generic `@inline` body
//!     containing exactly that `let`.
//!
//! The binding predicate uses the concrete nullable fact before SROA is allowed
//! to rematerialize an escaping load. An absent row therefore stays absent, and
//! `tests/nullable/generic_binding_sroa.rock` covers the direct index and iterator paths.
//!
//! MARKED and UNMARKED are legitimately two mechanisms here, not one to unify:
//! marked splits leaves at LOWERING (`nobox_shape_of_ty`, re-applying the `?`),
//! unmarked does no leaf splitting at all and is only opportunistically
//! scalarized by SROA afterwards. Only the nullability QUERY was duplicated.

/// THE PROPERTY THE FIX ENDANGERED. `nonnull_value_struct` exists to LICENSE
/// SROA; narrowing it could switch non-nullable values back to boxed emission
/// and the corpus absent-row cases would still pass. So assert on the EMITTED code:
/// a non-nullable value read through a generic `let` must still scalarize to
/// direct column reads, with no `_ct` rebuild anywhere in the body.
#[test]
fn a_non_nullable_value_still_scalarizes_through_a_generic_let() {
    const SRC: &str = r#"
value BV2 { x: float, y: float }

fn thru<T>(a: [T], i: int) -> T { let v = a[i]  return v }

fn main() {
    let b: [BV2] = [BV2 { x: 1.0, y: 2.0 }, BV2 { x: 3.0, y: 4.0 }]
    print("{thru(b, 1).x}")
}
"#;
    let lua = crate::common::try_emit_module(SRC).expect("must emit");
    let body = lua
        .split("function thru")
        .nth(1)
        .expect("the specialized `thru` must be emitted");
    let body = body.split("\nend").next().unwrap();
    assert!(
        !body.contains("BV2_ct"),
        "a non-nullable value through a generic `let` must stay scalarized, not \
         rebuild a cdata -- the SROA license this fix narrowed:\n{body}"
    );
    assert_eq!(crate::common::run_main(&lua), "3");
}
