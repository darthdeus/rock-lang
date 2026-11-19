use super::*;

pub(super) type EffectSnapshot = FxHashMap<FuncRef, bone_core::effects::MemEffect>;

fn current_effect_snapshot(
    ws: &Workspace,
    functions: impl IntoIterator<Item = FuncRef>,
) -> EffectSnapshot {
    let Some(effects) = ws.vec_effects.get() else {
        return FxHashMap::default();
    };
    functions
        .into_iter()
        .map(|fref| (fref, effects.of(fref.module, fref.func)))
        .collect()
}

fn rebuild_chunk(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    body: &Instance,
    f: &bone_semantic::hir::Function,
    inline_cache: &mut skull_transform::InlineCache,
) -> Result<Chunk, CompilerError> {
    match body.args.is_empty() {
        true => emit_body_chunk(ws, cg, scx, concrete, body.func, f, inline_cache),
        false => {
            let name = body.symbol(ws, scx.interner());
            emit_spec_chunk(ws, cg, scx, concrete, body, &name, inline_cache)
        }
    }
}

/// Per-body emit cache for incremental codegen, keyed by [`Instance`].
pub struct SkullEmitCache {
    pub(super) chunks: FxHashMap<Instance, Result<Chunk, CompilerError>>,
    pub(super) value_ctors: crate::ValueCtors,
    pub(super) attribution: bool,
    pub(super) concrete: crate::concrete_body::ConcreteBodies,
    pub(super) graph: crate::instance_graph::InstanceGraph,
    pub(super) lua_view: crate::artifact::ArtifactView,
    pub(super) changed_since_full: FxHashSet<Instance>,
    pub(super) cg: CodegenCtx,
    pub(super) scx: skull_mir::lower::ScalarCtx,
    pub(super) inline_cache: skull_transform::InlineCache,
    pub(super) dependents: FxHashMap<FuncRef, FxHashSet<Instance>>,
    pub(super) effect_snapshot: EffectSnapshot,
    pub(super) changed_consts_since_full: FxHashSet<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)>,
    pub(super) static_cells_since_full: FxHashMap<Ustr, StaticBaseline>,
    pub(super) roots: RootCache,
    pub(super) shader_registrations: ShaderRegistrations,
    pub(super) shader_view: crate::shader_artifact::ShaderArtifactView,
    pub(super) emitted: FxHashSet<Instance>,
    pub(super) code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    pub(super) lifecycle_plan: Option<std::sync::Arc<skull_mir::lifecycle::LifecyclePlan>>,
    pub(super) global_layout: Option<String>,
    pub(super) definition_plan: Option<std::sync::Arc<skull_mir::definition::DefinitionPlan>>,
    pub(super) artifact_fingerprint: Option<String>,
    pub(super) fingerprint_code_rows: Option<crate::artifact_fingerprint::CodeRows>,
    pub(super) unit_comptime_reads: UnitComptimeReads,
    pub(super) renderer_target: RendererTarget,
    pub(super) publish_fingerprint: bool,
    pub(super) global_accessors: bool,
    pub(super) build_kind: BuildKind,
    pub(super) selected_sim_functions: Option<FxHashSet<Ustr>>,
    pub(super) repair_stats: IncrementalRepairStats,
    pub(super) body_edit_rollback: Option<BodyEditRollback>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct IncrementalRepairStats {
    pub graph_update_ns: u64,
    pub concrete_update_ns: u64,
    pub artifact_update_ns: u64,
    pub candidate_update_ns: u64,
    pub affected_selection_ns: u64,
    pub relowering_ns: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheLifetime {
    Retained,
    OneShot,
}

#[derive(Clone, Default)]
pub(super) struct RootCache {
    pub(super) interface_roots: Vec<FuncRef>,
    pub(super) sim_roots: Vec<FuncRef>,
    pub(super) shader_roots: Vec<FuncRef>,
    pub(super) roots: Vec<FuncRef>,
}

pub(super) struct BodyEditRollback {
    reinferred: Vec<(ModuleId, BodyOwner)>,
    edits: Vec<crate::instance_graph::TextEditDelta>,
    changed_since_full: FxHashSet<Instance>,
    changed_consts_since_full: FxHashSet<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)>,
    effect_snapshot: EffectSnapshot,
    static_cells_since_full: FxHashMap<Ustr, StaticBaseline>,
    emitted: FxHashSet<Instance>,
    code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    lifecycle_plan: Option<std::sync::Arc<skull_mir::lifecycle::LifecyclePlan>>,
    global_layout: Option<String>,
    definition_plan: Option<std::sync::Arc<skull_mir::definition::DefinitionPlan>>,
    artifact_fingerprint: Option<String>,
    fingerprint_code_rows: Option<crate::artifact_fingerprint::CodeRows>,
    unit_comptime_reads: UnitComptimeReads,
    repair_stats: IncrementalRepairStats,
}

