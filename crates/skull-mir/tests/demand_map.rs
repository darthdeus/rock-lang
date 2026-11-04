//! The specialization demand map's agreement check.
//!
//! D-1 lets a specialization demand carry the bound-conformance tree the callee
//! body's placeholders resolve through, keyed by the EMITTED BODY. That is sound
//! only because the tree is a function of the instantiation rather than of the
//! caller -- so the map checks it instead of asserting it.
//!
//! The corpus supplies no disagreeing pair (that is the point), which is exactly
//! why the check needs a constructed one: a zero over an empty population says
//! nothing about whether the check can fire at all. Both sides are here -- a
//! repeat that AGREES must stay silent, or the check would reject every ordinary
//! program with two call sites at one instantiation.

use skull_mir::bound::{DemandMap, ResolvedBoundNext, ResolvedBoundTree, SpecDemand};
use skull_mir::instance::Instance;
use skull_mir::mir::FuncRef;
use skull_mir::monoty::MonoInterner;
use skull_mir::monoty::{MonoCtor, MonoTy};
use ustr::Ustr;

fn nominal(name: &str) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(Ustr::from(name)),
        params: Vec::new(),
    }
}

fn fref(n: u32) -> FuncRef {
    FuncRef {
        module: bone_semantic::prelude::ModuleId::builtin(),
        func: comfy_la_arena::Idx::from_raw(comfy_la_arena::RawIdx::from(n)),
    }
}

/// A one-leaf tree resolving generic #0 to witness `witness`.
fn tree(i: &MonoInterner, witness: u32) -> ResolvedBoundTree {
    let mut t = ResolvedBoundTree::default();
    t.next.insert(
        0,
        ResolvedBoundNext::Concrete {
            instance: Instance::intern(i, fref(witness), vec![nominal("float")]),
            sub: ResolvedBoundTree::default(),
        },
    );
    t
}

fn interner() -> MonoInterner {
    MonoInterner::new()
}

fn inst(i: &MonoInterner, callee: u32, ty: &str) -> Instance {
    Instance::intern(i, fref(callee), vec![nominal(ty)])
}

fn demand(i: &MonoInterner, callee: u32, ty: &str, tree: ResolvedBoundTree) -> SpecDemand {
    SpecDemand {
        inst: inst(i, callee, ty),
        tree,
    }
}

/// Two sites demanding ONE emitted body with DIFFERENT trees is a clash: one
/// definition would have to serve two resolutions, and whichever demand arrived
/// first would silently win.
#[test]
fn two_sites_demanding_one_body_with_different_trees_clash() {
    let i = interner();
    let mut map = DemandMap::default();
    assert!(
        map.record(&[demand(&i, 1, "int", tree(&i, 7))]).is_empty(),
        "a first sighting is not a clash"
    );

    let clashes = map.record(&[demand(&i, 1, "int", tree(&i, 9))]);
    assert_eq!(
        clashes,
        vec![inst(&i, 1, "int")],
        "a second demand for the same emitted body with a different tree must clash, \
         and must name the body"
    );
}

/// The other side of admission, and the one that keeps the check usable: an
/// ordinary program has many call sites at one instantiation, and every repeat
/// after the first agrees. If those clashed the check would reject everything.
#[test]
fn a_repeated_demand_that_agrees_is_silent() {
    let i = interner();
    let mut map = DemandMap::default();
    map.record(&[demand(&i, 1, "int", tree(&i, 7))]);
    assert!(
        map.record(&[
            demand(&i, 1, "int", tree(&i, 7)),
            demand(&i, 1, "int", tree(&i, 7))
        ])
        .is_empty(),
        "agreeing repeats must be silent"
    );
}

/// Two INSTANTIATIONS of one declaration are two emitted bodies, so their trees
/// are allowed to differ -- that is the whole reason the tree is keyed by
/// emitted body rather than by declaration. Without this a `filter<int>` and a
/// `filter<string>` in one program would clash.
#[test]
fn two_instantiations_of_one_declaration_may_differ() {
    let i = interner();
    let mut map = DemandMap::default();
    let clashes = map.record(&[
        demand(&i, 1, "int", tree(&i, 7)),
        demand(&i, 1, "string", tree(&i, 9)),
    ]);
    assert!(
        clashes.is_empty(),
        "distinct instantiations are distinct bodies: {clashes:?}"
    );
    assert_eq!(map.tree_of(&inst(&i, 1, "int")), tree(&i, 7));
    assert_eq!(map.tree_of(&inst(&i, 1, "string")), tree(&i, 9));
}

/// An unrecorded body reads as the EMPTY tree rather than as an error here.
/// That is not a fallback: a body with no placeholders is correctly emitted
/// under an empty tree, and one WITH placeholders is rejected by
/// `skull_transform::bound::resolve_standalone_bound_calls`, which refuses a
/// placeholder it cannot resolve rather than emitting it.
#[test]
fn an_unrecorded_body_reads_as_the_empty_tree() {
    let i = interner();
    let map = DemandMap::default();
    assert!(map.tree_of(&inst(&i, 3, "int")).is_empty());
}
