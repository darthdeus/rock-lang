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
    // The old incremental route left this unset and filtered every key here.
    // Count the exact iterator population so a mutation that restores that
    // route is visible rather than looking like a timing-only regression.
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

/// One demanded instance's outcome, keyed by its symbol so the queue's
/// completion order never shows in what is installed.
type SpecOutcome = (String, Instance, Result<Chunk, CompilerError>);

/// The specialized chunks a demand closure built, and the instances it
/// rejected, in installation order.
pub(super) type SpecChunks = (FxHashMap<Instance, Chunk>, Vec<(String, CompilerError)>);

/// Build each demanded specialization once. Inlining can deliver a retained
/// specialization entirely through its callers.
pub(super) struct SpecQueue<'a> {
    ws: &'a Workspace,
    scx: &'a skull_mir::lower::ScalarCtx,
    /// The instances the graph reached with two different witness trees. A
    /// set rather than the graph, so a one-shot build can free the graph's
    /// records before the fan-out that reads this.
    dissenters: FxHashSet<Instance>,
    /// A retained cache's chunks, whose specialized entries are reused rather
    /// than built again. A full build has none.
    cached: Option<&'a FxHashMap<Instance, Result<Chunk, CompilerError>>>,
    done: std::sync::Mutex<FxHashSet<Instance>>,
    outcomes: std::sync::Mutex<Vec<SpecOutcome>>,
    /// Keep each built body's attribution record, with its thread CPU time;
    /// only a build that folds attribution reads them.
    pub(super) attribution: bool,
}

impl<'a> SpecQueue<'a> {
    pub(super) fn new(
        ws: &'a Workspace,
        scx: &'a skull_mir::lower::ScalarCtx,
        graph: &crate::instance_graph::InstanceGraph,
        cached: Option<&'a FxHashMap<Instance, Result<Chunk, CompilerError>>>,
    ) -> Self {
        Self {
            ws,
            scx,
            dissenters: graph.bound_tree_dissenters(),
            cached,
            done: std::sync::Mutex::new(FxHashSet::default()),
            outcomes: std::sync::Mutex::new(Vec::new()),
            attribution: false,
        }
    }

    /// Admit every instance `demands` names; new build jobs go to `jobs`.
    pub(super) fn admit_all(
        &self,
        demands: &[skull_mir::bound::SpecDemand],
        jobs: &mut Vec<Instance>,
    ) {
        for d in demands {
            self.admit(d.inst.clone(), jobs);
        }
    }

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
        if self.dissenters.contains(&inst) {
            let (name, err) = demand_clash(self.ws, self.scx, &inst);
            self.record(name, inst, Err(err));
            return;
        }
        if let Some(Ok(c)) = self.cached.and_then(|cached| cached.get(&inst)) {
            self.admit_all(&c.specs, jobs);
            let name = inst.symbol(self.ws, self.scx.interner());
            self.record(name, inst, Ok(c.clone()));
            return;
        }
        jobs.push(inst);
    }

    /// Build one admitted job's chunk and admit its own demand to `follow_ups`.
    pub(super) fn build(
        &self,
        cg: &CodegenCtx,
        concrete: &crate::concrete_body::ConcreteBodies,
        inline_cache: &mut skull_transform::InlineCache,
        inst: Instance,
        follow_ups: &mut Vec<Instance>,
    ) {
        let name = inst.symbol(self.ws, self.scx.interner());
        let started = self.attribution.then(thread_cpu_time);
        let mut result =
            emit_spec_chunk(self.ws, cg, self.scx, concrete, &inst, &name, inline_cache);
        if let Ok(chunk) = &mut result {
            chunk.finish(started);
        }
        if let Ok(chunk) = &result {
            self.admit_all(&chunk.specs, follow_ups);
        }
        self.record(name, inst, result);
    }

    fn record(&self, name: String, inst: Instance, result: Result<Chunk, CompilerError>) {
        self.outcomes
            .lock()
            .expect("spec outcomes")
            .push((name, inst, result));
    }

    /// Install every outcome in symbol order, whatever order the pool made.
    pub(super) fn finish(self) -> SpecChunks {
        let mut outcomes: Vec<Option<SpecOutcome>> = self
            .outcomes
            .into_inner()
            .expect("spec outcomes")
            .into_iter()
            .map(Some)
            .collect();
        // Sorted by position so the chunks themselves are moved once. A tie
        // falls back to the instance itself.
        let mut order: Vec<usize> = (0..outcomes.len()).collect();
        let key = |at: usize| outcomes[at].as_ref().expect("unsorted outcome");
        order.sort_by(|&a, &b| {
            let ((a, ia, _), (b, ib, _)) = (key(a), key(b));
            a.cmp(b)
                .then_with(|| format!("{ia:?}").cmp(&format!("{ib:?}")))
        });
        let mut built: FxHashMap<Instance, Chunk> =
            FxHashMap::with_capacity_and_hasher(outcomes.len(), Default::default());
        let mut errs: Vec<(String, CompilerError)> = Vec::new();
        for at in order {
            let (name, inst, result) = outcomes[at].take().expect("each outcome installs once");
            match result {
                Ok(chunk) => {
                    built.insert(inst, chunk);
                }
                Err(err) => errs.push((name, err)),
            }
        }
        (built, errs)
    }
}

/// A worker's inline cache, over the build's shared candidate table.
pub(super) fn worker_inline_cache(
    table: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
) -> skull_transform::InlineCache {
    match table {
        Some(table) => {
            skull_transform::InlineCache::with_candidate_table(std::sync::Arc::clone(table))
        }
        None => skull_transform::InlineCache::default(),
    }
}

/// Drain `jobs` and everything they demand through `queue`. Each built
/// chunk's own demand is queued as soon as that chunk is done, so a deep
/// demand chain never waits on an unrelated body's build, and every worker
/// keeps its inline cache for the whole closure.
pub(super) fn drain_spec_jobs(
    queue: &SpecQueue<'_>,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    table: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
    jobs: Vec<Instance>,
) {
    if jobs.is_empty() {
        return;
    }
    par_drain(
        jobs,
        || worker_inline_cache(table),
        |inline_cache, inst, follow_ups| queue.build(cg, concrete, inline_cache, inst, follow_ups),
    );
}

/// The incremental driver of the demand closure: seeded from `initial` and
/// the retained chunk of every ordinary/coroutine declaration
/// `should_emit_via_view` retains.
pub(super) fn view_driven_specialized_chunks(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    cache: &SkullEmitCache,
    initial: &[skull_mir::bound::SpecDemand],
) -> SpecChunks {
    let mut queue = SpecQueue::new(ws, scx, &cache.graph, Some(&cache.chunks));
    queue.attribution = cache.attribution;
    let mut jobs: Vec<Instance> = Vec::new();
    queue.admit_all(initial, &mut jobs);
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
                queue.admit_all(&c.specs, &mut jobs);
            }
        }
    }
    drain_spec_jobs(
        &queue,
        cg,
        &cache.concrete,
        cache.inline_cache.candidate_table().as_ref(),
        jobs,
    );
    queue.finish()
}

/// The shared body of `fref` -- the id under which a declaration's own
/// definition is cached. Written out at every chunk lookup so the day a
/// SPECIALIZED body answers one of them is a visible change rather than a
/// silent widening.
pub(super) fn shared(fref: FuncRef) -> Instance {
    Instance::base(fref)
}

/// A definition can serve calls through a sibling group, fulfill an exact
/// bare-name publication, or do both.
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

/// An opaque body needs a definition for a publication, explicit root,
/// function value, or residual call.
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
