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
//! **Where the graph is still incomplete**, stated so a reader does not mistake
//! the histogram for the whole graph: `iterator-iter` has no edges at all
//! (a generic `iter` witness has no instantiation record to build one from --
//! the finding, not an omission), `CompilerGenerated` OVER-approximates, and a
//! coroutine's `__defer`/`__on_*` helpers exist only after the MIR transform so
//! nothing here can count them. Every edge recorded is real;
//! [`InstanceGraph::uninstantiated_roots`] records the generic roots no
//! instantiation reached. Full accounting: `docs/mono-stage0d-manifest.md`.

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
    /// A Rock function taken as a VALUE rather than called (`let f = adder`, a
    /// callback argument, a `func:` struct field).
    ///
    /// The discriminant is skull's OWN, not bone's: `lower/expr.rs:75` lowers a
    /// fn value from `Expr::Global(name)` where `ws.functions_by_name` has the
    /// name, and resolves the symbol with `fn_value_symbol`. bone records
    /// nothing in `resolved_overload_calls` for it unless the name is
    /// overloaded, so scanning that map finds none -- measured, and the reason
    /// `bbc06d767` withdrew the first attempt at this edge. `bone-lints`'
    /// `body_edges` reaches the same conclusion independently and states it:
    /// "never a call and never in the inference maps... the two nets are
    /// complementary".
    FnValue,
    /// An iterable argument coerced through its `iter()` at an
    /// `Iterator`-bounded param (`v.map(f)` with no explicit `.iter()`), from
    /// `implicit_iter_args`. The call-argument analog of [`Self::IteratorIter`].
    ImplicitIter,
    /// The `next` witness of a call to a BOUNDED generic fn, from the call
    /// site's conformance tree (`bound_call_trees`). Distinct from
    /// [`Self::IteratorNext`], which is the `for` desugar's witness.
    BoundNextWitness,
    /// A call no source expression contains: lowering synthesizes it and
    /// resolves the callee by LITERAL NAME through `Builder::stdlib_funcref` /
    /// `call_spine` (`docs/boundary-contracts.md` S 1.4). Over-approximated --
    /// see [`COMPILER_GENERATED_TRIGGERS`].
    CompilerGenerated,
}

impl EdgeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::DirectCall => "direct-call",
            Self::GenericCall => "generic-call",
            Self::IteratorIter => "iterator-iter",
            Self::IteratorNext => "iterator-next",
            Self::FnValue => "fn-value",
            Self::ImplicitIter => "implicit-iter",
            Self::BoundNextWitness => "bound-next-witness",
            Self::CompilerGenerated => "compiler-generated",
        }
    }
}

/// The boundary classes of `docs/boundary-contracts.md` Part 1. A boundary is a
/// callee whose body skull does not lower to MIR; plan S 9 requires every call
/// to resolve to exactly one `InstanceId` OR one `BoundaryId`.
///
/// [`BoundaryClass::Invalid`] is not a body kind but a required OUTCOME -- the
/// class a boundary takes on a target that cannot supply its contract. The
/// shader-interface class (S 1.5) has no member here: GLSL is not a `FnBackend`
/// the instance walk runs for, so no call in this graph resolves to one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum BoundaryClass {
    Extern,
    InlineBody,
    Intrinsic,
    ErasedRuntimeEntry,
}

impl BoundaryClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::Extern => "extern",
            Self::InlineBody => "inline-body",
            Self::Intrinsic => "intrinsic",
            Self::ErasedRuntimeEntry => "erased-runtime-entry",
        }
    }
}

/// One reached boundary, classified for the graph's target.
///
/// `symbol` is the name emission must produce: bare for `extern`/`@nomangle`,
/// mangled otherwise -- the rule `skull_mir::mangle` applies, read from it
/// rather than restated.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Boundary {
    pub class: BoundaryClass,
    pub symbol: String,
    /// The target has no body serving this boundary, so plan S 10's
    /// `Invalid boundary` outcome is required: reject with a span, never a stub.
    pub invalid_on_target: bool,
}

