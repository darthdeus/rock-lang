//! An `Iterator`-bounded body emits STANDALONE at each instantiation
//!.
//!
//! A bounded fn's body holds placeholder `Inst::CallBoundMulti`s whose concrete
//! `next` is known per instantiation. Those are resolved from a
//! `ResolvedBoundTree`, and the tree reaches the body two ways: from the SITE
//! when the inliner splices it, and from the specialization DEMAND when it is
//! emitted on its own. One rewrite serves both
//! (`skull_transform::bound::substitute_bound_call`).
//!
//! With inlining DISABLED there is no splice, so this is the arm that proves the
//! demand path carries it -- and that is the plan's standing gate for E1, which
//! runs with inlining off by construction.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::monoty::{MonoCtor, MonoTy};
use ustr::Ustr;

use crate::common::{InlineOffGuard, definitions_matching, function_body};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// A user fn taking an `Iterator`-bounded generic, called at one concrete
/// chain. `total`'s body holds the placeholder; `main` is the concrete root.
const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

/// The same program with the bounded generic REMOVED -- `total` takes the
/// concrete list. The admission control: it must emit with inlining disabled,
/// so a rejection above is about the BOUND and not about the mode refusing to
/// emit anything at all.
const CONCRETE: &str = r#"
fn total(xs: [int]) -> int {
    let s = 0
    for v in xs { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs)}")
}
"#;

/// A bounded body whose resolved witness is ITSELF bounded. `collect`'s
/// placeholder resolves to `MapIter.next`, which holds a placeholder of its own
/// for the inner `VecIter`. Only the second level needs the sub-tree that a
/// resolved leaf carries, which is why one level of nesting cannot see this.
///
/// The terminal is what forces it: a `for` over the same chain drives `next`
/// from the concrete loop header, where the witness is in hand. `.collect()`
/// hands the chain to a bounded body, so the resolution has to travel.
const NESTED: &str = r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [
        Item { id: 1, p: 3.0 },
        Item { id: 2, p: 2.0 },
    ]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#;

fn emit(src: &str) -> Result<String, String> {
    let ws = check(src);
    let mid = skull::user_module_id(&ws).expect("user module");
    skull::mir_emit::emit_workspace_mir(&ws, mid)
        .map(|e| e.lua)
        .map_err(|e| e.message)
}

fn emit_err(src: &str) -> Option<String> {
    emit(src).err()
}

/// The lines of the top-level Lua function whose header line is `header`.
/// The lines of the emitted definition `symbol`, read through the shape-aware
/// harness helper: under the bound callee shape a definition's header is
/// `__rock_mk_<symbol> = function(..) return function(..)`, so a scan for
/// `function <symbol>` at column 0 finds nothing.
fn body_of<'a>(lua: &'a str, symbol: &str) -> Vec<&'a str> {
    function_body(lua, symbol)
        .lines()
        .skip(1)
        .filter(|l| *l != "end" && *l != "end end")
        .collect()
}

/// The one emitted specialization whose name starts with `prefix` and carries a
/// `_spec_` instance suffix.
fn specialization<'a>(lua: &'a str, prefix: &str, what: &str) -> &'a str {
    let found: Vec<&str> = definitions_matching(lua, "_spec_")
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .collect();
    assert_eq!(found.len(), 1, "expected one standalone {what} in:\n{lua}");
    found[0]
}

/// Three separate things, because each alone is satisfied by a broken build.
///
/// That emission SUCCEEDS says nothing on its own -- a build that dropped
/// `total` entirely also reports no error. That a DEFINITION exists is checked
/// on the definition line, which the caller's own call cannot satisfy. And that
/// the definition RESOLVED its placeholder is checked by the concrete `next` in
/// its body: the whole content of D-1 is that substitution, and a definition
/// emitted with the placeholder still in it would pass the first two.
#[test]
fn a_bounded_body_emits_standalone_with_inlining_disabled() {
    let _inline = InlineOffGuard::on();
    let lua = emit(BOUNDED).expect("a bounded body must emit standalone under its demand");
    let symbol = specialization(&lua, "total_", "specialized definition of `total`");
    let body = body_of(&lua, symbol);
    assert!(
        body.iter().any(|l| l.contains("next_")),
        "the standalone `total` calls no concrete `next`, so its placeholder bound-call \
         was not resolved: {body:?}"
    );
    // A call site, not a header. Under the bound shape the caller names the
    // symbol as a factory PARAMETER too, which is a binding, not a call.
    let called = lua.lines().any(|l| {
        !l.starts_with("function ")
            && !l.starts_with(skull_codegen_lua::CALLEE_FACTORY_PREFIX)
            && l.contains(&format!("{symbol}("))
    });
    assert!(called, "`{symbol}` is emitted but nothing calls it");
}

