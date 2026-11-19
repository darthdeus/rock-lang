use super::*;

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

pub(super) fn concrete_stats_requested() -> bool {
    std::env::var("ROCK_LOG").is_ok_and(|value| value == "debug")
}

fn warm_candidates(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    keys: Vec<Instance>,
) -> Result<Vec<skull_transform::InlineCandidateWarm>, CompilerError> {
    par_map_with(
        keys,
        skull_transform::InlineCache::default,
        |inline_cache, inst| inline_cache.warm_candidate(ws, scx, inst, concrete.plans.as_ref()),
    )
    .into_iter()
    .collect()
}

pub(super) fn build_inline_candidate_table(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
) -> Result<std::sync::Arc<skull_transform::InlineCandidateTable>, CompilerError> {
    let keys: Vec<Instance> = concrete.candidate_keys().iter().cloned().collect();
    let warms = warm_candidates(ws, scx, concrete, keys.clone())?;
    Ok(std::sync::Arc::new(
        skull_transform::InlineCandidateTable::from_warms_with_base(None, keys, warms),
    ))
}

pub(super) fn update_inline_candidate_table(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    base: Option<&std::sync::Arc<skull_transform::InlineCandidateTable>>,
    warm_keys: Vec<Instance>,
    added: &BTreeSet<Instance>,
    removed: &BTreeSet<Instance>,
) -> Result<std::sync::Arc<skull_transform::InlineCandidateTable>, CompilerError> {
    let warms = warm_candidates(ws, scx, concrete, warm_keys)?;
    Ok(std::sync::Arc::new(match base {
        Some(base) => skull_transform::InlineCandidateTable::from_warms_incremental(
            std::sync::Arc::clone(base),
            warms,
            &added.iter().cloned().collect::<Vec<_>>(),
            &removed.iter().cloned().collect::<Vec<_>>(),
        ),
        None => {
            skull_transform::InlineCandidateTable::from_warms_with_base(None, Vec::new(), warms)
        }
    }))
}

type SpecOutcome = (String, Instance, Result<Chunk, CompilerError>);

pub(super) type SpecChunks = (FxHashMap<Instance, Chunk>, Vec<(String, CompilerError)>);

pub(super) struct SpecQueue<'a> {
    ws: &'a Workspace,
    scx: &'a skull_mir::lower::ScalarCtx,
    dissenters: FxHashSet<Instance>,
    cached: Option<&'a FxHashMap<Instance, Result<Chunk, CompilerError>>>,
    done: std::sync::Mutex<FxHashSet<Instance>>,
    outcomes: std::sync::Mutex<Vec<SpecOutcome>>,
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

    pub(super) fn admit_all(
        &self,
        demands: &[skull_mir::bound::SpecDemand],
        jobs: &mut Vec<Instance>,
    ) {
        for d in demands {
            self.admit(d.inst.clone(), jobs);
        }
    }

    fn admit(&self, inst: Instance, jobs: &mut Vec<Instance>) {
        if !self
            .done
            .lock()
            .expect("spec demand set")
            .insert(inst.clone())
        {
            return;
        }
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

    pub(super) fn finish(self) -> SpecChunks {
        let mut outcomes: Vec<Option<SpecOutcome>> = self
            .outcomes
            .into_inner()
            .expect("spec outcomes")
            .into_iter()
            .map(Some)
            .collect();
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

pub(super) fn shared(fref: FuncRef) -> Instance {
    Instance::base(fref)
}

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