/// A helper symbol a coroutine instance OWNS. Not an [`Edge`]: the target is
/// not another instance.
///
/// Stage 3's requirement is visible in `symbol_today`. `emit_coroutine` derives
/// every helper from `mangled(ws, self_ref)` (`coroutine_emit.rs:21`) -- the
/// SOURCE FuncRef -- so two instances of one generic `co fn` claim the same
/// helper names. The edge Stage 3 needs must therefore carry the OWNING
/// INSTANCE, so the base becomes [`InstanceGraph::symbol`] instead of the
/// source mangling.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CoroutineHelper {
    pub owner_identity: String,
    pub suffix: &'static str,
    /// What emission produces TODAY: mangled source name + suffix.
    pub symbol_today: String,
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
    /// The execution target this graph was built for. Instance IDENTITY is
    /// target-neutral (plan S 20a); the target decides which backend variant a
    /// call resolves to and which boundaries are available.
    pub target: bone_semantic::hir::FnBackend,
    /// Every reached boundary, classified per `docs/boundary-contracts.md`.
    pub boundaries: BTreeMap<String, Boundary>,
    /// Helper symbols owned by coroutine instances.
    pub coroutine_helpers: BTreeSet<CoroutineHelper>,
    /// Which of the 15 literal-name `stdlib_funcref` entries the walk actually
    /// reached (`docs/boundary-contracts.md` S 1.4). A name absent here is one
    /// the corpus never triggers, not one that does not exist.
    pub erased_entries: BTreeSet<String>,
    /// Callees seeded from an ALWAYS-EMITTED body (a decorator definition or a
    /// global initializer) rather than from a function root. Production treats
    /// their call edges as reachability roots (`mir_emit::reachable_fns`'s
    /// `extra_roots`/`extra_root_names`); round 1 walked neither body kind, so
    /// anything reachable only from a global initializer was missing entirely.
    pub always_emitted_seeds: usize,
}

/// Calls lowering SYNTHESIZES, keyed by the source operation that triggers
/// them: `(trigger name, the `stdlib_funcref`/`call_spine` names its arm can
/// emit)`. Read off `crates/skull-mir/src/lower/storage.rs`; the complete
/// 15-name set is catalogued in `docs/boundary-contracts.md` S 1.4.
///
/// **This OVER-APPROXIMATES, deliberately and by a large factor.** Each arm
/// fires only when the receiver's CONCRETE element is column-stored
/// (`vec_column_elem_of` / `map_column_value_of` returning `Some`); this table
/// keys on the trigger name alone, so a body operating exclusively on boxed
/// elements gets edges emission will never produce. The true set is a SUBSET of
/// what the histogram reports. Checking the storage condition needs the
/// element's concrete representation at each call, which is a lowering
/// question, not an HIR one -- so the honest options were an over-approximation
/// or nothing, and an over-approximation still proves the class is non-empty
/// and names which entries are reachable at all.
pub const COMPILER_GENERATED_TRIGGERS: &[(&str, &[&str])] = &[
    ("__vec_data_capacity", &["table_len", "__rock_col_capacity"]),
    ("__vec_data_insert", &["__rock_col_insert"]),
    ("__vec_data_clone", &["__rock_col_copy"]),
    ("__vec_data_sort", &["__rock_col_sort", "__rock_col_sort_leaves"]),
    ("__rock_vec_to_lua_array", &["__rock_vec_column_elements"]),
    ("table_unset", &["__map_col_free"]),
    ("len", &["__map_col_len"]),
    ("map_len", &["__map_col_len"]),
    ("contains", &["__map_col_contains"]),
    ("clear", &["__map_col_clear"]),
    ("get", &["__map_col_slot_of"]),
];

