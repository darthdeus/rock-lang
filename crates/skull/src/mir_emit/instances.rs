//! Which instances are emitted: inline candidates, specialization and view-driven chunks.

use super::*;

/// The diagnostic for a demand clash, at the declaration whose specialization
/// two sites disagree about.
fn demand_clash(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    body: &Instance,
) -> (String, CompilerError) {
    let fref = body.func;
    let f = &ws.module(fref.module).functions[fref.func];
    let name = body.symbol(ws, scx.interner());
    (
        name.clone(),
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "internal: two call sites demand `{name}` with different `Iterator` \
                 bound resolutions, so one emitted definition cannot serve both"
            ),
        ),
    )
}

/// The spanned rejection for a specialization demand whose type arguments have
/// outgrown [`crate::instance_graph::MAX_INSTANTIATION_DEPTH`], or `None` when
/// it is within bounds.
///
/// This is the emission side of plan S 0h's `ChangingInstanceRecursion` call --
/// changing-instance recursion is REJECT, and the rejection has to come from
/// the INSTANCE, because E1 leaves no declaration body to reject from. The
/// depth test is the graph's: one `MonoTy::depth`, one constant, so "does this
/// instantiation terminate" has a single answer on both sides.
///
/// Depth is the right discriminant rather than a chain walk: a recursion whose
/// arguments do not change is same-instance recursion, which is legal and which
/// the walk's dedup already closes. Only a chain that grows its arguments
/// diverges, and growth is exactly what depth measures.
fn too_deep_to_instantiate(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    inst: &Instance,
) -> Option<(String, CompilerError)> {
    let args = inst.arg_types(scx.interner());
    let at = skull_mir::monoty::too_deep_to_instantiate(args.iter())?.render();
    let f = &ws.module(inst.func.module).functions[inst.func.func];
    let name = inst.symbol(ws, scx.interner());
    Some((
        name,
        CompilerError::new_with_label(
            f.name_span,
            format!(
                "polymorphic recursion: instantiating `{}` at `{at}` exceeds the \
             instantiation depth limit of {}. A recursive call whose type \
             arguments GROW never terminates -- each call needs a new body -- so \
             it is rejected rather than expanded. Same-instance recursion (type \
             arguments unchanged) is unaffected.",
                f.name,
                crate::instance_graph::MAX_INSTANTIATION_DEPTH,
            ),
        ),
    ))
}

pub(super) fn inline_candidate_keys(
    concrete: &crate::concrete_body::ConcreteBodies,
) -> Vec<Instance> {
    concrete.candidate_keys().iter().cloned().collect()
}

pub(super) fn concrete_stats_requested() -> bool {
    std::env::var("ROCK_LOG").is_ok_and(|value| value == "debug")
}

