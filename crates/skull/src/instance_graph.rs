//! The SHADOW instance graph (`docs/monomorphization-plan.md` Stage 0d).
//!
//! Read-only with respect to emission: it observes what a monomorphizing
//! compiler WOULD instantiate, and changes nothing about what is emitted today.
//!
//! An [`Instance`] is `(FuncRef, Vec<MonoTyId>)` -- a function plus the total
//! logical types of its generic arguments, keyed on
//! [`MonoTy`](skull_mir::monoty::MonoTy) from Stage 0c rather than on `SpecKey`.
//! That is the whole point of the probe: `SpecKey` is partial (plan S 2a), so
//! `f<Vec2, unit>` and `f<Vec2, any>` share its identity while they get distinct
//! `InstanceId`s here.
//!
//! **Roots come from production.** [`super::mir_emit::is_emission_root`] is the
//! same predicate `reachable_fns` seeds its walk with; re-deriving it would make
//! any difference between this graph and the real one an artifact of the copy.
//!
//! **Transitive instantiation is the core.** Walking a callee's body under the
//! CALLER's substitution is what makes `count<I>` at `I = VecIter<Vec2>` a
//! different instance from the same fn at `VecIter<int>`, and it is what today's
//! conditional specializer only does when `any_value` happens to hold.
//!
//! What this round does NOT yet cover, stated so a reader does not mistake the
//! histogram for the whole graph: Rock functions used as VALUES, coroutine
//! constructor/step dependencies as distinct edges, and the compiler-generated
//! `stdlib_funcref` literal-name set (catalogued in `docs/boundary-contracts.md`
//! S 1.4). Those are round-2 edge kinds; every edge below is real, the set is
//! incomplete, and [`InstanceGraph::uninstantiated_roots`] records the generic
//! roots no instantiation reached.

use std::collections::{BTreeMap, BTreeSet};

use bone_semantic::hir::{BodyOwner, Function};
use bone_semantic::inference::{BodyInference, Workspace};
use bone_semantic::prelude::{BoundNext, ModuleId, Span};
use comfy_la_arena::Idx;
use rustc_hash::FxHashMap;
use skull_mir::mangle::mangle_fn;
use skull_mir::monoty::{MonoArena, MonoSubst, MonoTyId, MonoTyReason};
use skull_mir::mir::FuncRef;

/// A function at one concrete instantiation. Non-generic functions get an
/// instance too (empty `args`), so no consumer can reintroduce `if generic`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Instance {
    pub func: FuncRef,
    pub args: Vec<MonoTyId>,
}

/// `FuncRef` is not `Ord`, and ordering by it would be ordering by arena
/// position anyway. The key is `(module, function index, args)` -- stable within
/// a build, and only ever used to make iteration order deterministic; the
/// MANIFEST is sorted by rendered symbol, which is stable across builds.
fn ord_key(i: &Instance) -> (u32, u32, &[MonoTyId]) {
    (i.func.module.raw(), i.func.func.into_raw().into_u32(), &i.args)
}

impl Ord for Instance {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        ord_key(self).cmp(&ord_key(other))
    }
}

impl PartialOrd for Instance {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Why one instance depends on another. The plan's edge kinds; the histogram of
/// these is what makes an absent edge class visible.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum EdgeKind {
    /// A statically resolved direct call.
    DirectCall,
    /// A direct call whose callee is generic and whose instantiation came from
    /// the caller's substitution rather than from the call site alone. The
    /// transitive-instantiation edge.
    GenericCall,
    /// The `iter()` half of a desugared `for`.
    IteratorIter,
    /// The `next()` witness of a desugared `for`, resolved at instantiation
    /// (plan S 12).
    IteratorNext,
}

impl EdgeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::DirectCall => "direct-call",
            Self::GenericCall => "generic-call",
            Self::IteratorIter => "iterator-iter",
            Self::IteratorNext => "iterator-next",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: Instance,
    pub to: Instance,
    pub kind: EdgeKind,
    pub span: Span,
}