/// The synthesized entries reached from an INDEX rather than from a named call:
/// `m[k]` / `m[k] = v` on a column Map (`column_index_place`,
/// `map_column_store`, `map_column_index_repr`) and the debug-mode
/// `NativeBuffer` bounds check (`nb_checked_index`). Same over-approximation as
/// [`COMPILER_GENERATED_TRIGGERS`].
pub const INDEX_TRIGGERED: &[(&str, &[&str])] = &[
    ("Map", &["__map_col_slot", "__map_col_slot_checked", "__map_col_free"]),
    ("NativeBuffer", &["__nb_checked_index"]),
];

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
    /// The READABLE symbol for an instance: the function's mangled name plus its
    /// rendered logical arguments. For display only -- two distinct instances can
    /// share one rendering ([`MonoTy::render`] is a pretty type syntax, not an
    /// identity), so deduplicating or matching on this would merge them. Key on
    /// [`Self::identity`].
    pub fn symbol(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        if i.args.is_empty() {
            return base;
        }
        let args: Vec<String> = i.args.iter().map(|a| self.arena.render(*a)).collect();
        format!("{base}<{}>", args.join(", "))
    }

    /// The instance's IDENTITY: the mangled name plus its arguments' canonical
    /// encodings. Unambiguous, so sorting and dedup on it cannot merge two
    /// distinct instances.
    pub fn identity(&self, ws: &Workspace, i: &Instance) -> String {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let base = mangle_fn(ws, f);
        if i.args.is_empty() {
            return base;
        }
        let args: Vec<String> = i.args.iter().map(|a| self.arena.canonical(*a)).collect();
        format!("{base}<{}>", args.join(","))
    }

    /// Manifest lines, sorted and deduplicated by IDENTITY: `<identity>\t<symbol>`.
    ///
    /// The identity is IN the line, not merely the sort key. A caller that
    /// diffed, hashed or set-compared display symbols alone would collapse two
    /// instances whose arguments happen to render alike, which is exactly the
    /// property the identity encoding exists to prevent.
    pub fn manifest_lines(&self, ws: &Workspace) -> Vec<String> {
        let mut v: Vec<String> = self
            .instances
            .iter()
            .map(|i| format!("{}\t{}", self.identity(ws, i), self.symbol(ws, i)))
            .collect();
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
                direct.insert(self.identity(ws, from));
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
                        mutual.insert(self.identity(ws, n));
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
                .map(|i| self.identity(ws, i))
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
pub fn build(ws: &Workspace, roots: Vec<FuncRef>, perturb: bool) -> InstanceGraph {
    build_for_target(ws, roots, perturb, bone_semantic::hir::FnBackend::Lua)
}

/// [`build`], for an explicit execution target.
///
/// The target changes which backend VARIANT each call resolves to
/// (`skull_mir::lower::backend_variant`, production's own selector) and which
/// boundaries are available. It must NOT change logical instance identity --
/// that is plan S 20a's separation, and
/// `logical_identity_is_stable_across_targets` is the assertion.
pub fn build_for_target(
    ws: &Workspace,
    mut roots: Vec<FuncRef>,
    perturb: bool,
    target: bone_semantic::hir::FnBackend,
) -> InstanceGraph {
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
        target,
        boundaries: BTreeMap::new(),
        coroutine_helpers: BTreeSet::new(),
        erased_entries: BTreeSet::new(),
        always_emitted_seeds: 0,
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

    // Bodies that are ALWAYS emitted and are not functions: decorator
    // definitions and global initializers. Production seeds their call edges as
    // reachability roots (`mir_emit::reachable_fns`'s `extra_roots` /
    // `extra_root_names`); the walk has no `Instance` to hang them on, so their
    // callees are seeded as roots instead. A generic callee therefore arrives
    // with no substitution to instantiate from and lands in
    // `uninstantiated_roots` -- honest, and exactly what production does with it
    // today (it emits the shared generic body).
    for r in always_emitted_callees(ws, target) {
        let f = &ws.module(r.module).functions[r.func];
        g.always_emitted_seeds += 1;
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
            let callee = variant_for(ws, FuncRef { module: m, func: f }, target);
            let bindings = inf.call_generic_bindings.get(&e);
            let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
            let kind =
                if bindings.is_some() { EdgeKind::GenericCall } else { EdgeKind::DirectCall };
            record(&mut g, &mut work, callee, bindings, kind, span);
        }

        // Rock functions taken as VALUES. Not in any inference map: skull
        // lowers them from `Expr::Global(name)` under exactly this guard
        // (`lower/expr.rs:75`), so the expr arena is the only place they exist.
        if let Some(b) = body {
            for (e, callee) in fn_value_sites(ws, inf, b, target) {
                let span = b.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
                let cf = &ws.module(callee.module).functions[callee.func];
                if cf.generics.is_empty() {
                    record(&mut g, &mut work, callee, None, EdgeKind::FnValue, span);
                } else {
                    // THE WATCH ITEM. A fn value has no call site, so there are
                    // no bindings to key an instantiation on -- structurally the
                    // same gap as the `iter` witness, and a Stage-0h author call:
                    // a generic fn taken as a value cannot be monomorphized
                    // without either a recorded instantiation at the take site or
                    // a rule that rejects it.
                    g.unresolved.push(Unresolved {
                        from: inst.clone(),
                        callee,
                        reason: "fn-value-generic-no-instantiation",
                        span,
                    });
                }
            }

            // Synthesized stdlib calls. Over-approximated -- see
            // `COMPILER_GENERATED_TRIGGERS`.
            for (e, name) in compiler_generated_sites(ws, inf, b) {
                let span = b.expr_spans.get(e).copied().unwrap_or_else(Span::unknown);
                match stdlib_entry(ws, name, target) {
                    Some(callee) => {
                        record(&mut g, &mut work, callee, None, EdgeKind::CompilerGenerated, span);
                        g.erased_entries.insert(name.to_string());
                    }
                    // `stdlib_funcref` REJECTS here rather than falling back;
                    // recording the same absence keeps the two in step.
                    None => g.unresolved.push(Unresolved {
                        from: inst.clone(),
                        callee: inst.func,
                        reason: "compiler-generated-entry-missing-for-target",
                        span,
                    }),
                }
            }
        }

        // Iterable args coerced through `iter()` at an `Iterator`-bounded param.
        let mut implicit: Vec<(Idx<bone_semantic::hir::Expr>, (ModuleId, Idx<Function>))> =
            inf.implicit_iter_args.iter().map(|(e, r)| (*e, *r)).collect();
        implicit.sort_by_key(|(e, _)| *e);
        for (e, (m, f)) in implicit {
            let callee = variant_for(ws, FuncRef { module: m, func: f }, target);
            let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
            let cf = &ws.module(callee.module).functions[callee.func];
            if cf.generics.is_empty() {
                record(&mut g, &mut work, callee, None, EdgeKind::ImplicitIter, span);
            } else {
                // The SAME missing record as the `for`-head `iter` witness: bone
                // stores the resolved fn and no instantiation for it. Filed under
                // its own reason so the two sites stay countable apart.
                g.unresolved.push(Unresolved {
                    from: inst.clone(),
                    callee,
                    reason: "implicit-iter-witness-no-instantiation-record",
                    span,
                });
            }
        }

        // `next` witnesses of calls to BOUNDED generic fns.
        let mut trees: Vec<(Idx<bone_semantic::hir::Expr>, &bone_semantic::prelude::BoundTree)> =
            inf.bound_call_trees.iter().map(|(e, t)| (*e, t)).collect();
        trees.sort_by_key(|(e, _)| *e);
        for (e, tree) in trees {
            let span = body.and_then(|b| b.expr_spans.get(e).copied()).unwrap_or_else(Span::unknown);
            walk_bound_tree(
                &mut g, &mut work, ws, inf, &inst, &subst, depth, tree, span, target,
            );
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
            record_bound_next(
                &mut g, &mut work, ws, inf, &inst, &subst, depth, &info.next, span,
                EdgeKind::IteratorNext, target,
            );
        }
    }

    classify_boundaries(&mut g, ws);
    record_coroutine_helpers(&mut g, ws);
    g
}