pub(super) fn build_inline_candidate_table(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    keys: Vec<Instance>,
    base: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
    invalidated: Option<&FxHashSet<FuncRef>>,
    warm_keys_override: Option<Vec<Instance>>,
    added: Option<&BTreeSet<Instance>>,
    removed: Option<&BTreeSet<Instance>>,
) -> Result<std::sync::Arc<skull_transform::InlineCandidateTable>, CompilerError> {
    let mut census = CandidateBuildStats::default();
    build_inline_candidate_table_with_census(
        ws,
        scx,
        concrete,
        keys,
        base,
        invalidated,
        warm_keys_override,
        added,
        removed,
        &mut census,
    )
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CandidateBuildStats {
    pub(super) keys_examined: usize,
    pub(super) warmed: usize,
}

pub(super) fn build_inline_candidate_table_with_census(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    keys: Vec<Instance>,
    base: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
    invalidated: Option<&FxHashSet<FuncRef>>,
    warm_keys_override: Option<Vec<Instance>>,
    added: Option<&BTreeSet<Instance>>,
    removed: Option<&BTreeSet<Instance>>,
    census: &mut CandidateBuildStats,
) -> Result<std::sync::Arc<skull_transform::InlineCandidateTable>, CompilerError> {
    // The old incremental route left this unset and filtered every key here.
    // Count the exact iterator population so a mutation that restores that
    // route is visible rather than looking like a timing-only regression.
    census.keys_examined = usize::from(warm_keys_override.is_none()) * keys.len();
    let warm_keys: Vec<Instance> = warm_keys_override.unwrap_or_else(|| {
        keys.iter()
            .filter(|key| match base {
                None => true,
                Some(base) => {
                    invalidated.is_some_and(|frefs| frefs.contains(&key.func))
                        || !base.contains(key)
                }
            })
            .cloned()
            .collect()
    });
    let warms: Vec<skull_transform::InlineCandidateWarm> = par_map_with(
        warm_keys,
        skull_transform::InlineCache::default,
        |inline_cache, inst| inline_cache.warm_candidate(ws, scx, inst, concrete.plans.as_ref()),
    )
    .into_iter()
    .collect::<Result<_, _>>()?;
    census.warmed = warms.len();
    if let Some(base) = base {
        if let (Some(added), Some(removed)) = (added, removed) {
            return Ok(std::sync::Arc::new(
                skull_transform::InlineCandidateTable::from_warms_incremental(
                    std::sync::Arc::clone(base),
                    warms,
                    &added.iter().cloned().collect::<Vec<_>>(),
                    &removed.iter().cloned().collect::<Vec<_>>(),
                ),
            ));
        }
    }
    Ok(std::sync::Arc::new(
        skull_transform::InlineCandidateTable::from_warms_with_base(
            base.map(std::sync::Arc::as_ref),
            keys,
            warms,
        ),
    ))
}

/// **Phase 2's specialized-body population, driven by SpecDemand closure over
/// `cache.lua_view`'s retained ordinary population -- not by `lua_view.
/// instances` directly, and not by a reachability walk.**
///
/// `lua_view` answers RETENTION (is this instance part of the program at
/// all), which is a different question from DELIVERY (does it need a
/// standalone Lua definition, or is every call to it spliced/inlined away at
/// the call site). Delivery is decided per call site during ordinary lowering
/// -- already done, once, in `emit_full`'s upfront parallel chunk build -- and
/// is exactly what a chunk's own `specs: Vec<SpecDemand>` records. A generic
/// instance the graph retains but nothing DEMANDS standalone (every caller
/// spliced it) has no chunk to build at all: `map_new<int, IVec2>` is such a
/// case in the shipped corpus (found empirically -- every real project
/// reaches it only through a global's `Map<int, IVec2> = map_new()`
/// initializer, which the column-storage inliner splices away entirely, so
/// building it standalone hits a REAL rejection -- `transmute` cannot build a
/// column-stored `Map` -- for an instance nothing ever calls that way).
///
/// The queue seeds from the already-lowered chunk of every ordinary/coroutine
/// declaration `should_emit_via_view` retains, plus every already-collected
/// always-emitted (decorator/global) demand; each newly BUILT specialized
/// chunk's own `.specs` is queued the moment that chunk is done, mirroring
/// `emit_specialized_callees`'s render-time fixpoint but for BUILDING. Each
/// instance's tree is the concrete plan's resolved bound tree, which the graph
/// installs as part of that owner plan (never `DemandMap` -- sufficient per
/// `instance_tree_to_resolved`'s doc, corpus-verified byte-identical against
/// `DemandMap`-lowered text for every case both sides reach). A demand clash
/// (`cache.graph.has_bound_tree_disagreement`) rejects
/// that one instance rather than picking an arbitrary tree -- the
/// graph-driven equivalent of `DemandMap`'s clash check.
/// Test-only observer of the specialization schedule: it sees each job as it
/// is queued, may hold a finished job back to force a completion order, and
/// records what is installed in which order.
#[cfg(test)]
pub(super) trait SpecScheduleHook: Send + Sync {
    /// Called as a job is admitted to the queue, with its symbol.
    fn scheduled(&self, name: &str);
    /// Called on the worker once a job's chunk is built; blocking here holds
    /// that job's completion (and so its follow-up demand) back.
    fn finished(&self, name: &str);
    /// Called as each demanded instance's outcome is installed.
    fn installed(&self, name: &str, ok: bool);
}

#[cfg(test)]
thread_local! {
    pub(super) static SPEC_SCHEDULE_HOOK: std::cell::RefCell<Option<std::sync::Arc<dyn SpecScheduleHook>>> =
        const { std::cell::RefCell::new(None) };
}

/// One demanded instance's outcome, keyed by its symbol so the queue's
/// completion order never shows in what is installed.
type SpecOutcome = (String, Instance, Result<Chunk, CompilerError>);

/// The demand closure's shared state: which instances have been admitted
/// (each is decided once, whatever thread demands it) and every outcome so far.
struct SpecQueue<'a> {
    ws: &'a Workspace,
    scx: &'a skull_mir::lower::ScalarCtx,
    cache: &'a SkullEmitCache,
    done: std::sync::Mutex<FxHashSet<Instance>>,
    outcomes: std::sync::Mutex<Vec<SpecOutcome>>,
    #[cfg(test)]
    hook: Option<std::sync::Arc<dyn SpecScheduleHook>>,
}