/// A call the walk could not turn into an instance. Data, not an error: an
/// unbound generic here means the caller is itself an uninstantiated shared
/// body, which is exactly the surface Stage 2 must supply.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub from: Instance,
    pub callee: FuncRef,
    pub reason: &'static str,
    pub span: Span,
}

pub struct InstanceGraph {
    pub arena: MonoArena,
    pub instances: BTreeSet<Instance>,
    pub edges: Vec<Edge>,
    pub unresolved: Vec<Unresolved>,
    /// Generic functions that are roots but that no call instantiates -- they
    /// have no concrete instance to emit, which is the shared-generic-body
    /// surface stated as a count.
    pub uninstantiated_roots: BTreeSet<String>,
    /// Instances whose expansion was cut by [`MAX_INSTANTIATION_DEPTH`].
    pub depth_capped: Vec<Instance>,
}

/// Polymorphic recursion (`f<T> -> f<[T]>`) does not terminate under
/// demand-driven instantiation (plan S 7). The cap turns divergence into a
/// REPORT: a capped instance is a changing-instance recursion candidate.
///
/// It caps the STRUCTURAL DEPTH of the instantiation types, not the path length
/// from a root. A path length is a property of how the walk arrived, so capping
/// on it makes graph MEMBERSHIP depend on discovery order -- measured: with a
/// path cap, `games/little-knights` produced two different manifests under
/// forward and reversed root order, failing plan S 20g.4. Type depth is a
/// property of the instance itself, so the same instance is capped or not
/// regardless of how it was reached.
pub const MAX_INSTANTIATION_DEPTH: u32 = 12;

impl InstanceGraph {
    /// The deterministic symbol for an instance: the function's mangled name
    /// plus its rendered logical arguments. Sorting by this is what makes the
    /// manifest byte-stable regardless of discovery order.
    pub fn symbol(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        if i.args.is_empty() {
            return base;
        }
        let args: Vec<String> = i.args.iter().map(|a| self.arena.render(*a)).collect();
        format!("{base}<{}>", args.join(", "))
    }

    /// Instances sorted by symbol -- the manifest order.
    pub fn sorted_symbols(&self, ws: &Workspace) -> Vec<String> {
        let mut v: Vec<String> = self.instances.iter().map(|i| self.symbol(ws, i)).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn edge_histogram(&self) -> BTreeMap<&'static str, usize> {
        let mut h = BTreeMap::new();
        for e in &self.edges {
            *h.entry(e.kind.name()).or_insert(0) += 1;
        }
        h
    }

    /// Recursive strongly-connected components over the instance graph, split
    /// by whether the recursion PRESERVES its instance arguments.
    ///
    /// Same-instance recursion is ordinary and finite. Changing-instance
    /// recursion is the unbounded case the plan proposes to reject; it shows up
    /// here as a depth cap rather than as a cycle, because the walk never
    /// returns to the same instance.
    pub fn recursion_census(&self, ws: &Workspace) -> RecursionCensus {
        // Adjacency over instances.
        let mut succ: FxHashMap<&Instance, Vec<&Instance>> = FxHashMap::default();
        for e in &self.edges {
            succ.entry(&e.from).or_default().push(&e.to);
        }
        let mut direct = BTreeSet::new();
        let mut mutual = BTreeSet::new();

        // Direct: an instance that calls itself.
        for (from, tos) in &succ {
            if tos.iter().any(|t| *t == *from) {
                direct.insert(self.symbol(ws, from));
            }
        }
        // Mutual: any cycle of length > 1, found by an iterative DFS with a
        // path stack. Bounded by the instance count, which is small.
        let mut color: FxHashMap<&Instance, u8> = FxHashMap::default();
        let mut path: Vec<&Instance> = Vec::new();
        let mut order: Vec<&Instance> = self.instances.iter().collect();
        order.sort();
        for start in order {
            if color.get(start).copied().unwrap_or(0) != 0 {
                continue;
            }
            dfs(start, &succ, &mut color, &mut path, &mut |cycle: &[&Instance]| {
                if cycle.len() > 1 {
                    for n in cycle {
                        mutual.insert(self.symbol(ws, n));
                    }
                }
            });
        }
        // A symbol in both is reported as direct only -- the stronger fact.
        for d in &direct {
            mutual.remove(d);
        }
        RecursionCensus {
            direct_same_instance: direct,
            mutual_same_instance: mutual,
            changing_instance: self
                .depth_capped
                .iter()
                .map(|i| self.symbol(ws, i))
                .collect(),
        }
    }
}