/// One `next` witness plus, recursively, the witnesses of ITS bounded generics.
///
/// The nested `sub` tree is why this recurses: a chained iterator resolves
/// `next(MapIter<VecIter<..>>)` to MapIter's `next`, whose own inner bound
/// resolves to VecIter's, and only the outermost is named in `info.next`.
/// Round 1 stopped at the top level, so every inner witness of a combinator
/// chain was missing from the graph.
#[allow(clippy::too_many_arguments)]
fn record_bound_next(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    from: &Instance,
    subst: &MonoSubst,
    depth: u32,
    next: &BoundNext,
    span: Span,
    kind: EdgeKind,
    target: bone_semantic::hir::FnBackend,
) {
    let BoundNext::Concrete { func: (m, f), next_generics, sub } = next else { return };
    let callee = variant_for(ws, FuncRef { module: *m, func: *f }, target);
    let cf = &ws.module(callee.module).functions[callee.func];
    match instantiate_next(&mut g.arena, ws, inf, cf, next_generics, subst) {
        Ok((args, callee_subst)) => {
            let to = Instance { func: callee, args };
            g.edges.push(Edge { from: from.clone(), to: to.clone(), kind, span });
            if g.instances.insert(to.clone()) {
                work.push((to, callee_subst, depth + 1));
            }
        }
        Err(reason) => {
            g.unresolved.push(Unresolved { from: from.clone(), callee, reason, span })
        }
    }
    walk_bound_tree(g, work, ws, inf, from, subst, depth, sub, span, target);
}

