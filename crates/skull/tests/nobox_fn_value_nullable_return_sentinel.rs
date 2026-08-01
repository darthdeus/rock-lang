//! A lambda RETURNED as a fn value whose return type is a nullable `nobox`
//! value: `f(true)?.x` must read `x`, not the next leaf.
//!
//! It answered `6` (the value of `y`), because the hoisted lambda returned only
//! its LEAVES -- `do return r0, r1 end` for `P { x, y }` -- with no presence
//! slot, while the call site read the declared fn type and bound three. Leaf
//! count moved the symptom without changing the defect (measured at
//! `d3cd09c7d`: 1 leaf read as absent, 2 answered 6, 3 answered `x == 6`).
//!
//! What separated it from the controls was which bone path records the lambda's
//! type. A lambda filling a hole with a known type (a declared `let`, a call
//! argument) goes through `check_expr`, which records the EXPECTED type, so both
//! ends read one fn type. RETURN position was inferred-then-coerced, recording
//! the lambda's OWN narrower type (`if c { P {..} } else { null }` infers `P`,
//! not `P?`). All three return positions now check: the fn-body TAIL, a
//! `Statement::Return` and an `Expr::Return`.
//!
//! The ABSENT path used to crash here too (`attempt to index local 'r0'`) and
//! was attributed to the nullable-nobox phi. It no longer does: with the lambda
//! emitting its presence slot, the absent arm returns `false, nil, nil` and the
//! site reads it. The phi defect is real and still pinned in
//! `nobox_nullable_phi_return_sentinel.rs`, where it reproduces with no lambda
//! and no fn value anywhere -- this program was reaching it only through the
//! missing slot.


/// The repro, both arms -- the absent one no longer crashes.
const RETURNED_LAMBDA: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> (bool) -> P? { present => if present { P { x: 5, y: 6 } } else { null } }

fn main() {
    let f = mk()
    print("{f(true)?.x ?? -1} {f(false)?.x ?? -1}")
}
"#;

/// Control: identical fn value, return type NOT nullable. No presence slot is
/// expected, so the dest list is one entry per leaf.
const RETURNED_LAMBDA_NON_NULL: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> (bool) -> P { present => P { x: 5, y: 6 } }

fn main() {
    let f = mk()
    let p = f(true)
    print("{p.x} {p.y}")
}
"#;

/// Control: a NAMED fn taken as that exact fn-value type. The callee always
/// emitted the presence slot, so this route was correct throughout.
const NAMED_FN_AS_VALUE: &str = r#"
nobox value P { x: int, y: int }

fn maybe(present: bool) -> P? { if present { P { x: 5, y: 6 } } else { null } }

fn main() {
    let f: (bool) -> P? = maybe
    print("{f(true)?.x ?? -1}")
}
"#;

/// Control: the same named fn crossing a `(bool) -> P?` PARAM rather than a
/// local -- the other way a fn value reaches an indirect call site.
const NAMED_FN_AS_PARAM: &str = r#"
nobox value P { x: int, y: int }

fn maybe(present: bool) -> P? { if present { P { x: 5, y: 6 } } else { null } }

fn use_it(f: (bool) -> P?) -> int { f(true)?.x ?? -1 }

fn main() {
    print("{use_it(maybe)}")
}
"#;

/// Control, and the sharp one: the SAME lambda bound to a typed local instead
/// of returned. This was the form that already went through `check_expr`, so it
/// is what localized the defect to the return positions.
const LOCALLY_BOUND_LAMBDA: &str = r#"
nobox value P { x: int, y: int }

fn main() {
    let f: (bool) -> P? = present => if present { P { x: 5, y: 6 } } else { null }
    print("{f(true)?.x ?? -1}")
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn a_returned_lambda_at_a_nullable_nobox_return_reads_the_right_field() {
    assert_eq!(
        run(RETURNED_LAMBDA, "fnval_returned_lambda"),
        "5 -1",
        "a returned lambda must read `x` when present and report absence when not"
    );
}

/// The other two return positions the same fix had to cover. The repro above is
/// a fn-body TAIL; these are an explicit `return` of the lambda, and the same
/// lambda routed through a declared local first (which was always correct, and
/// bounds the fix on the side that already worked).
const RETURNED_LAMBDA_EXPLICIT: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> (bool) -> P? { return present => if present { P { x: 5, y: 6 } } else { null } }

fn main() {
    let f = mk()
    print("{f(true)?.x ?? -1}")
}
"#;

const RETURNED_LAMBDA_VIA_LOCAL: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> (bool) -> P? {
    let g: (bool) -> P? = present => if present { P { x: 5, y: 6 } } else { null }
    return g
}

fn main() {
    let f = mk()
    print("{f(true)?.x ?? -1}")
}
"#;

#[test]
fn an_explicitly_returned_lambda_reads_the_right_field() {
    assert_eq!(
        run(RETURNED_LAMBDA_EXPLICIT, "fnval_returned_lambda_explicit"),
        "5",
        "`return <lambda>` must record the declared return type, like the tail form"
    );
}

#[test]
fn a_lambda_returned_through_a_declared_local_is_correct() {
    assert_eq!(
        run(RETURNED_LAMBDA_VIA_LOCAL, "fnval_returned_lambda_via_local"),
        "5",
        "the declared-let route was always correct and must stay so"
    );
}

/// Bounds the sentinel on nullability: a fix that broke every fn-value return
/// of a marked type would otherwise leave the sentinel green and read as progress.
#[test]
fn a_returned_lambda_at_a_non_nullable_nobox_return_is_correct() {
    assert_eq!(
        run(RETURNED_LAMBDA_NON_NULL, "fnval_returned_lambda_nonnull"),
        "5 6",
        "a non-nullable nobox return through a fn value must stay correct"
    );
}

/// Bounds it on the callee: the fn-value TYPE is not enough, the callee has to
/// be a hoisted lambda.
#[test]
fn a_named_fn_bound_as_that_fn_value_is_correct() {
    assert_eq!(
        run(NAMED_FN_AS_VALUE, "fnval_named_local"),
        "5",
        "a named fn behind the same fn-value type must read the right field"
    );
}

/// Same bound through the other route a fn value takes to an indirect call.
#[test]
fn a_named_fn_passed_as_a_fn_value_param_is_correct() {
    assert_eq!(
        run(NAMED_FN_AS_PARAM, "fnval_named_param"),
        "5",
        "a named fn crossing a fn-value param must read the right field"
    );
}

/// The form that was always correct, and the one the fix made the others match:
/// a lambda in a LET is checked against the declared type and records it, so
/// both ends of the fn value read `(bool) -> P?` and the body emits its presence
/// slot.
#[test]
fn the_same_lambda_bound_to_a_typed_local_is_correct() {
    assert_eq!(
        run(LOCALLY_BOUND_LAMBDA, "fnval_local_lambda"),
        "5",
        "a locally-bound lambda at a nullable nobox return must read the right field"
    );
}