pub(super) fn comptime_reads_stale(
    ws: &Workspace,
    reads: &[skull_mir::mir::ComptimeRead],
    file_hashes: &mut FxHashMap<Ustr, Option<u64>>,
) -> bool {
    use skull_mir::mir::{ComptimeInputId, comptime_value_hash};
    reads.iter().any(|read| {
        let current = match &read.input {
            ComptimeInputId::Var(name) => ws
                .comptime_inputs
                .vars
                .get(name)
                .map(|v| comptime_value_hash(&v.value)),
            ComptimeInputId::File(path) => *file_hashes.entry(*path).or_insert_with(|| {
                let text = ws.comptime_inputs.embed_files.reread(path.as_str()).ok()?;
                Some(comptime_value_hash(&text))
            }),
        };
        current != Some(read.value_hash)
    })
}

impl SkullEmitCache {
    pub fn registered_sim_names(&self, ws: &Workspace) -> std::collections::HashSet<String> {
        collect_sim_fns(ws)
            .into_iter()
            .filter(|(_, f, _)| {
                self.selected_sim_functions
                    .as_ref()
                    .is_none_or(|names| names.contains(&f.name))
            })
            .map(|(_, _, wire)| wire)
            .collect()
    }

    pub(super) fn refresh_effect_snapshot(&mut self, ws: &Workspace) {
        self.effect_snapshot = current_effect_snapshot(
            ws,
            self.dependents
                .iter()
                .filter(|(_, callers)| !callers.is_empty())
                .map(|(&callee, _)| callee),
        );
    }

    pub fn begin_body_edit_transaction(
        &mut self,
        reinferred: &[(ModuleId, BodyOwner)],
        edits: &[crate::instance_graph::TextEditDelta],
    ) {
        assert!(
            self.body_edit_rollback.is_none(),
            "body edit transaction already active"
        );
        self.body_edit_rollback = Some(BodyEditRollback {
            reinferred: reinferred.to_vec(),
            edits: edits.to_vec(),
            changed_since_full: self.changed_since_full.clone(),
            changed_consts_since_full: self.changed_consts_since_full.clone(),
            effect_snapshot: self.effect_snapshot.clone(),
            static_cells_since_full: self.static_cells_since_full.clone(),
            emitted: self.emitted.clone(),
            code_artifact: self.code_artifact.clone(),
            lifecycle_plan: self.lifecycle_plan.clone(),
            global_layout: self.global_layout.clone(),
            definition_plan: self.definition_plan.clone(),
            artifact_fingerprint: self.artifact_fingerprint.clone(),
            fingerprint_code_rows: self.fingerprint_code_rows.clone(),
            unit_comptime_reads: self.unit_comptime_reads.clone(),
            repair_stats: self.repair_stats,
        });
    }

    pub fn commit_body_edit_transaction(&mut self) {
        self.body_edit_rollback = None;
    }