#[allow(clippy::too_many_arguments)]
fn walk_bound_tree(
    g: &mut InstanceGraph,
    work: &mut Vec<(Instance, MonoSubst, u32)>,
    ws: &Workspace,
    inf: &BodyInference,
    from: &Instance,
    subst: &MonoSubst,
    depth: u32,
    tree: &bone_semantic::prelude::BoundTree,
    span: Span,
    target: bone_semantic::hir::FnBackend,
) {
    let mut keys: Vec<&usize> = tree.next.keys().collect();
    keys.sort();
    for k in keys {
        record_bound_next(
            g, work, ws, inf, from, subst, depth, &tree.next[k], span,
            EdgeKind::BoundNextWitness, target,
        );
    }
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
        let ty = skull_mir::monoty::convert(ws, inf, *b, caller_subst).map_err(reason_name)?;
        args.push(arena.intern(ty.clone()));
        callee_subst.insert(var, ty);
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
        let ty =
            skull_mir::monoty::convert(ws, inf, *bound, caller_subst).map_err(reason_name)?;
        args.push(arena.intern(ty.clone()));
        callee_subst.insert(var, ty);
    }
    Ok((args, callee_subst))
}

fn reason_name(r: MonoTyReason) -> &'static str {
    r.bucket()
}

/// The backend variant of `rep` serving `target`, through production's own
/// selector. `Err` (nothing serves it) keeps `rep`: the boundary pass reports
/// that as an INVALID boundary for the target, which is the outcome plan S 10
/// requires, rather than dropping the edge and under-reporting the graph.
fn variant_for(
    ws: &Workspace,
    rep: FuncRef,
    target: bone_semantic::hir::FnBackend,
) -> FuncRef {
    let name = ws.module(rep.module).functions[rep.func].name;
    skull_mir::lower::backend_variant(ws, name, rep, target).unwrap_or(rep)
}

