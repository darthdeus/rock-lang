//! FIXED -- was a sentinel. A NULL tuple-option return from a generic fn read
//! as non-null when the tuple carried a `nobox` element.
//!
//! The presence slot was on the wire the whole time; two consumers computed the
//! shape under the WRONG substitution and so never looked at it. `lower_call`
//! read the callee's return shape under the CALL's instantiation, while the
//! presence consumers (`nullable_tuple_call_shape`) read it under the enclosing
//! body's `spec_subst` -- empty in a non-generic caller, so the callee's generic
//! stayed unbound, every slot came back `Opaque`, and `tuple_ret_shape`'s
//! `nullable && !splats` guard answered `None`. With `None` both consumers fell
//! through to the boxed path: `!= null` packed a `MakeTuple`, which is
//! structurally never nil and const-folds to `true`, and `if let` emitted no
//! branch at all. `Arena<NP>.random()` on an EMPTY arena reported a hit for this
//! reason, and now correctly reports null.
//!
//! Both consumers now read `call_ret_tuple_shape`, the single oracle -- the same
//! rule CLAUDE.md records for `bound_next_elem_slot` / `expr_tuple_shape` /
//! `body_ret_slot`. A disagreement here cannot fail loudly; it just branches on
//! a slot that is not there, or fails to.
//!
//! Distinct from the accessor-selection defect in
//! `nobox_arena_random_sentinel.rs`: that one picked the wrong BODY (shared vs
//! specialized) and crashed; this one reaches the right body and silently drops
//! the option-ness at the ABI. Fixing the former did not touch it -- it
//! reproduces identically at `3c8ccefeb`.
//!
//! The controls stay: an unmarked value element and a non-generic fn returning
//! `(int, NP)?` both reported null correctly throughout, which is what scoped
//! the defect to the splatting tuple ABI rather than to nullable tuples or to
//! generics at large. The discriminant was generic-vs-non-generic, not the UFCS
//! spelling -- a plain call through the same generic failed identically.

use crate::common::compile_and_run;

const MARKED: &str = r#"
nobox value NP { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.pick(h) -> (int, T)? {
    if vec_len(h.items) == 0 { return null }
    return (0, h.items[0])
}

fn main() {
    let empty: Holder<NP> = Holder { items: [] }
    print("{empty.pick() != null}")
}
"#;

/// Control: identical generic, element column-stored but NOT marked.
const UNMARKED: &str = r#"
value CV { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.pick(h) -> (int, T)? {
    if vec_len(h.items) == 0 { return null }
    return (0, h.items[0])
}

fn main() {
    let empty: Holder<CV> = Holder { items: [] }
    print("{empty.pick() != null}")
}
"#;

/// Control: a marked element in a tuple option, but no generic to specialize.
const MARKED_NON_GENERIC: &str = r#"
nobox value NP { x: float, y: float }

fn pick(flag: bool) -> (int, NP)? {
    if !flag { return null }
    return (1, NP { x: 3.0, y: 4.0 })
}

fn main() {
    print("{pick(false) != null}")
}
"#;

#[test]
fn a_null_tuple_option_at_a_nobox_element_reads_as_null() {
    assert_eq!(
        compile_and_run(MARKED),
        "false",
        "a `return null` from a tuple-returning generic at a marked element must \
         report null, matching the controls below"
    );
}

/// The property the fix ENDANGERED, not the one it intended: reading the
/// presence slot could just as easily report a PRESENT tuple as absent, and the
/// null-side assertions above would all still pass.
#[test]
fn a_present_tuple_option_at_a_nobox_element_still_reads_its_values() {
    assert_eq!(
        compile_and_run(MARKED_PRESENT),
        "true 0 7 9",
        "a present tuple option must stay present and keep its element's leaves"
    );
}

/// Bounds the sentinel: without this, a fix that broke the presence slot for
/// EVERY column element would leave the sentinel green and look like progress.
const MARKED_PRESENT: &str = r#"
nobox value NP { x: float, y: float }

struct Holder<T> { items: [T] }

fn Holder<T>.pick(h) -> (int, T)? {
    if vec_len(h.items) == 0 { return null }
    return (0, h.items[0])
}

fn main() {
    let h: Holder<NP> = Holder { items: [NP { x: 7.0, y: 9.0 }] }
    let ok = h.pick() != null
    if let t = h.pick() {
        print("{ok} {t.0} {t.1.x} {t.1.y}")
    } else {
        print("MISS")
    }
}
"#;

#[test]
fn a_null_tuple_option_at_an_unmarked_column_element_reads_as_null() {
    assert_eq!(
        compile_and_run(UNMARKED),
        "false",
        "an unmarked column element must still report null correctly"
    );
}

/// Bounds the sentinel on the other axis: the mark alone is not enough, the
/// splatting tuple ABI needs a generic to specialize through.
#[test]
fn a_null_tuple_option_from_a_non_generic_fn_reads_as_null() {
    assert_eq!(
        compile_and_run(MARKED_NON_GENERIC),
        "false",
        "a non-generic tuple option must report null correctly even at a mark"
    );
}
