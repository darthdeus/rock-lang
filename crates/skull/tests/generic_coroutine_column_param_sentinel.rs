//! SENTINEL -- a GENERIC `co fn` iterating a column-stored container param
//! iterates ZERO times. The loop body never runs, so the coroutine yields
//! nothing and the fn answers 0 for a 2-element vec. Compile-clean, no
//! rejection, silent wrong answer.
//!
//! `compute_call_spec` (`crates/skull-mir/src/lower/shape.rs`) bails out for
//! `is_coroutine` before it can build a `SpecKey`, so the generic body is
//! lowered ERASED -- it iterates the vec as though the boxed `_data`
//! representation were present, and a column vec has per-leaf arrays instead.
//! Unlike the ordinary generic path, nothing forces specialization here, and
//! unlike a rejection nothing tells the author.
//!
//! DISCRIMINANT, measured as a full 2x2 against `[P]` -- the defect is
//! generic AND coroutine, jointly:
//!
//! |            | non-generic | generic |
//! |------------|-------------|---------|
//! | plain `fn` | 2 (ok)      | 2 (ok)  |
//! | `co fn`    | 2 (ok)      | 0 (BUG) |
//!
//! Both single-axis controls are below, so a fix that moves only one cell is
//! visible. The bounding control on the ELEMENT is `[int]`: boxed, keeps
//! `_data`, and the same generic `co fn` answers correctly.
//!
//! It is NOT about the `nobox` mark: an UNMARKED value struct is column-eligible
//! on the same terms and fails identically. Column eligibility is wider than the
//! mark, so the blast radius is wider than nobox tests can see.
//!
//! A coroutine is an ordinary CFG after `coro_transform`, so nothing actually
//! prevents specializing one -- the `is_coroutine` bail-out is artificial, and
//! the correct fix is to let these instantiations specialize like any other
//! column-forced generic. A spanned rejection would also be correct, and is
//! strictly better than today's silence.
//!
//! PINNED BEHAVIOR (BUG): the sentinels assert `0`. When the fix lands they go
//! RED -- flip them to `2`, matching the controls, and delete this paragraph. If
//! the fix instead makes the shape REJECT, replace the runs with the `Err`
//! assertion it produces.

mod common;

/// The repro: generic `co fn`, column-stored element.
const GENERIC_CO_COLUMN: &str = r#"
nobox value P { x: float, y: float }

co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let xs: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    let c = walk(xs)
    let total = 0
    for v in c { total = v }
    print("{total}")
}
"#;

/// Blast-radius control: an UNMARKED value is column-eligible too.
const GENERIC_CO_UNMARKED: &str = r#"
value Q { x: float, y: float }

co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let xs: [Q] = [Q { x: 1.0, y: 0.0 }, Q { x: 2.0, y: 0.0 }]
    let c = walk(xs)
    let total = 0
    for v in c { total = v }
    print("{total}")
}
"#;

/// Control, one axis: drop GENERIC, keep the coroutine.
const CONCRETE_CO_COLUMN: &str = r#"
nobox value P { x: float, y: float }

co fn walk_concrete(v: [P]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let xs: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    let c = walk_concrete(xs)
    let total = 0
    for v in c { total = v }
    print("{total}")
}
"#;

/// Control, the other axis: keep GENERIC, drop the coroutine. This is the
/// column-forced specialization path working as intended.
const GENERIC_PLAIN_COLUMN: &str = r#"
nobox value P { x: float, y: float }

fn count_plain<T>(v: [T]) -> int {
    let n = 0
    for _e in v { n = n + 1 }
    return n
}

fn main() {
    let xs: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    print("{count_plain(xs)}")
}
"#;

/// Bounding control on the ELEMENT: boxed `[int]` through the same generic
/// `co fn`.
const GENERIC_CO_INT: &str = r#"
co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let xs: [int] = [10, 20]
    let c = walk(xs)
    let total = 0
    for v in c { total = v }
    print("{total}")
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src).expect("must emit");
    common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn a_generic_coroutine_over_a_column_vec_iterates_zero_times_sentinel() {
    let got = run(GENERIC_CO_COLUMN, "gen_co_col");
    assert_eq!(
        got, "0",
        "PINNED BEHAVIOR (BUG): a generic `co fn` over a column `[P]` is \
         expected to iterate zero times and answer `0`. It answered `{got}`, so \
         the fix landed -- flip this to `2`, matching the controls."
    );
}

/// Records the blast radius: a column-storage defect, not a `nobox` one.
#[test]
fn a_generic_coroutine_over_an_unmarked_value_column_fails_identically_sentinel() {
    let got = run(GENERIC_CO_UNMARKED, "gen_co_unmarked");
    assert_eq!(
        got, "0",
        "PINNED BEHAVIOR (BUG): an UNMARKED value element is column-eligible on \
         the same terms and must fail identically. It answered `{got}`."
    );
}

/// 2x2 cell: coroutine WITHOUT generic. Isolates genericity as load-bearing.
#[test]
fn a_non_generic_coroutine_over_the_same_column_vec_is_correct() {
    assert_eq!(
        run(CONCRETE_CO_COLUMN, "concrete_co_col"),
        "2",
        "a NON-generic `co fn` over the same column vec must stay correct -- \
         this is half of what pins the defect to the generic x coroutine cell"
    );
}

/// 2x2 cell: generic WITHOUT coroutine. The column-forced specialization path.
#[test]
fn a_generic_non_coroutine_over_the_same_column_vec_is_correct() {
    assert_eq!(
        run(GENERIC_PLAIN_COLUMN, "gen_plain_col"),
        "2",
        "a generic non-coroutine over the same column vec must stay correct -- \
         this is the other half, and it is the forced-specialization path"
    );
}

/// Bounds the sentinel on the element type. Without this, a fix that broke every
/// generic `co fn` would leave the sentinels green and read as progress.
#[test]
fn a_generic_coroutine_over_a_boxed_element_is_correct() {
    assert_eq!(
        run(GENERIC_CO_INT, "gen_co_int"),
        "2",
        "a boxed `[int]` element through the same generic `co fn` must stay correct"
    );
}
