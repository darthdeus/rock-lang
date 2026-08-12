//! D-3: a generic `co fn` is emitted CONCRETELY, one body per instantiation.
//!
//! The concrete graph used to be bypassed for coroutine bodies, so every
//! instantiation shared one erased body and one set of `__step`/`__resume`
//! symbols. Two things followed, and they are the two arms here:
//!
//! - A COLUMN-stored instantiation would read a `_data` that its representation
//!   does not have, so the shared body is WRONG rather than slow. It must
//!   specialize and run; `generic_coroutine_column_param.rs` owns that 2x2.
//! - Two instantiations must own two SETS of helper symbols.
//!   `InstanceGraph::colliding_coroutine_helpers` is the graph-side oracle for
//!   the same fact; this file asserts it on the emitted text, which is where a
//!   collision actually clobbers a definition.
//!
//! The controls bound the change on both axes: a NON-generic `co fn` over the
//! same column vec, and the same generic `co fn` over a BOXED element, must both
//! keep working. Under instance-owned emission the boxed one owns a family per
//! instantiation too -- there is no shared generic body to fall back to.

use std::collections::BTreeSet;
use crate::common;

/// One generic `co fn`, two column-stored instantiations with different element
/// types AND different lengths, so a body serving both would have to answer both
/// counts.
const TWO_COLUMN_INSTANCES: &str = r#"
nobox value P { x: float, y: float }
nobox value Q { a: float }

co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let ps: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    let qs: [Q] = [Q { a: 1.0 }, Q { a: 2.0 }, Q { a: 3.0 }]
    let cp = walk(ps)
    let tp = 0
    for v in cp { tp = v }
    let cq = walk(qs)
    let tq = 0
    for v in cq { tq = v }
    print("{tp} {tq}")
}
"#;

/// Control, one axis: drop GENERIC, keep the coroutine over a column vec.
const CONCRETE_CO_COLUMN: &str = r#"
nobox value P { x: float, y: float }

co fn walk_p(v: [P]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let ps: [P] = [P { x: 1.0, y: 0.0 }, P { x: 2.0, y: 0.0 }]
    let c = walk_p(ps)
    let total = 0
    for v in c { total = v }
    print("{total}")
}
"#;

/// Control, the other axis: keep GENERIC, drop the column storage. Both
/// instantiations must run and own their own helper families.
const GENERIC_CO_BOXED: &str = r#"
co fn walk<T>(v: [T]) -> Coroutine<int, int, ()> {
    let n = 0
    for _e in v { n = n + 1; yield n }
    return n
}

fn main() {
    let xs: [int] = [10, 20]
    let ys: [string] = ["a", "b", "c"]
    let cx = walk(xs)
    let tx = 0
    for v in cx { tx = v }
    let cy = walk(ys)
    let ty = 0
    for v in cy { ty = v }
    print("{tx} {ty}")
}
"#;

const GENERIC_CO_CONCRETE_RESULT_FALLOFF: &str = r#"
co fn missing<T>(value: T) -> Coroutine<int, T> {
    yield 1
}

fn main() {
    let _c = missing(1)
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src)
        .unwrap_or_else(|e| panic!("must emit ({tag}): {}", e.message));
    common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

/// Every top-level `function <name>__step(` the program defines, in text order.
fn step_definitions(lua: &str) -> Vec<String> {
    lua.match_indices("__step(")
        .filter_map(|(i, _)| {
            let head = &lua[..i];
            let start = head.rfind("function ")? + "function ".len();
            let name = &lua[start..i];
            (!name.contains(['(', ')', ' ', '\n'])).then(|| name.to_string())
        })
        .collect()
}

/// The load-bearing arm. Two column instantiations of one generic `co fn`, each
/// answering its OWN length.
#[test]
fn two_column_instantiations_of_one_generic_coroutine_each_answer_their_own_length() {
    assert_eq!(
        run(TWO_COLUMN_INSTANCES, "two_col_co"),
        "2 3",
        "each instantiation of a generic `co fn` must run against its own \
         element's representation: `[P]` has 2 elements and `[Q]` has 3, and a \
         shared erased body reading a `_data` neither column vec has answers 0 \
         for both"
    );
}

