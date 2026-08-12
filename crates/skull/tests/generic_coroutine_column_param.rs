//! A GENERIC `co fn` iterating a column-stored container param.
//!
//! It used to iterate ZERO times: the loop body never ran, the coroutine yielded
//! nothing, and the fn answered 0 for a 2-element vec -- compile-clean, no
//! diagnostic, a silent wrong answer. The coroutine now owns a concrete frame
//! and element representation plan, so it cannot lower erased and read a
//! `_data` that a column vec does not have.
//!
//! Specialization is DELIVERED by inline-splicing a specialized clone, and a
//! `co fn` cannot be spliced into its caller: the call constructs a coroutine
//! object rather than running the body. So the repair was a mechanism rather
//! than a guard to delete -- emit the clone as its own top-level coroutine and
//! put it through `coro_transform` (`skull::mir_emit::emit_coroutine_chunk`,
//! mono cut C+D step D-3). `crates/skull/tests/concrete_coroutine_emission.rs`
//! owns the emission-shape half; this file is the 2x2 that pins the DEFECT.
//!
//! DISCRIMINANT, a full 2x2 against `[P]` -- the defect was generic AND
//! coroutine, jointly, and every cell must now answer 2:
//!
//! |            | non-generic | generic |
//! |------------|-------------|---------|
//! | plain `fn` | 2           | 2       |
//! | `co fn`    | 2           | 2 (was 0) |
//!
//! Both single-axis controls are below, so a regression that moves only one cell
//! is visible. The bounding control on the ELEMENT is `[int]`: boxed, keeps
//! `_data`, and does not specialize -- so it holds the shared-body path.
//!
//! It is NOT about the `nobox` mark: an UNMARKED value struct is column-eligible
//! on the same terms and failed identically. Column eligibility is wider than the
//! mark, so the blast radius is wider than nobox tests can see.

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
/// `co fn`. Without this, a fix that broke every generic `co fn` would leave the
/// other cells green and read as progress.
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
    let lua = crate::common::try_emit_module(src)
        .unwrap_or_else(|e| panic!("must emit ({tag}): {}", e.message));
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn a_generic_coroutine_over_a_column_vec_iterates_its_elements() {
    assert_eq!(
        run(GENERIC_CO_COLUMN, "gen_co_col"),
        "2",
        "the instantiation must run against COLUMN storage: a shared erased body \
         reads a `_data` the vec does not have and answers 0 with no diagnostic"
    );
}

/// Records the blast radius: a column-storage rule, not a `nobox` one.
#[test]
fn a_generic_coroutine_over_an_unmarked_value_column_iterates_identically() {
    assert_eq!(
        run(GENERIC_CO_UNMARKED, "gen_co_unmarked"),
        "2",
        "an UNMARKED value element is column-eligible on the same terms, so it \
         must behave identically -- column eligibility is wider than the mark"
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

/// Bounds the fix on the element type: a boxed element keeps the SHARED body,
/// so this is the arm that says the repair did not simply specialize everything.
#[test]
fn a_generic_coroutine_over_a_boxed_element_is_correct() {
    assert_eq!(
        run(GENERIC_CO_INT, "gen_co_int"),
        "2",
        "a boxed `[int]` element through the same generic `co fn` must stay correct"
    );
}