fn dfs<'a>(
    node: &'a Instance,
    succ: &FxHashMap<&'a Instance, Vec<&'a Instance>>,
    color: &mut FxHashMap<&'a Instance, u8>,
    path: &mut Vec<&'a Instance>,
    on_cycle: &mut impl FnMut(&[&'a Instance]),
) {
    color.insert(node, 1);
    path.push(node);
    for &next in succ.get(node).map(|v| v.as_slice()).unwrap_or(&[]) {
        match color.get(next).copied().unwrap_or(0) {
            0 => dfs(next, succ, color, path, on_cycle),
            1 => {
                if let Some(at) = path.iter().position(|p| *p == next) {
                    on_cycle(&path[at..]);
                }
            }
            _ => {}
        }
    }
    path.pop();
    color.insert(node, 2);
}

#[derive(Debug, Default)]
pub struct RecursionCensus {
    pub direct_same_instance: BTreeSet<String>,
    pub mutual_same_instance: BTreeSet<String>,
    /// Instances whose expansion hit the depth cap -- changing-instance
    /// (polymorphic) recursion candidates.
    pub changing_instance: BTreeSet<String>,
}

/// Build the shadow graph for `ws`.
///
/// `roots` is supplied by the caller so this shares production's root
/// discovery instead of re-deriving it. `perturb` reverses the seeding order;
/// the manifest must be identical either way (plan S 20g.4).
pub fn build(ws: &Workspace, mut roots: Vec<FuncRef>, perturb: bool) -> InstanceGraph {
    roots.sort_by_key(|r| (r.module.raw(), r.func.into_raw().into_u32()));
    roots.dedup();
    if perturb {
        roots.reverse();
    }

    let mut g = InstanceGraph {
        arena: MonoArena::new(),
        instances: BTreeSet::new(),
        edges: Vec::new(),
        unresolved: Vec::new(),
        uninstantiated_roots: BTreeSet::new(),
        depth_capped: Vec::new(),
    };

    // A root that is generic has no instantiation of its own; it is a shared
    // body until a call supplies one. Recorded rather than invented.
    let mut work: Vec<(Instance, MonoSubst, u32)> = Vec::new();
    for r in roots {
        let f = &ws.module(r.module).functions[r.func];
        if !f.generics.is_empty() {
            g.uninstantiated_roots.insert(mangle_fn(ws, f));
            continue;
        }
        let inst = Instance { func: r, args: Vec::new() };
        if g.instances.insert(inst.clone()) {
            work.push((inst, MonoSubst::default(), 0));
        }
    }

    while let Some((inst, subst, depth)) = work.pop() {
        let ty_depth =
            inst.args.iter().map(|a| g.arena.depth(*a)).max().unwrap_or(0);
        if ty_depth > MAX_INSTANTIATION_DEPTH {
            g.depth_capped.push(inst);
            continue;
        }
        let Some(inf) = body_inference(ws, inst.func) else { continue };
        let module = ws.module(inst.func.module);
        let body = module.bodies.get(&BodyOwner::Function(inst.func.func));

        let record = |g: &mut InstanceGraph,
                          work: &mut Vec<(Instance, MonoSubst, u32)>,
                          callee: FuncRef,
                          bindings: Option<&Vec<bone_semantic::prelude::TyId>>,
                          kind: EdgeKind,
                          span: Span| {
            let cf = &ws.module(callee.module).functions[callee.func];
            let (args, callee_subst) =
                match instantiate(&mut g.arena, ws, inf, cf, bindings, &subst) {
                    Ok(v) => v,
                    Err(reason) => {
                        g.unresolved.push(Unresolved {
                            from: inst.clone(),
                            callee,
                            reason,
                            span,
                        });
                        return;
                    }
                };
            let to = Instance { func: callee, args };
            g.edges.push(Edge { from: inst.clone(), to: to.clone(), kind, span });
            if g.instances.insert(to.clone()) {
                work.push((to, callee_subst, depth + 1));
            }
        };

        // Direct + generic calls.
        let mut calls: Vec<(Idx<bone_semantic::hir::Expr>, (ModuleId, Idx<Function>))> =
            inf.resolved_overload_calls.iter().map(|(e, r)| (*e, *r)).collect();
        calls.sort_by_key(|(e, _)| *e);
        for (e, (m, f)) in calls {
            let callee = FuncRef { module: m, func: f };
            let bindings = inf.call_generic_bindings.get(&e);
            let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
            let kind =
                if bindings.is_some() { EdgeKind::GenericCall } else { EdgeKind::DirectCall };
            record(&mut g, &mut work, callee, bindings, kind, span);
        }

        // Synthesized iterator operations (plan S 12).
        let mut iters: Vec<(Idx<bone_semantic::hir::Expr>, &bone_semantic::prelude::ForIterInfo)> =
            inf.for_iter_protocol.iter().map(|(e, i)| (*e, i)).collect();
        iters.sort_by_key(|(e, _)| *e);
        for (e, info) in iters {
            let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
            // The `iter` witness has NO recorded instantiation anywhere: bone
            // records `next_generics` for `next` and nothing for `iter_fn`, and
            // skull emits this call with no spec binding (`lower/loops.rs:159`),
            // so it always reaches the shared generic body. A generic one
            // therefore cannot be given a concrete instance here -- its own
            // bucket, because it is a missing RECORD, not a missing binding at a
            // call site.
            if let Some((m, f)) = info.iter_fn {
                let callee = FuncRef { module: m, func: f };
                let cf = &ws.module(callee.module).functions[callee.func];
                if cf.generics.is_empty() {
                    record(&mut g, &mut work, callee, None, EdgeKind::IteratorIter, span);
                } else {
                    g.unresolved.push(Unresolved {
                        from: inst.clone(),
                        callee,
                        reason: "iter-witness-no-instantiation-record",
                        span,
                    });
                }
            }
            if let BoundNext::Concrete { func: (m, f), next_generics, .. } = &info.next {
                let callee = FuncRef { module: *m, func: *f };
                let cf = &ws.module(callee.module).functions[callee.func];
                match instantiate_next(&mut g.arena, ws, inf, cf, next_generics, &subst) {
                    Ok((args, callee_subst)) => {
                        let to = Instance { func: callee, args };
                        g.edges.push(Edge {
                            from: inst.clone(),
                            to: to.clone(),
                            kind: EdgeKind::IteratorNext,
                            span,
                        });
                        if g.instances.insert(to.clone()) {
                            work.push((to, callee_subst, depth + 1));
                        }
                    }
                    Err(reason) => g.unresolved.push(Unresolved {
                        from: inst.clone(),
                        callee,
                        reason,
                        span,
                    }),
                }
            }
        }
    }

    g
}

/// The callee's instantiation: its generic bindings converted to `MonoTy` under
/// the CALLER's substitution, plus the substitution to walk the callee's body
/// with. This is the transitive-instantiation step.
fn instantiate(
    arena: &mut MonoArena,
    ws: &Workspace,
    inf: &BodyInference,
    callee: &Function,
    bindings: Option<&Vec<bone_semantic::prelude::TyId>>,
    caller_subst: &MonoSubst,
) -> Result<(Vec<MonoTyId>, MonoSubst), &'static str> {
    if callee.generics.is_empty() {
        return Ok((Vec::new(), MonoSubst::default()));
    }
    let Some(bindings) = bindings else { return Err("no-bindings") };
    if bindings.len() != callee.generics.len() {
        return Err("arity-mismatch");
    }
    let mut args = Vec::with_capacity(bindings.len());
    let mut callee_subst = MonoSubst::default();
    for (g, b) in callee.generics.iter().zip(bindings) {
        let Some(var) = skull_mir::monoty::generic_var_of(ws, inf, *g) else {
            return Err("declared-generic-not-generic");
        };
        let id = arena.convert(ws, inf, *b, caller_subst).map_err(reason_name)?;
        callee_subst.insert(var, id);
        args.push(id);
    }
    Ok((args, callee_subst))
}