/// The symbol half: a shared `__step` means the second definition clobbers the
/// first, so the emitted text must carry one per instantiation.
///
/// The declaration's own shared snapshot is emitted too -- every declaration
/// gets a chunk, specialized or not, which is the same shape an ordinary generic
/// has. What must not happen is two definitions of ONE symbol.
#[test]
fn each_column_instantiation_owns_its_own_step_symbol() {
    let lua = common::try_emit_module(TWO_COLUMN_INSTANCES)
        .unwrap_or_else(|e| panic!("must emit: {}", e.message));
    let steps: Vec<String> =
        step_definitions(&lua).into_iter().filter(|n| n.starts_with("walk")).collect();
    let specialized: Vec<&String> = steps.iter().filter(|n| n.contains("_spec_")).collect();
    assert_eq!(
        specialized.len(),
        2,
        "each column instantiation must own a `__step` of its own; got {steps:?}"
    );
    let distinct: std::collections::BTreeSet<&String> = steps.iter().collect();
    assert_eq!(
        steps.len(),
        distinct.len(),
        "no `__step` symbol may be defined twice -- the later definition would \
         clobber the earlier one: {steps:?}"
    );
}

/// 2x2 cell: coroutine WITHOUT generic over the same column vec. Keeps the arm
/// above attributable to genericity.
#[test]
fn a_non_generic_coroutine_over_a_column_vec_is_correct() {
    assert_eq!(run(CONCRETE_CO_COLUMN, "concrete_co_col"), "2");
}

/// Every top-level definition whose symbol starts with `prefix`, and every
/// symbol with that prefix the chunk REFERENCES anywhere but its own definition
/// header. The two are compared, so a body that is defined and never reached, or
/// reached and never defined, is visible.
///
/// REFERENCED, not called: a coroutine's helpers are reached as VALUES --
/// `pcall(..__step, s, arg)` and `__resume = ..__resume` -- so a call-shaped
/// probe sees only the two ctors and reports four phantom orphans.
fn defined_and_referenced(lua: &str, prefix: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut defined = BTreeSet::new();
    let mut referenced = BTreeSet::new();
    for (i, _) in lua.match_indices(prefix) {
        let rest = &lua[i..];
        let len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = rest[..len].to_string();
        if lua[..i].ends_with("function ") {
            defined.insert(name);
        } else {
            referenced.insert(name);
        }
    }
    (defined, referenced)
}

/// 2x2 cell: generic WITHOUT column storage. Each instantiation owns its own
/// family -- emission is instance-owned, so a boxed element is not a reason to
/// share a body, and there is no shared generic body left to share.
///
/// **Runtime output cannot carry this test.** Both instantiations have the SAME
/// boxed ABI, so a call routed to the wrong family still returns a well-typed
/// answer of the right shape -- and here the two lengths differ only because the
/// fixture feeds different-length inputs. Same-ABI misrouting is invisible to
/// `"2 3"` and invisible to a count of definitions. So the evidence is the exact
/// set equality between what is DEFINED and what is REFERENCED: every emitted family
/// member is reached, and every reached symbol exists. A missing definition, a
/// definition nothing routes to, and two families collapsing onto one symbol all
/// break it.
#[test]
fn a_generic_coroutine_over_boxed_elements_owns_a_family_per_instance() {
    assert_eq!(run(GENERIC_CO_BOXED, "gen_co_boxed"), "2 3");
    let lua = common::try_emit_module(GENERIC_CO_BOXED).expect("must emit");

    let (defined, referenced) = defined_and_referenced(&lua, "walk_");
    // Non-empty and EXACT: a zero-over-zero equality is the vacuous pass this
    // shape invites, so the population is asserted first. Measured: two
    // instantiations x (ctor, `__resume`, `__step`).
    assert_eq!(
        defined.len(),
        6,
        "expected a ctor/`__resume`/`__step` family per instantiation; defined {defined:?}"
    );
    assert_eq!(
        defined, referenced,
        "every emitted `walk` body must be reached and every reached symbol must \
         exist -- defined {defined:?}, referenced {referenced:?}"
    );

    let steps: Vec<String> =
        step_definitions(&lua).into_iter().filter(|n| n.starts_with("walk")).collect();
    let distinct: BTreeSet<&String> = steps.iter().collect();
    assert_eq!(steps.len(), 2, "each instantiation must own a `__step`; got {steps:?}");
    assert_eq!(
        steps.len(),
        distinct.len(),
        "no `__step` symbol may be defined twice -- the later definition would \
         clobber the earlier one: {steps:?}"
    );
}

#[test]
fn a_concrete_coroutine_result_rejects_a_reachable_falloff() {
    let err = common::try_emit_module(GENERIC_CO_CONCRETE_RESULT_FALLOFF)
        .expect_err("a concrete non-unit coroutine result cannot fall off");
    assert!(
        err.message.contains("can fall off") && err.message.contains("missing"),
        "{err:?}"
    );
}