impl SpecQueue<'_> {
    /// Decide `inst` if it is new: reject it, take its cached chunk (and
    /// admit that chunk's own demand), or queue it as a build job.
    fn admit(&self, inst: Instance, jobs: &mut Vec<Instance>) {
        if !self
            .done
            .lock()
            .expect("spec demand set")
            .insert(inst.clone())
        {
            return;
        }
        // Polymorphic recursion (plan S 0h: REJECT). The graph's own build
        // silently CAPS a too-deep instance (`depth_capped`, no rejection --
        // it is a target-neutral fact-recorder, not a diagnostic surface), so
        // emission is where the spanned rejection has to live, same as before.
        if let Some((name, err)) = too_deep_to_instantiate(self.ws, self.scx, &inst) {
            self.record(name, inst, Err(err));
            return;
        }
        if self.cache.graph.has_bound_tree_disagreement(&inst) {
            let (name, err) = demand_clash(self.ws, self.scx, &inst);
            self.record(name, inst, Err(err));
            return;
        }
        if let Some(Ok(c)) = self.cache.chunks.get(&inst) {
            for d in &c.specs {
                self.admit(d.inst.clone(), jobs);
            }
            let name = inst.symbol(self.ws, self.scx.interner());
            self.record(name, inst, Ok(c.clone()));
            return;
        }
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            hook.scheduled(&inst.symbol(self.ws, self.scx.interner()));
        }
        jobs.push(inst);
    }

    fn record(&self, name: String, inst: Instance, result: Result<Chunk, CompilerError>) {
        self.outcomes
            .lock()
            .expect("spec outcomes")
            .push((name, inst, result));
    }
}

pub(super) fn view_driven_specialized_chunks(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    cache: &SkullEmitCache,
    initial: &[skull_mir::bound::SpecDemand],
) -> (FxHashMap<Instance, Chunk>, Vec<(String, CompilerError)>) {
    let queue = SpecQueue {
        ws,
        scx,
        cache,
        done: std::sync::Mutex::new(FxHashSet::default()),
        outcomes: std::sync::Mutex::new(Vec::new()),
        #[cfg(test)]
        hook: SPEC_SCHEDULE_HOOK.with(|hook| hook.borrow().clone()),
    };
    let mut jobs: Vec<Instance> = Vec::new();
    for d in initial {
        queue.admit(d.inst.clone(), &mut jobs);
    }
    for module in ws.source_modules() {
        for (fid, f) in module.functions.iter() {
            let self_ref = FuncRef {
                module: module.id,
                func: fid,
            };
            if !is_cached_emitted_body(f, &shared(self_ref))
                || !should_emit_via_view(ws, &cache.lua_view, self_ref)
            {
                continue;
            }
            if let Some(Ok(c)) = cache.chunks.get(&shared(self_ref)) {
                for d in &c.specs {
                    queue.admit(d.inst.clone(), &mut jobs);
                }
            }
        }
    }
    // Each built chunk's own demand is queued as soon as that chunk is done,
    // so a deep demand chain never waits on an unrelated body's build, and
    // every worker keeps its inline cache for the whole closure.
    par_drain(
        jobs,
        || match cache.inline_cache.candidate_table() {
            Some(table) => skull_transform::InlineCache::with_candidate_table(table),
            None => skull_transform::InlineCache::default(),
        },
        |inline_cache, inst, follow_ups| {
            let name = inst.symbol(ws, scx.interner());
            let mut specs = Vec::new();
            let result = emit_spec_chunk(
                ws,
                cg,
                scx,
                &cache.concrete,
                &inst,
                &name,
                inline_cache,
                &mut specs,
            );
            #[cfg(test)]
            if let Some(hook) = &queue.hook {
                hook.finished(&name);
            }
            if let Ok(chunk) = &result {
                for d in &chunk.specs {
                    queue.admit(d.inst.clone(), follow_ups);
                }
            }
            queue.record(name, inst, result);
        },
    );
    let mut outcomes = queue.outcomes.into_inner().expect("spec outcomes");
    // Completion order is whatever the pool made it; the symbol fixes the
    // installation order (a tie falls back to the instance itself).
    outcomes.sort_by(|(a, ia, _), (b, ib, _)| {
        a.cmp(b)
            .then_with(|| format!("{ia:?}").cmp(&format!("{ib:?}")))
    });
    let mut built: FxHashMap<Instance, Chunk> = FxHashMap::default();
    let mut errs: Vec<(String, CompilerError)> = Vec::new();
    for (name, inst, result) in outcomes {
        #[cfg(test)]
        if let Some(hook) = &queue.hook {
            hook.installed(&name, result.is_ok());
        }
        match result {
            Ok(chunk) => {
                built.insert(inst, chunk);
            }
            Err(err) => errs.push((name, err)),
        }
    }
    (built, errs)
}