/// The `for`-protocol `next` witness's instantiation. It is SYNTHESIZED, so it
/// has no call expr and can never appear in `call_generic_bindings`;
/// `next_generics` is the only record of what its generics bound to (plan S 12:
/// resolve the witness at instantiation). Reading the callee's DECLARED generics
/// and looking each up here is the same route `spec_key_from_next_generics`
/// takes -- a shape comes from a declaration, never from the body's own
/// inference.
fn instantiate_next(
    arena: &mut MonoArena,
    ws: &Workspace,
    inf: &BodyInference,
    callee: &Function,
    next_generics: &FxHashMap<bone_semantic::hir::VarId, bone_semantic::prelude::TyId>,
    caller_subst: &MonoSubst,
) -> Result<(Vec<MonoTyId>, MonoSubst), &'static str> {
    if callee.generics.is_empty() {
        return Ok((Vec::new(), MonoSubst::default()));
    }
    let mut args = Vec::with_capacity(callee.generics.len());
    let mut callee_subst = MonoSubst::default();
    for g in &callee.generics {
        let Some(var) = skull_mir::monoty::generic_var_of(ws, inf, *g) else {
            return Err("declared-generic-not-generic");
        };
        let Some(bound) = next_generics.get(&var) else { return Err("next-generic-unbound") };
        let id = arena.convert(ws, inf, *bound, caller_subst).map_err(reason_name)?;
        callee_subst.insert(var, id);
        args.push(id);
    }
    Ok((args, callee_subst))
}