    pub fn rollback_body_edit(&mut self, ws: &Workspace) -> Result<(), CompilerError> {
        let Some(snapshot) = self.body_edit_rollback.take() else {
            return Ok(());
        };
        let BodyEditRollback {
            reinferred,
            edits,
            changed_since_full,
            changed_consts_since_full,
            effect_snapshot,
            static_cells_since_full,
            emitted,
            code_artifact,
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint,
            fingerprint_code_rows,
            unit_comptime_reads,
            repair_stats,
        } = snapshot;
        let inverse_edits: Vec<_> = edits
            .iter()
            .map(|edit| crate::instance_graph::TextEditDelta {
                file: edit.file,
                at: edit.at,
                byte_delta: -edit.byte_delta,
                line_delta: -edit.line_delta,
            })
            .collect();
        let result = (|| {
            let shader_changed = self.update_shader_registrations(ws, &reinferred)?;
            let (_affected, update, added_errors, _fingerprint_identities) =
                self.update_chunk_cache(ws, &reinferred, &inverse_edits)?;
            if let Some(error) = Self::combine_errors(added_errors) {
                return Err(error);
            }
            if shader_changed || self.shader_delta_affects_view(&update) {
                self.refresh_shader_registrations(ws)?;
            }
            Ok(())
        })();
        self.changed_since_full = changed_since_full;
        self.changed_consts_since_full = changed_consts_since_full;
        self.effect_snapshot = effect_snapshot;
        self.static_cells_since_full = static_cells_since_full;
        self.emitted = emitted;
        self.code_artifact = code_artifact;
        self.lifecycle_plan = lifecycle_plan;
        self.global_layout = global_layout;
        self.definition_plan = definition_plan;
        self.artifact_fingerprint = artifact_fingerprint;
        self.fingerprint_code_rows = fingerprint_code_rows;
        self.unit_comptime_reads = unit_comptime_reads;
        self.repair_stats = repair_stats;
        result
    }

    pub(super) fn candidate_index_by_func(&self) -> &FxHashMap<FuncRef, FxHashSet<Instance>> {
        self.concrete.candidate_keys_by_func()
    }

    /// Every `embed_string` input baked into the program, with the hash of
    /// the baked bytes: the files a watcher must treat as sources.
    pub fn comptime_file_hashes(&self) -> Vec<(Ustr, u64)> {
        let mut out: FxHashMap<Ustr, u64> = FxHashMap::default();
        let chunk_reads = self
            .chunks
            .values()
            .filter_map(|chunk| chunk.as_ref().ok())
            .flat_map(|chunk| chunk.comptime_reads.iter());
        let unit_reads = self
            .unit_comptime_reads
            .globals
            .values()
            .flatten()
            .chain(self.unit_comptime_reads.decorators.iter());
        for read in chunk_reads.chain(unit_reads) {
            if let skull_mir::mir::ComptimeInputId::File(path) = read.input {
                out.insert(path, read.value_hash);
            }
        }
        out.into_iter().collect()
    }

    /// The authoritative instance graph this cache emits against.
    pub fn graph(&self) -> &crate::instance_graph::InstanceGraph {
        &self.graph
    }

    /// The Lua artifact's retained realization of [`Self::graph`].
    pub fn lua_view(&self) -> &crate::artifact::ArtifactView {
        &self.lua_view
    }

    pub fn global_layout(&self) -> Option<&str> {
        self.global_layout.as_deref()
    }

    pub fn lifecycle_plan(&self) -> Option<&skull_mir::lifecycle::LifecyclePlan> {
        self.lifecycle_plan.as_deref()
    }

    pub fn body_edit_baseline_lifecycle_plan(
        &self,
    ) -> Option<&skull_mir::lifecycle::LifecyclePlan> {
        self.body_edit_rollback
            .as_ref()
            .and_then(|snapshot| snapshot.lifecycle_plan.as_deref())
    }

    pub fn artifact_fingerprint(&self) -> Option<&str> {
        self.artifact_fingerprint.as_deref()
    }

    /// A live shader registry cannot remove a key from the running process,
    /// so a key-set change must take the process-restarting reload path.
    pub fn shader_registration_keys_changed(&self, ws: &Workspace) -> Result<bool, CompilerError> {
        let graphics = skull_mir::lower::collect_shader_pairs(ws)?
            .into_iter()
            .map(|pair| pair.key);
        let current: BTreeSet<String> = graphics.collect();
        Ok(current != self.shader_registrations.keys)
    }

    /// [`Self::shader_registration_keys_changed`] over only the re-inferred bodies.
    pub fn shader_registration_keys_changed_for_bodies(
        &self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
    ) -> Result<bool, CompilerError> {
        let mut users = self.shader_registrations.shader_pair_users.clone();
        for &(mid, owner) in reinferred {
            if let Some(previous) = self
                .shader_registrations
                .shader_contributions
                .get(&(mid, owner))
            {
                for pair in previous {
                    let count = users
                        .get_mut(&pair.key)
                        .expect("shader key index lost a body contribution");
                    *count -= 1;
                    if *count == 0 {
                        users.remove(&pair.key);
                    }
                }
            }
            let (shader, _) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
            for pair in shader {
                *users.entry(pair.key).or_insert(0) += 1;
            }
        }
        let current: BTreeSet<String> = users.keys().cloned().collect();
        Ok(current != self.shader_registrations.keys)
    }