/// Every `Expr::Global(name)` in `body` that names a FUNCTION -- skull's own
/// fn-value discriminant (`lower/expr.rs:75`).
///
/// Resolution mirrors `fn_value_symbol`: bone's recorded overload wins;
/// otherwise the candidates are backend VARIANTS of one `(name, signature)` and
/// mangle identically, so any of them names the same instance. A genuine
/// unresolved overload (candidates with distinct manglings) is skipped here --
/// `fn_value_symbol` REJECTS it, so it cannot occur in a program that compiles.
///
/// A `Expr::Global` in `Callee::Value` position is an indirect CALL, not a value
/// take, and is excluded.
fn fn_value_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_semantic::hir::Body,
    target: bone_semantic::hir::FnBackend,
) -> Vec<(Idx<bone_semantic::hir::Expr>, FuncRef)> {
    use bone_semantic::hir::{Callee, Expr as E};
    let mut called_values: BTreeSet<u32> = BTreeSet::new();
    for (_, expr) in body.expr_arena.iter() {
        if let E::Call { callee: Callee::Value(ve), .. } = expr {
            called_values.insert(ve.into_raw().into());
        }
    }
    let mut out = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        let E::Global(name) = expr else { continue };
        if called_values.contains(&e.into_raw().into()) {
            continue;
        }
        let Some(cands) = ws.functions_by_name.get(name) else { continue };
        let rep = match inf.resolved_overload_calls.get(&e) {
            Some(&(m, f)) => FuncRef { module: m, func: f },
            None => {
                let mut manglings: Vec<String> = cands
                    .iter()
                    .map(|&(m, fi)| mangle_fn(ws, &ws.module(m).functions[fi]))
                    .collect();
                manglings.sort();
                manglings.dedup();
                if manglings.len() != 1 {
                    continue;
                }
                let (m, f) = cands[0];
                FuncRef { module: m, func: f }
            }
        };
        out.push((e, variant_for(ws, rep, target)));
    }
    out.sort_by_key(|(e, _)| *e);
    out
}