fn reason_name(r: MonoTyReason) -> &'static str {
    r.bucket()
}

fn body_inference(ws: &Workspace, f: FuncRef) -> Option<&BodyInference> {
    ws.inference(f.module)?.per_fn.get(&BodyOwner::Function(f.func))
}

/// Every function the emitter would root, using production's own predicate.
pub fn emission_roots(ws: &Workspace, project_root: Option<&str>) -> Vec<FuncRef> {
    let (deco_map, _) = super::mir_emit::build_deco_map(ws);
    let mut roots = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (fid, f) in module.functions.iter() {
            if super::mir_emit::is_emission_root(module.file.as_str(), project_root, f, &deco_map)
            {
                roots.push(FuncRef { module: module.id, func: fid });
            }
        }
    }
    roots
}

/// The compact manifest: one line per instance, sorted by symbol.
pub fn render_manifest(ws: &Workspace, g: &InstanceGraph) -> String {
    let mut out = String::new();
    for s in g.sorted_symbols(ws) {
        out.push_str(&s);
        out.push('\n');
    }
    out
}

/// A stable digest of the manifest, for cross-run and cross-projection
/// comparison without carrying the whole text.
pub fn manifest_digest(ws: &Workspace, g: &InstanceGraph) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in render_manifest(ws, g).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn instance_count(g: &InstanceGraph) -> usize {
    g.instances.len()
}