    pub(super) fn update_shader_registrations(
        &mut self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
    ) -> Result<bool, CompilerError> {
        let affected = reinferred.iter().any(|&(mid, owner)| {
            self.shader_registrations.callsites.contains(&(mid, owner))
                || matches!(
                    owner,
                    BodyOwner::Function(fid)
                        if self
                            .shader_registrations
                            .dependencies
                            .contains(&FuncRef { module: mid, func: fid })
                )
                || skull_mir::lower::body_has_shader_pipeline_call(ws, mid, owner)
        });
        if !affected {
            return Ok(false);
        }
        for &(mid, owner) in reinferred {
            let key = (mid, owner);
            let old_shader = self
                .shader_registrations
                .shader_contributions
                .remove(&key)
                .unwrap_or_default();
            for pair in old_shader {
                let users = self
                    .shader_registrations
                    .shader_pair_users
                    .get_mut(&pair.key)
                    .expect("shader contribution index lost an existing key");
                *users -= 1;
                if *users == 0 {
                    self.shader_registrations
                        .shader_pair_users
                        .remove(&pair.key);
                    self.shader_registrations.shader_pairs.remove(&pair.key);
                    if let Some(vert) = pair.vert {
                        self.shader_registrations.shader_root_set.remove(&vert);
                    }
                    self.shader_registrations.shader_root_set.remove(&pair.frag);
                }
            }

            let (shader, shader_callsite) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
            for pair in &shader {
                let users = self
                    .shader_registrations
                    .shader_pair_users
                    .entry(pair.key.clone())
                    .or_insert(0);
                *users += 1;
                if *users == 1 {
                    self.shader_registrations
                        .shader_pairs
                        .insert(pair.key.clone(), pair.clone());
                    if let Some(vert) = pair.vert {
                        self.shader_registrations.shader_root_set.insert(vert);
                    }
                    self.shader_registrations.shader_root_set.insert(pair.frag);
                }
            }
            self.shader_registrations
                .shader_contributions
                .insert(key, shader);
            if shader_callsite {
                self.shader_registrations.callsites.insert(key);
            } else {
                self.shader_registrations.callsites.remove(&key);
            }
        }
        let mut shader_roots: Vec<FuncRef> = self
            .shader_registrations
            .shader_root_set
            .iter()
            .copied()
            .collect();
        shader_roots.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
        self.shader_registrations.roots = shader_roots;
        self.shader_registrations.keys = self
            .shader_registrations
            .shader_pairs
            .keys()
            .cloned()
            .collect();
        Ok(true)
    }

    pub(super) fn refresh_shader_registrations(
        &mut self,
        ws: &Workspace,
    ) -> Result<(), CompilerError> {
        let collection = skull_mir::lower::ShaderCollection {
            pairs: self
                .shader_registrations
                .shader_pairs
                .values()
                .cloned()
                .collect(),
            callsites: self
                .shader_registrations
                .callsites
                .iter()
                .copied()
                .collect(),
            contributions: self.shader_registrations.shader_contributions.clone(),
        };
        let programs = shader_programs(&collection);
        let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
            .sharing(std::sync::Arc::clone(self.graph.interner()));
        let view = realize_shader_view(
            ws,
            &self.graph,
            &programs,
            &shader_scx,
            std::sync::Arc::clone(&self.concrete.plans),
        )?;
        let registrations = shader_registration_lines(ws, &shader_scx, &collection, &view)?;
        self.shader_view = view;
        self.shader_registrations = registrations;
        Ok(())
    }

    pub(super) fn shader_delta_affects_view(
        &self,
        update: &crate::instance_graph::UpdateStats,
    ) -> bool {
        let shader_decls: FxHashSet<FuncRef> = self
            .shader_view
            .roots
            .iter()
            .chain(self.shader_view.helpers.iter())
            .chain(self.shader_view.builtins.iter())
            .map(|instance| instance.func)
            .collect();
        update
            .added
            .iter()
            .chain(update.removed.iter())
            .any(|instance| shader_decls.contains(&instance.func))
    }