/// The shared body of `fref` -- the id under which a declaration's own
/// definition is cached. Written out at every chunk lookup so the day a
/// SPECIALIZED body answers one of them is a visible change rather than a
/// silent widening.
pub(super) fn shared(fref: FuncRef) -> Instance {
    Instance::base(fref)
}

/// Whether `self_ref`'s OWN ordinary/coroutine chunk must be emitted, per
/// `view` -- the exact predicate a population-source cutover would gate the
/// module-fn loop's `emit_def` with. TWO independent reasons, matching this
/// session's author decision:
///
/// - **call-serving**: `self_ref` is the declaration `realize_decl` SELECTED
///   to serve calls to its canonical `Instance` (which may be a DIFFERENT
///   `FuncRef` than `self_ref` when `self_ref` is a non-representative
///   sibling, and vice versa when `self_ref` IS the representative but a
///   same-name sibling serves calls instead).
/// - **publication**: `self_ref` carries an explicit `@nomangle` root whose
///   own bare-name promise the artifact must publish regardless of whether
///   any resolved call reaches it that way.
///
/// A declaration can be neither (pruned), either, or -- the whole point of
/// modeling publication separately -- BOTH under two different symbols.
pub(super) fn should_emit_via_view(
    ws: &Workspace,
    view: &crate::artifact::ArtifactView,
    self_ref: FuncRef,
) -> bool {
    let canonical = skull_mir::lower::canonical_variant(ws, self_ref);
    let call_serving = view
        .realization
        .get(&Instance::base(canonical))
        .is_some_and(|r| r.decl == self_ref);
    let publication = view.publications.contains_key(&Instance::base(self_ref));
    call_serving || publication
}

/// Whether a FOREIGN (`inline(lua)`) body needs a standalone definition.
///
/// Retention alone over-emits here: a `@pure @inline` stdlib operator is
/// retained (something calls `+`) and spliced away at every call site, so the
/// question is delivery, not membership. Four independent reasons force a
/// definition -- a bare-name publication promise, being a DECLARED root in its
/// own right (an own-project `inline(lua)` fn is rooted with no caller at all,
/// same reasoning as `main`/`@sim`/`@test`), a residual non-spliced call, or a
/// FUNCTION-VALUE take, which is not a call and so has nothing to splice into:
/// the program stores the definition itself under its own symbol.
///
/// `declared_roots`, not `roots`: the latter also carries the graph's own
/// "callee of an always-emitted body, no incoming edge" category, which is an
/// ordinary reached instance subject to the same splicing question (found
/// empirically -- `__op_mul`/`__op_div`, reached from a global initializer's
/// constant expression, are not declared roots and are spliced away
/// everywhere). A generic declaration has no base instance and so is never in
/// the set.
///
/// ONE oracle: the definition loop and the emitted-body census both call it, so
/// they cannot disagree about what the assembly contains.
pub(super) fn emits_foreign_definition(
    ws: &Workspace,
    view: &crate::artifact::ArtifactView,
    needed_foreign: &FxHashSet<Instance>,
    self_ref: FuncRef,
) -> bool {
    let canonical = shared(skull_mir::lower::canonical_variant(ws, self_ref));
    view.publications.contains_key(&shared(self_ref))
        || view.declared_roots.contains(&canonical)
        || view.fn_value_targets.contains(&canonical)
        || needed_foreign.contains(&shared(self_ref))
}
