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
//! the same test covers both the direct index and iterator paths.
//!
//! MARKED and UNMARKED are legitimately two mechanisms here, not one to unify:
//! marked splits leaves at LOWERING (`nobox_shape_of_ty`, re-applying the `?`),
//! unmarked does no leaf splitting at all and is only opportunistically
//! scalarized by SROA afterwards. Only the nullability QUERY was duplicated.


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

fn run_ok(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    let (ok, out) = run_capturing(&format!("{lua}\nmain()\n"), tag);
    assert!(ok, "program must run:\n{out}");
    out.trim().to_string()
}

/// The minimal form: no loop, no iterator, no columns. Just a generic bound to
/// `BV2?` whose `let` escapes.
const GENERIC_LET: &str = r#"
value BV2 { x: float, y: float }

fn first<T>(a: [T], i: int) -> T { let v = a[i]  return v }

fn main() {
    let a: [BV2?] = [BV2 { x: 1.0, y: 1.0 }, null, BV2 { x: 2.0, y: 2.0 }]
    print("{first(a, 1) == null} {first(a, 0)?.x ?? -1.0}")
}
"#;

/// The shape the original sentinel pinned -- iteration, which reaches the same
/// `let` through `VecIter<T>.next`.
const ITERATE: &str = r#"
value BV2 { x: float, y: float }

fn main() {
    let a: [BV2?] = [BV2 { x: 1.0, y: 1.0 }, null, BV2 { x: 2.0, y: 2.0 }]
    let t = 0.0
    for v in a { if v != null { t = t + v.x } }
    print("{t as int}")
}
"#;

/// Storage was never the defect: the same data read by INDEX was always correct.
const INDEXED: &str = r#"
value BV2 { x: float, y: float }

fn main() {
    let a: [BV2?] = [BV2 { x: 1.0, y: 1.0 }, null, BV2 { x: 2.0, y: 2.0 }]
    print("{a[1] == null} {a.get(1) == null} {a[2] != null} {a.count()}")
}
"#;

/// The control that located the defect while it was open: the NON-generic twin
/// lowers to structurally identical MIR and always stayed boxed. The flag was
/// the only differing input.
const NON_GENERIC: &str = r#"
value BV2 { x: float, y: float }

fn pick(a: [BV2?], i: int) -> (bool, BV2?) { let v = a[i]  return (true, v) }

fn main() {
    let a: [BV2?] = [BV2 { x: 1.0, y: 1.0 }, null]
    let (ok, v) = pick(a, 1)
    print("{ok} {v == null}")
}
"#;

#[test]
fn a_generic_bound_to_a_nullable_value_keeps_its_absent_row() {
    assert_eq!(run_ok(GENERIC_LET, "nullable_generic_let"), "true 1");
}

#[test]
fn iterating_an_absent_row_works() {
    assert_eq!(run_ok(ITERATE, "nullable_generic_iter"), "3");
}

#[test]
fn indexed_reads_are_correct() {
    assert_eq!(run_ok(INDEXED, "nullable_generic_indexed"), "true true true 3");
}

#[test]
fn the_non_generic_twin_is_correct() {
    assert_eq!(run_ok(NON_GENERIC, "nullable_generic_nongeneric"), "true true");
}

/// THE PROPERTY THE FIX ENDANGERED. `nonnull_value_struct` exists to LICENSE
/// SROA; narrowing it could switch non-nullable values back to boxed emission
/// and every assertion above would still pass. So assert on the EMITTED code:
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
    assert_eq!(run_ok(SRC, "nullable_generic_nonnull"), "3");
}