    pub(super) fn retain_static_cells(&mut self, ws: &Workspace) -> Result<(), CompilerError> {
        for chunk in self.chunks.values().flatten() {
            for sc in &chunk.statics {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.static_cells_since_full.entry(sc.cell)
                {
                    entry.insert(
                        StaticBaseline::of(ws, self.scx.interner(), sc)
                            .map_err(CompilerError::new_no_location)?,
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn absorb_spec_chunks(&mut self, built: Vec<(Instance, Chunk)>) -> usize {
        let mut dependency_edges = 0;
        for (body, chunk) in built {
            dependency_edges += chunk.deps.len();
            for d in &chunk.deps {
                self.dependents.entry(*d).or_default().insert(body.clone());
            }
            self.chunks.insert(body, Ok(chunk));
        }
        dependency_edges
    }

    fn forget_instance(&mut self, gone: &Instance) {
        if let Some(Ok(c)) = self.chunks.get(gone) {
            let deps: Vec<FuncRef> = c.deps.iter().copied().collect();
            for d in deps {
                if let Some(set) = self.dependents.get_mut(&d) {
                    set.remove(gone);
                }
            }
        }
        self.chunks.remove(gone);
        self.changed_since_full.remove(gone);
        self.emitted.remove(gone);
    }

    fn build_graph_added_chunks(
        cache: &mut SkullEmitCache,
        ws: &Workspace,
        added: &std::collections::BTreeSet<Instance>,
    ) -> (
        Vec<CompilerError>,
        FxHashSet<skull_mir::code_identity::CodeIdentity>,
    ) {
        let mut fingerprint_identities = FxHashSet::default();
        let mut pending: Vec<Instance> = added.iter().cloned().collect();
        let mut seen: FxHashSet<Instance> = FxHashSet::default();
        let mut built: Vec<(Instance, Chunk)> = Vec::new();
        let mut errs = Vec::new();

        while let Some(body) = pending.pop() {
            if !seen.insert(body.clone()) {
                continue;
            }
            let f = &ws.module(body.func.module).functions[body.func.func];
            if !is_cached_emitted_body(f, &body) {
                continue;
            }
            if let Some(Ok(chunk)) = cache.chunks.get(&body) {
                if !cache.emitted.contains(&body) {
                    cache.changed_since_full.insert(body.clone());
                    fingerprint_identities.extend(
                        chunk
                            .code_artifact
                            .entries()
                            .iter()
                            .map(|entry| entry.identity.clone()),
                    );
                }
                continue;
            }
            let chunk = if body.args.is_empty() {
                match rebuild_chunk(
                    ws,
                    &cache.cg,
                    &cache.scx,
                    &cache.concrete,
                    &body,
                    f,
                    &mut cache.inline_cache,
                ) {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        errs.push(err);
                        continue;
                    }
                }
            } else {
                let name = body.symbol(ws, cache.scx.interner());
                match emit_spec_chunk(
                    ws,
                    &cache.cg,
                    &cache.scx,
                    &cache.concrete,
                    &body,
                    &name,
                    &mut cache.inline_cache,
                ) {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        errs.push(err);
                        continue;
                    }
                }
            };
            let mut valid = true;
            for demand in &chunk.specs {
                if cache.graph.concreteness(&demand.inst)
                    == crate::instance_graph::Concreteness::Missing
                {
                    errs.push(CompilerError::new_with_label(
                        f.name_span,
                        format!(
                            "graph omitted added specialization `{}` demanded by `{}`",
                            demand.inst.symbol(ws, cache.scx.interner()),
                            body.symbol(ws, cache.scx.interner())
                        ),
                    ));
                    valid = false;
                    continue;
                }
                pending.push(demand.inst.clone());
            }
            if valid {
                built.push((body, chunk));
            }
        }

        for (body, chunk) in built {
            fingerprint_identities.extend(
                chunk
                    .code_artifact
                    .entries()
                    .iter()
                    .map(|entry| entry.identity.clone()),
            );
            for dep in &chunk.deps {
                cache
                    .dependents
                    .entry(*dep)
                    .or_default()
                    .insert(body.clone());
            }
            cache.chunks.insert(body.clone(), Ok(chunk));
            cache.changed_since_full.insert(body);
        }
        (errs, fingerprint_identities)
    }

    pub(super) fn combine_errors(mut errors: Vec<CompilerError>) -> Option<CompilerError> {
        let first = errors.first()?.clone();
        errors.remove(0);
        Some(first.with_siblings(errors))
    }

    pub(super) fn update_chunk_cache(
        &mut self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
        edits: &[crate::instance_graph::TextEditDelta],
    ) -> Result<
        (
            FxHashSet<Instance>,
            crate::instance_graph::UpdateStats,
            Vec<CompilerError>,
            FxHashSet<skull_mir::code_identity::CodeIdentity>,
        ),
        CompilerError,
    > {
        let cache = self;
        let changed_literals = if reinferred
            .iter()
            .any(|(_, owner)| matches!(owner, BodyOwner::Global(_)))
        {
            cache.cg.refresh_scalar_constants(ws)
        } else {
            FxHashSet::default()
        };
        if reinferred.iter().any(|&(mid, owner)| match owner {
            BodyOwner::Global(_) => true,
            BodyOwner::Function(func) => ws.module(mid).functions[func].inline_body.is_some(),
            _ => false,
        }) {
            for name in cache.cg.refresh_unread_constants(ws) {
                cache
                    .changed_consts_since_full
                    .extend(ws.source_modules().flat_map(|m| {
                        m.globals
                            .iter()
                            .filter(move |(_, g)| g.name == name)
                            .map(move |(gid, _)| (m.id, gid))
                    }));
            }
        }
        let mut fingerprint_identities = FxHashSet::default();
        cache.repair_stats = IncrementalRepairStats::default();
        let mut flipped_frefs: FxHashSet<FuncRef> = FxHashSet::default();
        for (mid, owner) in reinferred {
            for name in cache.scx.update_body(ws, *mid, *owner) {
                if let Some(list) = ws.functions_by_name.get(&name) {
                    for (m, fid) in list.iter() {
                        flipped_frefs.insert(FuncRef {
                            module: *m,
                            func: *fid,
                        });
                    }
                }
            }
        }
        let t_graph = Instant::now();
        let previous_shader_roots = cache.roots.shader_roots.clone();
        let current_shader_roots = cache.shader_registrations.roots.clone();
        let shader_roots_changed = previous_shader_roots != current_shader_roots;
        let mut root_added = Vec::new();
        let mut root_removed = Vec::new();
        if shader_roots_changed {
            let mut current_roots = cache.roots.roots.clone();
            for root in previous_shader_roots
                .iter()
                .filter(|root| !current_shader_roots.contains(root))
            {
                if cache.roots.interface_roots.contains(root)
                    || cache.roots.sim_roots.contains(root)
                {
                    continue;
                }
                if let Some(position) = current_roots.iter().position(|current| current == root) {
                    current_roots.remove(position);
                    root_removed.push(*root);
                }
            }
            for root in current_shader_roots
                .iter()
                .filter(|root| !previous_shader_roots.contains(root))
            {
                if !current_roots.contains(root) {
                    current_roots.push(*root);
                    root_added.push(*root);
                }
            }
            current_roots.sort_by_key(|root| (root.module.index(), root.func.into_raw()));
            cache.roots.roots = current_roots;
        }
        cache.roots.shader_roots = current_shader_roots;
        cache.lua_view.prepare_update(&cache.graph);
        let update = cache.graph.update_bodies_with_root_delta(
            ws,
            &root_added,
            &root_removed,
            reinferred,
            edits,
        );
        cache.repair_stats.graph_update_ns = t_graph.elapsed().as_nanos() as u64;
        let t_concrete = Instant::now();
        let concrete_update = cache.concrete.update_local(
            ws,
            &cache.graph,
            &update.revisited,
            &update.removed,
            &update.revisited_owners,
        );
        cache.repair_stats.concrete_update_ns = t_concrete.elapsed().as_nanos() as u64;

        for gone in &update.removed {
            cache.forget_instance(gone);
        }

        let t_artifact = Instant::now();
        cache.lua_view.update_local_with_root_delta(
            ws,
            &cache.graph,
            &crate::artifact::ArtifactRequest::new(
                crate::target_projection::LUAJIT_DESKTOP,
                cache.roots.roots.as_slice(),
            )
            .with_scalar_context(&cache.scx),
            &update.revisited,
            &update.added,
            &update.removed,
            &root_added,
            &root_removed,
        );
        cache.repair_stats.artifact_update_ns = t_artifact.elapsed().as_nanos() as u64;

        let scx = &cache.scx;
        let cg = &cache.cg;

        let t_aff = Instant::now();
        let mut changed: FxHashSet<FuncRef> = reinferred
            .iter()
            .filter_map(|(mid, owner)| match owner {
                BodyOwner::Function(fid) => Some(FuncRef {
                    module: *mid,
                    func: *fid,
                }),
                _ => None,
            })
            .collect();
        changed.extend(flipped_frefs.iter().copied());

        let effect_changed: FxHashSet<FuncRef> =
            if cache.effect_snapshot.is_empty() && ws.vec_effects.get().is_none() {
                FxHashSet::default()
            } else {
                let effects = ws.effects();
                cache
                    .effect_snapshot
                    .iter()
                    .filter_map(|(&fref, previous)| {
                        (effects.of(fref.module, fref.func) != *previous).then_some(fref)
                    })
                    .collect()
            };

        {
            let mut file_hashes: FxHashMap<Ustr, Option<u64>> = FxHashMap::default();
            for (body, chunk) in cache.chunks.iter() {
                let Ok(chunk) = chunk else { continue };
                if comptime_reads_stale(ws, &chunk.comptime_reads, &mut file_hashes) {
                    changed.insert(body.func);
                }
            }
        }

        let mut invalidated_candidate_frefs = changed.clone();
        invalidated_candidate_frefs.extend(update.added.iter().map(|instance| instance.func));
        invalidated_candidate_frefs.extend(update.removed.iter().map(|instance| instance.func));
        let mut warm_keys = concrete_update.candidate_keys_added.clone();
        for fref in &invalidated_candidate_frefs {
            if let Some(keys) = cache.candidate_index_by_func().get(fref) {
                warm_keys.extend(keys.iter().cloned());
            }
        }
        let warm_keys: Vec<Instance> = warm_keys.into_iter().collect();
        let t_candidate = Instant::now();
        let previous_candidate_table = cache.inline_cache.candidate_table();
        let candidate_table = build_inline_candidate_table(
            ws,
            scx,
            &cache.concrete,
            Vec::new(),
            previous_candidate_table.as_ref(),
            Some(&invalidated_candidate_frefs),
            Some(warm_keys),
            Some(&concrete_update.candidate_keys_added),
            Some(&concrete_update.candidate_keys_removed),
        )?;
        let mut stable_residual_frefs = FxHashSet::default();
        if let Some(previous) = previous_candidate_table.as_ref() {
            for fref in &changed {
                if flipped_frefs.contains(fref)
                    || update
                        .added
                        .iter()
                        .chain(&update.removed)
                        .any(|key| key.func == *fref)
                    || concrete_update
                        .candidate_keys_added
                        .iter()
                        .chain(&concrete_update.candidate_keys_removed)
                        .any(|key| key.func == *fref)
                {
                    continue;
                }
                if let Some(keys) = cache.candidate_index_by_func().get(fref) {
                    if !keys.is_empty()
                        && keys
                            .iter()
                            .all(|key| previous.residual_eligibility_matches(&candidate_table, key))
                    {
                        stable_residual_frefs.insert(*fref);
                    }
                }
            }
        }
        cache.inline_cache.replace_candidate_table(candidate_table);
        cache.repair_stats.candidate_update_ns = t_candidate.elapsed().as_nanos() as u64;
        let mut affected: FxHashSet<Instance> = changed
            .iter()
            .copied()
            .map(shared)
            .filter(|body| cache.chunks.contains_key(body))
            .collect();
        if !changed_literals.is_empty() {
            affected.extend(cache.chunks.iter().filter_map(|(body, chunk)| {
                chunk
                    .as_ref()
                    .ok()
                    .filter(|chunk| {
                        chunk
                            .literal_globals
                            .iter()
                            .any(|name| changed_literals.contains(name))
                    })
                    .map(|_| body.clone())
            }));
        }
        for c in &changed {
            let instances: Vec<Instance> = cache.graph.instances_of(*c).cloned().collect();
            affected.extend(instances);
        }
        let dependency_seeds: FxHashSet<FuncRef> =
            changed.union(&effect_changed).copied().collect();
        for c in &dependency_seeds {
            if let Some(deps) = cache.dependents.get(c) {
                if effect_changed.contains(c) {
                    affected.extend(deps.iter().cloned());
                } else {
                    let demand = skull_mir::lower::parameter_demand_key(ws, *c);
                    affected.extend(
                        deps.iter()
                            .filter(|body| {
                                !stable_residual_frefs.contains(c)
                                    || !matches!(cache.chunks.get(*body), Some(Ok(chunk))
                            if !chunk.embedded_deps.contains(c)
                                && chunk.abi_deps.get(c).is_none_or(|key| *key == demand))
                            })
                            .cloned(),
                    );
                }
            }
        }
        cache.repair_stats.affected_selection_ns = t_aff.elapsed().as_nanos() as u64;

        let t_relow = Instant::now();
        for body in &affected {
            cache.inline_cache.invalidate(body.func);
        }
        for body in &affected {
            let old_deps: Vec<FuncRef> = match cache.chunks.get(body) {
                Some(Ok(c)) => c.deps.iter().copied().collect(),
                _ => Vec::new(),
            };
            for d in &old_deps {
                if let Some(set) = cache.dependents.get_mut(d) {
                    set.remove(body);
                }
            }
            let fref = body.func;
            let f = &ws.module(fref.module).functions[fref.func];
            if is_cached_emitted_body(f, body) {
                let chunk = rebuild_chunk(
                    ws,
                    cg,
                    scx,
                    &cache.concrete,
                    body,
                    f,
                    &mut cache.inline_cache,
                );
                if let Ok(c) = &chunk {
                    fingerprint_identities.extend(
                        c.code_artifact
                            .entries()
                            .iter()
                            .map(|entry| entry.identity.clone()),
                    );
                    let new_deps: Vec<FuncRef> = c.deps.iter().copied().collect();
                    for d in new_deps {
                        cache.dependents.entry(d).or_default().insert(body.clone());
                    }
                }
                cache.chunks.insert(body.clone(), chunk);
            } else {
                cache.chunks.remove(body);
            }
        }
        let (added_errors, added_fingerprint_identities) =
            Self::build_graph_added_chunks(cache, ws, &update.added);
        fingerprint_identities.extend(added_fingerprint_identities);
        cache.refresh_effect_snapshot(ws);
        cache.repair_stats.relowering_ns = t_relow.elapsed().as_nanos() as u64;
        Ok((affected, update, added_errors, fingerprint_identities))
    }
}

pub fn emit_incremental(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    let t_reemit = Instant::now();
    let shader_changed = cache.update_shader_registrations(ws, reinferred)?;
    let (_affected, update, added_errors, _fingerprint_identities) =
        cache.update_chunk_cache(ws, reinferred, &[])?;
    if let Some(err) = SkullEmitCache::combine_errors(added_errors) {
        return Err(err);
    }
    if shader_changed || cache.shader_delta_affects_view(&update) {
        cache.refresh_shader_registrations(ws)?;
    }
    let reemit = t_reemit.elapsed();

    let t_asm = Instant::now();
    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let (mut emit, emitted, unit_reads) = assemble(
        ws,
        cache,
        &mut built_specs,
        CacheLifetime::Retained,
        None,
        None,
    )?;
    if concrete_stats_requested() {
        emit.concrete_stats = cache.concrete.snapshot_build_stats();
    }
    cache.emitted = emitted;
    cache.code_artifact = emit.code_artifact.clone();
    cache.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
    cache.global_layout = Some(emit.global_layout.clone());
    cache.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
    cache.artifact_fingerprint = emit.artifact_fingerprint.clone();
    cache.fingerprint_code_rows = Some(
        crate::artifact_fingerprint::CodeRows::collect(
            ws,
            cache.scx.interner(),
            &emit.code_artifact,
        )
        .map_err(|error| CompilerError::new_no_location(error.to_string()))?,
    );
    cache.unit_comptime_reads = unit_reads;
    cache.absorb_spec_chunks(built_specs);
    cache.refresh_effect_snapshot(ws);
    emit.compile_counts.chunks = cache.chunks.len();
    let reassemble = t_asm.elapsed();
    emit.phases.push(("incr: re-emit affected bodies", reemit));
    emit.phases
        .push(("incr: reassemble full output", reassemble));
    // A codegen rejection is recorded on the shared `CodegenCtx`, so it must
    // be drained here or it would surface at a later, unrelated emit.
    if let Some(err) = cache.cg.take_rejection() {
        return Err(err);
    }
    Ok(emit)
}