/// Reconciliation of today's conditional specializer against the shadow graph
/// (plan S 15a). Equality is NOT expected -- the old mechanism is conditional,
/// so the shadow side is strictly larger. What must hold is that nothing today's
/// machinery produces is MISSING from the shadow graph; an absence in that
/// direction means the shadow walk cannot see an edge production already takes.
#[derive(Default, Debug)]
pub struct Reconciliation {
    /// Specializations the current machinery produces.
    pub today: usize,
    /// ...of which the shadow graph contains a matching instance.
    pub matched: usize,
    /// ...of which it does NOT, and whose CALLER the shadow walk did reach.
    /// Each of these is a failure, not a difference.
    pub missing: Vec<String>,
    /// ...of which it does not, but whose caller the shadow walk never reached
    /// either. A reachability difference, explained rather than unexplained.
    pub missing_unreached_caller: usize,
    /// Call sites whose current spec decision was a Stage-0b invariant
    /// rejection. Expected to be zero on a valid corpus.
    pub rejected_sites: usize,
    /// Shadow instances with no counterpart today. These are the EXPECTED
    /// difference: instances the conditional specializer declines to make.
    pub shadow_only: usize,
}

/// A today-slot matches a shadow arg when the slot is `None` (the partial key
/// erased it, so it constrains nothing) or the rendered types are equal.
fn slots_match(slots: &[Option<String>], shadow: &[String]) -> bool {
    slots.len() == shadow.len()
        && slots
            .iter()
            .zip(shadow)
            .all(|(s, a)| s.as_ref().is_none_or(|s| s == a))
}

pub fn reconcile(
    ws: &Workspace,
    g: &InstanceGraph,
    target: bone_semantic::hir::FnBackend,
) -> Reconciliation {
    let (today, rejected_sites) = skull_mir::spec_census::today_specializations(ws, target);

    // Shadow instances grouped by function, as rendered argument lists.
    let mut by_func: FxHashMap<(u32, u32), Vec<Vec<String>>> = FxHashMap::default();
    for i in &g.instances {
        by_func
            .entry((i.func.module.raw(), i.func.func.into_raw().into_u32()))
            .or_default()
            .push(i.args.iter().map(|a| g.arena.render(*a)).collect());
    }

    let mut r = Reconciliation { today: today.len(), rejected_sites, ..Default::default() };
    let mut matched_shadow: std::collections::HashSet<(u32, u32, Vec<String>)> =
        Default::default();
    for t in &today {
        let k = (t.func.module.raw(), t.func.func.into_raw().into_u32());
        let hit = by_func
            .get(&k)
            .map(|args| args.iter().find(|a| slots_match(&t.slots, a)))
            .unwrap_or(None);
        match hit {
            Some(a) => {
                r.matched += 1;
                matched_shadow.insert((k.0, k.1, a.clone()));
            }
            None => {
                let ck = (t.caller.module.raw(), t.caller.func.into_raw().into_u32());
                if !by_func.contains_key(&ck) {
                    r.missing_unreached_caller += 1;
                    continue;
                }
                let f = &ws.module(t.func.module).functions[t.func.func];
                let caller = &ws.module(t.caller.module).functions[t.caller.func];
                let slots: Vec<&str> =
                    t.slots.iter().map(|s| s.as_deref().unwrap_or("<erased>")).collect();
                r.missing.push(format!(
                    "{}<{}> (called from `{}`)",
                    f.name,
                    slots.join(", "),
                    caller.name
                ));
            }
        }
    }
    r.missing.sort();
    r.missing.dedup();
    r.shadow_only = g
        .instances
        .iter()
        .filter(|i| {
            let k = (i.func.module.raw(), i.func.func.into_raw().into_u32());
            let args: Vec<String> = i.args.iter().map(|a| g.arena.render(*a)).collect();
            !matched_shadow.contains(&(k.0, k.1, args))
        })
        .count();
    r
}