/// A resolved witness that is itself bounded resolves too, one level down.
///
/// The demand a substitution files for its witness must carry the witness's OWN
/// tree -- the `sub` of the leaf that resolved it. Dropping it files the demand
/// with an EMPTY tree, and `MapIter.next` then reaches its standalone emission
/// with nothing to resolve generic #0 from. The splice path cannot see this: it
/// recurses into the callee in place, so the sub-tree never has to travel.
///
/// Asserting the emitted `MapIter.next` calls a concrete `next` is the arm that
/// distinguishes a real resolution from an emission that merely succeeded --
/// the same reason the single-level test checks the callee and not just the
/// absence of an error.
#[test]
fn a_witness_that_is_itself_bounded_resolves_from_the_leafs_sub_tree() {
    let _inline = InlineOffGuard::on();
    let lua = emit(NESTED).expect("a bounded witness of a bounded body must emit standalone");
    let symbol = specialization(&lua, "next_MapIter_", "`MapIter.next` specialization");
    let body = body_of(&lua, symbol);
    assert!(
        body.iter().any(|l| l.contains("next_VecIter")),
        "`MapIter.next` emitted without resolving its own placeholder to the inner \
         `VecIter.next`: {body:?}"
    );
}

/// The admission control for the sentinel. Not inverted: this asserts the
/// CORRECT behavior and must stay green through D-1.
///
/// Without it the sentinel could be green because the mode rejects every
/// program, which would say nothing about bounded bodies.
#[test]
fn an_unbounded_body_emits_with_inlining_disabled() {
    let _inline = InlineOffGuard::on();
    let err = emit_err(CONCRETE);
    assert!(
        err.is_none(),
        "the same program without a bounded generic must still emit with \
         inlining disabled, or the sentinel cannot attribute its rejection to \
         the bound; got {err:?}"
    );
}

fn first_call_multi_arity(mf: &MirFunc) -> usize {
    mf.insts
        .values()
        .find_map(|inst| match inst {
            Inst::CallMulti { dests, .. } => Some(dests.len()),
            _ => None,
        })
        .expect("resolved bounded body has no concrete next call")
}

#[test]
fn standalone_bound_rewrite_consumes_the_selected_result_plan() {
    let ws = check(
        r#"
nobox value Pair { x: int, y: int }

fn sum_all<I: Iterator<int>>(it: I) -> int {
    let total = 0
    for v in it { total = total + v }
    return total
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{sum_all(xs.iter())}")
}
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let graph = skull::instance_graph::build(&ws, roots, false);
    let bodies = skull::concrete_body::build(&ws, &graph);
    let (_inst, plan) = bodies
        .plans
        .iter()
        .find(|(inst, plan)| {
            ws.module(inst.func.module).functions[inst.func.func]
                .name
                .as_str()
                == "sum_all"
                && !plan.bound_tree.is_empty()
        })
        .expect("no standalone bounded instance with a resolved tree");
    let witness = plan
        .bound_tree
        .next
        .values()
        .find_map(|next| match next {
            skull_mir::bound::ResolvedBoundNext::Concrete { instance, .. } => {
                Some(instance.clone())
            }
            skull_mir::bound::ResolvedBoundNext::CallerGeneric(_) => None,
        })
        .expect("bounded plan has no concrete witness");

    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(graph.interner().clone());
    let mut intact = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .expect("intact bounded body did not lower");
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        graph.interner(),
        &mut intact,
        &plan.bound_tree,
        &bodies.plans,
    )
    .expect("intact bounded body did not resolve");
    assert_eq!(
        first_call_multi_arity(&intact),
        2,
        "the control lost (ok, int)"
    );

    let mut poisoned_plans = (*bodies.plans).clone();
    let pair = MonoTy::App {
        ctor: MonoCtor::Nominal(Ustr::from("Pair")),
        params: Vec::new(),
    };
    let wrong_result = graph
        .interner()
        .intern(MonoTy::Tuple(vec![MonoTy::Bool, pair]));
    let witness_plan = std::sync::Arc::make_mut(
        poisoned_plans
            .get_mut(&witness)
            .expect("witness plan disappeared"),
    );
    std::sync::Arc::make_mut(&mut witness_plan.signature).result = Some(wrong_result);
    let mut poisoned_owner = (**plan).clone();
    for (site, callee) in &poisoned_owner.callees {
        if callee == &witness {
            std::sync::Arc::make_mut(
                poisoned_owner
                    .callee_signatures
                    .get_mut(site)
                    .expect("witness signature disappeared"),
            )
            .result = Some(wrong_result);
        }
    }
    let mut poisoned =
        skull_mir::lower::lower_instance_with_plans(&ws, &scx, &poisoned_owner, &poisoned_plans)
            .expect("plan mutation made the bounded body fail before rewrite");
    skull_transform::bound::resolve_standalone_bound_calls(
        &ws,
        &scx,
        graph.interner(),
        &mut poisoned,
        &poisoned_owner.bound_tree,
        &poisoned_plans,
    )
    .expect("poisoned bounded body did not resolve");
    assert_eq!(
        first_call_multi_arity(&poisoned),
        3,
        "bound rewriting ignored the selected concrete result plan"
    );
}