/// Source operations whose lowering can SYNTHESIZE a `stdlib_funcref` /
/// `call_spine` call, paired with each name it can synthesize. Over-approximates
/// -- see [`COMPILER_GENERATED_TRIGGERS`].
fn compiler_generated_sites(
    ws: &Workspace,
    inf: &BodyInference,
    body: &bone_semantic::hir::Body,
) -> Vec<(Idx<bone_semantic::hir::Expr>, &'static str)> {
    use bone_semantic::hir::{Callee, Expr as E};
    let mut out: Vec<(Idx<bone_semantic::hir::Expr>, &'static str)> = Vec::new();
    let mut push = |e, names: &'static [&'static str]| {
        for n in names {
            out.push((e, *n));
        }
    };
    for (e, expr) in body.expr_arena.iter() {
        match expr {
            E::Call { callee: Callee::Unresolved { name, .. }, .. } => {
                if let Some((_, names)) =
                    COMPILER_GENERATED_TRIGGERS.iter().find(|(t, _)| *t == name.as_str())
                {
                    push(e, names);
                }
            }
            // The index forms have no callee name to key on: the receiver's
            // CONSTRUCTOR is the trigger. Rendering is confined to index exprs,
            // which are a small minority of any body.
            E::Index { base, .. } => {
                let Some(ty) = inf.expr_types.get(*base) else { continue };
                // A `TyId::Local` renders only against ITS body's inference --
                // `None` panics rather than degrading, which is how this was
                // caught immediately.
                let r = ws.render(Some(inf), *ty);
                if let Some((_, names)) =
                    INDEX_TRIGGERED.iter().find(|(ctor, _)| r.starts_with(*ctor))
                {
                    push(e, names);
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|(e, n)| (*e, *n));
    out
}

/// The unique stdlib function `name` serving `target` -- the same lookup
/// `Builder::stdlib_funcref` performs, and it fails on the same conditions
/// (missing name, no serving body, residual ambiguity).
fn stdlib_entry(
    ws: &Workspace,
    name: &str,
    target: bone_semantic::hir::FnBackend,
) -> Option<FuncRef> {
    let decls = ws.functions_by_name.get(&ustr::Ustr::from(name))?;
    let serving: Vec<FuncRef> = decls
        .iter()
        .filter(|(m, f)| serves(ws.module(*m).functions[*f].backend, target))
        .map(|&(m, f)| FuncRef { module: m, func: f })
        .collect();
    match serving.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

fn serves(
    decl: bone_semantic::hir::FnBackend,
    target: bone_semantic::hir::FnBackend,
) -> bool {
    use bone_semantic::hir::FnBackend;
    decl == FnBackend::Any || target == FnBackend::Any || decl == target
}

/// Callees of the bodies production ALWAYS emits and that are not functions:
/// decorator definitions and global initializers. Mirrors
/// `mir_emit::reachable_fns`'s `extra_roots` / `extra_root_names` pair -- the
/// inference-map edges and the fn-value name scan, which are complementary nets.
fn always_emitted_callees(
    ws: &Workspace,
    target: bone_semantic::hir::FnBackend,
) -> Vec<FuncRef> {
    let mut out = Vec::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        let Some(inference) = ws.inference(module.id) else { continue };
        let mut owners: Vec<&BodyOwner> = module
            .bodies
            .keys()
            .filter(|o| matches!(o, BodyOwner::Decorator(_) | BodyOwner::Global(_)))
            .collect();
        owners.sort_by_key(|o| format!("{o:?}"));
        for owner in owners {
            let Some(inf) = inference.per_fn.get(owner) else { continue };
            let mut calls: Vec<(ModuleId, Idx<Function>)> =
                inf.resolved_overload_calls.values().copied().collect();
            for info in inf.for_iter_protocol.values() {
                calls.extend(info.iter_fn);
            }
            calls.sort_by_key(|(m, f)| (m.raw(), f.into_raw().into_u32()));
            out.extend(
                calls
                    .into_iter()
                    .map(|(m, f)| variant_for(ws, FuncRef { module: m, func: f }, target)),
            );
            if let Some(b) = module.bodies.get(owner) {
                out.extend(fn_value_sites(ws, inf, b, target).into_iter().map(|(_, f)| f));
            }
        }
    }
    out.sort_by_key(|r| (r.module.raw(), r.func.into_raw().into_u32()));
    out.dedup();
    out
}

/// Classify every reached callee that is a BOUNDARY, per
/// `docs/boundary-contracts.md` Part 1.
///
/// An instance is kept for a boundary as well as a Rock body -- Stage 2 splits
/// them (plan S 9: one `InstanceId` OR one `BoundaryId`), and keeping both here
/// makes the split's cost countable instead of changing the round-1 numbers
/// underneath it.
fn classify_boundaries(g: &mut InstanceGraph, ws: &Workspace) {
    let erased: BTreeSet<String> = g.erased_entries.clone();
    let mut found: BTreeMap<String, Boundary> = BTreeMap::new();
    for i in &g.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        let symbol = mangle_fn(ws, f);
        let intrinsic = f.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic");
        let class = if erased.contains(f.name.as_str()) {
            BoundaryClass::ErasedRuntimeEntry
        } else if intrinsic {
            BoundaryClass::Intrinsic
        } else if f.is_extern {
            BoundaryClass::Extern
        } else if f.inline_body.is_some() {
            BoundaryClass::InlineBody
        } else {
            continue;
        };
        // `variant_for` kept the representative when nothing served the target,
        // so a decl whose backend does not serve it is the S 1.6 outcome:
        // reject with a span, never a stub.
        let invalid_on_target = !serves(f.backend, g.target);
        found.insert(symbol.clone(), Boundary { class, symbol, invalid_on_target });
    }
    g.boundaries = found;
}

/// The helper symbols each coroutine instance owns.
///
/// `__step` and `__resume` are unconditional; `__advance` exists only on the
/// frame-slot channel, which `coroutine_yield_channel` reads from the DECLARED
/// return type -- a declaration, not the body's own inference, per the standing
/// rule. Defers (`__defer<k>`) and lifecycle hooks (`__on_*`) are NOT counted:
/// they come from `CoroLowered`, which exists only after the MIR transform, so
/// there is no HIR-level oracle for them and guessing would be a fabricated
/// number.
fn record_coroutine_helpers(g: &mut InstanceGraph, ws: &Workspace) {
    let mut out = BTreeSet::new();
    for i in &g.instances {
        let f = &ws.module(i.func.module).functions[i.func.func];
        if !f.is_coroutine {
            continue;
        }
        let base = mangle_fn(ws, f);
        let owner_identity = g.identity(ws, i);
        let mut suffixes: Vec<&'static str> = vec!["__step", "__resume"];
        if f.return_ty
            .and_then(|t| skull_mir::lower::coroutine_yield_channel(ws, t))
            .is_some()
        {
            suffixes.push("__advance");
        }
        for suffix in suffixes {
            out.insert(CoroutineHelper {
                owner_identity: owner_identity.clone(),
                suffix,
                symbol_today: format!("{base}{suffix}"),
            });
        }
    }
    g.coroutine_helpers = out;
}

impl InstanceGraph {
    /// Helper symbols that more than one INSTANCE claims.
    ///
    /// Each is a Stage-3 collision: `emit_coroutine` names helpers from the
    /// source `FuncRef`, so every instance of one generic `co fn` produces the
    /// same `<name>__step` / `__resume` / `__advance`. Nothing detects it today
    /// because no generic `co fn` is ever specialized -- `compute_call_spec`
    /// bails on `is_coroutine` first (`shape.rs:1584`, the subject of
    /// `generic_coroutine_column_param_sentinel`).
    ///
    /// **That premise is branch-scoped.** It holds on `mono`, where the sentinel
    /// stays pinned. The Stage-0f tactical slice (`mono-0f-slice`, `285853bad`)
    /// falsifies it: it drops the `is_coroutine` bail on the column trigger, two
    /// instantiations of one `co fn` come out correct, and the sentinel FLIPS --
    /// on that branch only. If that slice or a successor merges, this helper
    /// starts seeing specialized coroutines and the collisions stop being latent.
    pub fn colliding_coroutine_helpers(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut by_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for h in &self.coroutine_helpers {
            by_symbol
                .entry(h.symbol_today.clone())
                .or_default()
                .insert(h.owner_identity.clone());
        }
        by_symbol.retain(|_, owners| owners.len() > 1);
        by_symbol
    }
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

/// The compact manifest: one line per instance, sorted by identity, each line
/// carrying its identity and its display symbol.
pub fn render_manifest(ws: &Workspace, g: &InstanceGraph) -> String {
    let mut out = String::new();
    for s in g.manifest_lines(ws) {
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
/// erased it, so it constrains nothing) or the canonical types are equal. Both
/// sides encode with `MonoTy::canonical`, so a pretty-printing ambiguity cannot
/// match two different types to each other.
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

    // Shadow instances grouped by function, as canonical argument lists.
    let mut by_func: FxHashMap<(u32, u32), Vec<Vec<String>>> = FxHashMap::default();
    for i in &g.instances {
        by_func
            .entry((i.func.module.raw(), i.func.func.into_raw().into_u32()))
            .or_default()
            .push(i.args.iter().map(|a| g.arena.canonical(*a)).collect());
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
            let args: Vec<String> = i.args.iter().map(|a| g.arena.canonical(*a)).collect();
            !matched_shadow.contains(&(k.0, k.1, args))
        })
        .count();
    r
}
