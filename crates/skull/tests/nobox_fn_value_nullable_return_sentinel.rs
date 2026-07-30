//! SENTINEL -- a lambda RETURNED as a fn value whose return type is a nullable
//! `nobox` value reads one field too far: `f(true)?.x` yields `y`, not `x`.
//!
//! Two things go wrong at that boundary and both are visible in the emit for
//! `fn mk() -> (bool) -> P? { present => ... }`:
//!
//!   1. The hoisted lambda returns only its LEAVES -- `do return r0, r1 end`,
//!      two values for `P { x, y }` -- with no presence slot, while a named fn
//!      of the same signature returns three (`do return (true), r0, r1 end`).
//!   2. The call site binds three dests whose 1st and 3rd entries are the SAME
//!      cell: `c1, c2, c1 = r0((true))`. Dest-list construction for
//!      `Inst::CallValueMulti` (`crates/skull-mir/src/mir.rs`) does not keep the
//!      dests distinct.
//!
//! Composed, presence reads `x`(=5, truthy), `x` reads `y`(=6), so `?.x`
//! answers 6. Leaf count moves the symptom without changing the defect --
//! measured at `d3cd09c7d`: 1 leaf answers -1 (reads as absent), 2 leaves
//! answer 6, 3 leaves answer `x == 6, z == -1`.
//!
//! The ABSENT path of the same program crashes (`attempt to index local 'r0'`),
//! but that is a DIFFERENT defect -- the nullable-nobox phi, pinned separately
//! in `nobox_nullable_phi_return_sentinel.rs`, where it reproduces with no
//! lambda and no fn value anywhere. This file pins only the wrong VALUE on the
//! present path, so a fix to either one moves exactly one of the two.
//!
//! The controls below bound the class to a lambda that is RETURNED: the same
//! lambda bound to a typed LOCAL is correct, and every named-fn route through
//! the identical fn-value type answers 5. What separates the two is which bone
//! path records the lambda's type. A lambda filling a hole with a known type
//! (a let with a declared type, a call argument) goes through `check_expr`,
//! which records the EXPECTED type -- so the body and the indirect call site
//! read one fn type and agree on the presence slot. A lambda in RETURN position
//! never reaches `check_expr`: `Expr::Return` infers it and coerces to the
//! declared return, so what gets recorded is the lambda's OWN type, whose return
//! is narrower than the hole (`if c { P {..} } else { null }` infers `P`, not
//! `P?`). Closing this means routing the return position through the same check.
//!
//! PINNED BEHAVIOR (BUG): the sentinel asserts the wrong field, `6`. When the
//! fix lands this test goes RED -- flip it to assert `5`, matching the controls,
//! and delete this paragraph. A fix that instead turns this shape into a
//! REJECTION (the route the locally-bound control already takes) is equally
//! correct: replace the run with the same `Err` assertion the reject control
//! uses.

mod common;

/// The repro. Only the PRESENT call is made -- the absent call crashes, and
/// that crash belongs to the phi sentinel, not here.
const RETURNED_LAMBDA: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> (bool) -> P? { present => if present { P { x: 5, y: 6 } } else { null } }

fn main() {
    let f = mk()
    print("{f(true)?.x ?? -1}")
}
"#;

/// Control: identical fn value, return type NOT nullable. No presence slot is
/// expected, so the dest list has one entry per leaf and nothing aliases.
const RETURNED_LAMBDA_NON_NULL: &str = r#"
nobox value P { x: int, y: int }

fn mk() -> (bool) -> P { present => P { x: 5, y: 6 } }

fn main() {
    let f = mk()
    let p = f(true)
    print("{p.x} {p.y}")
}
"#;

/// Control: a NAMED fn taken as that exact fn-value type. The callee emits the
/// presence slot, so the same aliased dest list still lands `x` in the cell the
/// site reads.
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
/// of returned. The materialization gate fires here and does not fire for the
/// returned form -- which is why the returned form reaches codegen at all.
const LOCALLY_BOUND_LAMBDA: &str = r#"
nobox value P { x: int, y: int }

fn main() {
    let f: (bool) -> P? = present => if present { P { x: 5, y: 6 } } else { null }
    print("{f(true)?.x ?? -1}")
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src).expect("must emit");
    common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn a_returned_lambda_at_a_nullable_nobox_return_reads_the_next_leaf_sentinel() {
    let got = run(RETURNED_LAMBDA, "fnval_returned_lambda");
    assert_eq!(
        got, "6",
        "PINNED BEHAVIOR (BUG): `f(true)?.x` through a returned lambda is \
         expected to answer `6` (the value of `y`). It answered `{got}`, so the \
         fix landed -- flip this to assert `5`, matching the controls."
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

/// The asymmetry that localizes what is left: this form is CORRECT. A lambda in
/// a LET is checked against the declared type and records it, so both ends of
/// the fn value read `(bool) -> P?` and the body emits its presence slot. A
/// lambda in RETURN position never reaches `check_expr` -- `Expr::Return` infers
/// it and coerces -- so it records its own narrower `(bool) -> P` and emits two
/// leaves against a three-dest call site.
#[test]
fn the_same_lambda_bound_to_a_typed_local_is_correct() {
    assert_eq!(
        run(LOCALLY_BOUND_LAMBDA, "fnval_local_lambda"),
        "5",
        "a locally-bound lambda at a nullable nobox return must read the right field"
    );
}
