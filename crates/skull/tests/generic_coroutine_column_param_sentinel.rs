//! A GENERIC `co fn` iterating a column-stored container param is REJECTED.
//!
//! It used to iterate ZERO times: the loop body never ran, the coroutine yielded
//! nothing, and the fn answered 0 for a 2-element vec -- compile-clean, no
//! diagnostic, a silent wrong answer. It is now a spanned rejection at the call
//! (`generic_coroutine_column_reject`).
//!
//! The rejection is a stop-gap for the SILENCE, not the repair. The right fix is
//! to let these instantiations specialize; when that lands, the rejection goes
//! with it and these cases become runs answering `2` like the controls.
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
//! THE BAIL-OUT IS NOT ARTIFICIAL, and an earlier revision of this file said it
//! was. Measured: deleting `is_coroutine` from `compute_call_spec`'s guard does
//! not let these specialize, it PANICS codegen with `Yield must be removed by
//! the coroutine transform before codegen`.
//!
//! "A coroutine is an ordinary CFG after `coro_transform`" is true and beside
//! the point. Specialization is not a CFG property -- it is DELIVERED by
//! inline-splicing a specialized clone, which is why eligibility here is a
//! deliberate subset of the inliner's `classify`, and a `co fn` cannot be
//! spliced into its caller at all: the call constructs a coroutine object rather
//! than running the body.
//!
//! So the repair is a mechanism that does not exist: emit the specialized clone
//! as its own top-level coroutine, run `coro_transform` over it, and retarget
//! the call. That is monomorphization-by-cloning, which is why this is mono's to
//! make and not a guard to delete.
//!
//! The controls are what keep the rejection NARROW: it must fire only where
//! `compute_call_spec` would have specialized but for that bail-out, so all three
//! still compile and answer `2`. Without them, a rejection that swallowed every
//! generic `co fn` would look like the same green.


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
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

/// The rejection's message, or a panic naming what it emitted instead.
fn reject(src: &str) -> String {
    match crate::common::try_emit_module(src) {
        Ok(_) => panic!(
            "expected a spanned rejection for a generic `co fn` at a column element --              emitting instead means the silent zero-iteration miscompile is back"
        ),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_generic_coroutine_over_a_column_vec_is_rejected() {
    let msg = reject(GENERIC_CO_COLUMN);
    assert!(
        msg.contains("GENERIC `co fn`") && msg.contains("COLUMN storage"),
        "the rejection must name both halves of the discriminant, since either \
         alone is a shape that works: {msg}"
    );
}

/// Records the blast radius: a column-storage rule, not a `nobox` one.
#[test]
fn a_generic_coroutine_over_an_unmarked_value_column_is_rejected_identically() {
    let msg = reject(GENERIC_CO_UNMARKED);
    assert!(
        msg.contains("GENERIC `co fn`"),
        "an UNMARKED value element is column-eligible on the same terms, so it \
         must reject identically -- column eligibility is wider than the mark: {msg}"
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
