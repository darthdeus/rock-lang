use super::*;

type EffectSnapshot = FxHashMap<FuncRef, bone_core::effects::MemEffect>;

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

/// Per-body emit cache for incremental codegen, keyed by [`Instance`].
pub struct SkullEmitCache {
    pub(super) chunks: FxHashMap<Instance, Result<Chunk, CompilerError>>,
    pub(super) value_ctors: crate::ValueCtors,
    pub(super) attribution: bool,
    pub(super) concrete: crate::concrete_body::ConcreteBodies,
    pub(super) graph: crate::instance_graph::InstanceGraph,
    pub(super) lua_view: crate::artifact::ArtifactView,
    // Its overload set derives from signatures, so it holds across body edits;
    // an interface edit takes `emit_full`, which builds a fresh cache.
    pub(super) cg: CodegenCtx,
    // Updated per body edit. A name whose taken-by-reference membership flips
    // re-emits that function and its callers, whose scalar ABI changed.
    pub(super) scx: skull_mir::lower::ScalarCtx,
    pub(super) inline_cache: skull_transform::InlineCache,
    // Codegen reverse dependencies: a chunk's text embeds or specializes another
    // declaration's body. Not derivable from the graph's call-edge `preds`.
    pub(super) dependents: FxHashMap<FuncRef, FxHashSet<Instance>>,
    pub(super) roots: RootSet,
    pub(super) shader_registrations: ShaderRegistrations,
    pub(super) shader_view: crate::shader_artifact::ShaderArtifactView,
    pub(super) renderer_target: RendererTarget,
    pub(super) publish_fingerprint: bool,
    pub(super) global_accessors: bool,
    pub(super) build_kind: BuildKind,
    pub(super) selected_sim_functions: Option<FxHashSet<Ustr>>,
    pub(super) edit: BodyEditState,
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

pub(super) struct RootSet {
    pub(super) interface_roots: Vec<FuncRef>,
    pub(super) sim_roots: Vec<FuncRef>,
    pub(super) shader_roots: Vec<FuncRef>,
    pub(super) emission_roots: Vec<FuncRef>,
}

#[derive(Clone, Default)]
pub(super) struct BodyEditState {
    pub(super) changed_since_full: FxHashSet<Instance>,
    // The overlay re-runs these inits unguarded. Mutable globals never appear:
    // their guard keeps the live value across a reload.
    pub(super) changed_consts_since_full: FxHashSet<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)>,
    // Removed cells stay until the next full build, so a later overlay can retry
    // a drop after a failed or skipped load.
    pub(super) static_cells_since_full: FxHashMap<Ustr, StaticBaseline>,
    // Bounds elimination depends on transitive callee effects, residual calls included.
    effect_snapshot: EffectSnapshot,
    // Keyed by emitted body, since a specialization is its own definition.
    emitted: FxHashSet<Instance>,
    pub(super) code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    lifecycle_plan: Option<std::sync::Arc<skull_mir::lifecycle::LifecyclePlan>>,
    pub(super) global_layout: Option<String>,
    // Kept by body overlays: definition structure is a full-reload boundary.
    pub(super) definition_plan: Option<std::sync::Arc<skull_mir::definition::DefinitionPlan>>,
    artifact_fingerprint: Option<String>,
    pub(super) fingerprint_code_rows: Option<crate::artifact_fingerprint::CodeRows>,
    pub(super) unit_comptime_reads: UnitComptimeReads,
    pub(super) repair_stats: IncrementalRepairStats,
}

impl BodyEditState {
    pub(super) fn record_artifact(&mut self, emit: &MirEmit) {
        self.code_artifact = emit.code_artifact.clone();
        self.lifecycle_plan = Some(std::sync::Arc::new(emit.lifecycle_plan.clone()));
        self.artifact_fingerprint = emit.artifact_fingerprint.clone();
    }
}

pub(super) struct BodyEditRollback {
    reinferred: Vec<(ModuleId, BodyOwner)>,
    edits: Vec<crate::instance_graph::TextEditDelta>,
    baseline: BodyEditState,
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

    pub(super) fn retain_assembly(
        &mut self,
        ws: &Workspace,
        emit: &MirEmit,
        emitted: FxHashSet<Instance>,
        unit_reads: UnitComptimeReads,
        built_specs: Vec<(Instance, Chunk)>,
    ) -> Result<usize, CompilerError> {
        self.edit.emitted = emitted;
        self.edit.record_artifact(emit);
        self.edit.global_layout = Some(emit.global_layout.clone());
        self.edit.definition_plan = Some(std::sync::Arc::new(emit.definition_plan.clone()));
        self.edit.fingerprint_code_rows = Some(
            crate::artifact_fingerprint::CodeRows::collect(
                ws,
                self.scx.interner(),
                &emit.code_artifact,
            )
            .map_err(|error| CompilerError::new_no_location(error.to_string()))?,
        );
        self.edit.unit_comptime_reads = unit_reads;
        let dependency_edges = built_specs
            .into_iter()
            .map(|(body, chunk)| self.insert_chunk(body, Ok(chunk)))
            .sum();
        self.refresh_effect_snapshot(ws);
        Ok(dependency_edges)
    }

    fn refresh_effect_snapshot(&mut self, ws: &Workspace) {
        self.edit.effect_snapshot = current_effect_snapshot(
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
            baseline: self.edit.clone(),
        });
    }

    pub fn commit_body_edit_transaction(&mut self) {
        self.body_edit_rollback = None;
    }

    pub fn rollback_body_edit(&mut self, ws: &Workspace) -> Result<(), CompilerError> {
        let Some(BodyEditRollback {
            reinferred,
            edits,
            baseline,
        }) = self.body_edit_rollback.take()
        else {
            return Ok(());
        };
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
        self.edit = baseline;
        result
    }

    pub(super) fn candidate_index_by_func(&self) -> &FxHashMap<FuncRef, FxHashSet<Instance>> {
        self.concrete.candidate_keys_by_func()
    }

    /// Every `embed_string` input baked into the program, with the hash of
    /// Covers the always-emitted units too, which hold most production embeds.
    /// the baked bytes: the files a watcher must treat as sources.
    pub fn comptime_file_hashes(&self) -> Vec<(Ustr, u64)> {
        let mut out: FxHashMap<Ustr, u64> = FxHashMap::default();
        let chunk_reads = self
            .chunks
            .values()
            .filter_map(|chunk| chunk.as_ref().ok())
            .flat_map(|chunk| chunk.comptime_reads.iter());
        let unit_reads = self
            .edit
            .unit_comptime_reads
            .globals
            .values()
            .flatten()
            .chain(self.edit.unit_comptime_reads.decorators.iter());
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
        self.edit.global_layout.as_deref()
    }

    pub fn lifecycle_plan(&self) -> Option<&skull_mir::lifecycle::LifecyclePlan> {
        self.edit.lifecycle_plan.as_deref()
    }

    pub fn body_edit_baseline_lifecycle_plan(
        &self,
    ) -> Option<&skull_mir::lifecycle::LifecyclePlan> {
        self.body_edit_rollback
            .as_ref()
            .and_then(|snapshot| snapshot.baseline.lifecycle_plan.as_deref())
    }

    pub fn artifact_fingerprint(&self) -> Option<&str> {
        self.edit.artifact_fingerprint.as_deref()
    }

    /// A live shader registry cannot remove a key from the running process,
    /// so a key-set change must take the process-restarting reload path.
    pub fn shader_registration_keys_changed(&self, ws: &Workspace) -> Result<bool, CompilerError> {
        let pairs = skull_mir::lower::collect_shader_pairs(ws)?;
        Ok(!self
            .shader_registrations
            .keys_match(pairs.iter().map(|pair| pair.key.as_str())))
    }

    /// Check the registration keys over only the re-inferred bodies.
    pub fn shader_registration_keys_changed_for_bodies(
        &self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
    ) -> Result<bool, CompilerError> {
        let mut changed = FxHashSet::default();
        let mut new_pairs = Vec::new();
        for &(mid, owner) in reinferred {
            let (pairs, _) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
            if !pairs.is_empty()
                || self
                    .shader_registrations
                    .shader_contributions
                    .contains_key(&(mid, owner))
            {
                changed.insert((mid, owner));
                new_pairs.extend(pairs);
            }
        }
        if changed.is_empty() {
            return Ok(false);
        }
        let keys: BTreeSet<&str> = self
            .shader_registrations
            .shader_contributions
            .iter()
            .filter(|(key, _)| !changed.contains(key))
            .flat_map(|(_, pairs)| pairs)
            .chain(&new_pairs)
            .map(|pair| pair.key.as_str())
            .collect();
        Ok(!self.shader_registrations.keys_match(keys.into_iter()))
    }

    pub(super) fn update_shader_registrations(
        &mut self,
        ws: &Workspace,
        reinferred: &[(ModuleId, BodyOwner)],
    ) -> Result<bool, CompilerError> {
        let affected = reinferred.iter().any(|&(mid, owner)| {
            self.shader_registrations
                .shader_contributions
                .contains_key(&(mid, owner))
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
        let registrations = &mut self.shader_registrations;
        for &(mid, owner) in reinferred {
            let (pairs, _) = skull_mir::lower::collect_shader_body(ws, mid, owner)?;
            if pairs.is_empty() {
                registrations.shader_contributions.remove(&(mid, owner));
            } else {
                registrations
                    .shader_contributions
                    .insert((mid, owner), pairs);
            }
        }
        registrations.roots = registrations
            .shader_contributions
            .values()
            .flatten()
            .flat_map(|pair| pair.vert.into_iter().chain(std::iter::once(pair.frag)))
            .collect();
        registrations
            .roots
            .sort_unstable_by_key(|f| (f.module.index(), f.func.into_raw()));
        registrations.roots.dedup();
        Ok(true)
    }

    pub(super) fn refresh_shader_registrations(
        &mut self,
        ws: &Workspace,
    ) -> Result<(), CompilerError> {
        let collection = self.shader_registrations.collection();
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
                    self.edit.static_cells_since_full.entry(sc.cell)
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

    fn rebuild_chunk(&mut self, ws: &Workspace, body: &Instance) -> Result<Chunk, CompilerError> {
        let Self {
            cg,
            scx,
            concrete,
            inline_cache,
            ..
        } = self;
        let name = (!body.args.is_empty()).then(|| body.symbol(ws, scx.interner()));
        emit_body_chunk(ws, cg, scx, concrete, body, name.as_deref(), inline_cache)
    }

    fn insert_chunk(&mut self, body: Instance, chunk: Result<Chunk, CompilerError>) -> usize {
        let dependency_edges = chunk.as_ref().map_or(0, |chunk| {
            chunk.index_dependents(&body, &mut self.dependents)
        });
        self.chunks.insert(body, chunk);
        dependency_edges
    }

    fn unindex_chunk(&mut self, body: &Instance) {
        if let Some(Ok(chunk)) = self.chunks.get(body) {
            for callee in &chunk.deps {
                if let Some(callers) = self.dependents.get_mut(callee) {
                    callers.remove(body);
                }
            }
        }
    }

    /// A stale chunk would be reassembled by `emit_incremental` and offered to
    /// every later overlay, though no call reaches it.
    fn forget_instance(&mut self, gone: &Instance) {
        self.unindex_chunk(gone);
        self.chunks.remove(gone);
        self.edit.changed_since_full.remove(gone);
        self.edit.emitted.remove(gone);
    }

    fn build_graph_added_chunks(
        &mut self,
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
            if let Some(Ok(chunk)) = self.chunks.get(&body) {
                if !self.edit.emitted.contains(&body) {
                    self.edit.changed_since_full.insert(body.clone());
                    fingerprint_identities.extend(chunk.code_identities());
                }
                continue;
            }
            let chunk = match self.rebuild_chunk(ws, &body) {
                Ok(chunk) => chunk,
                Err(err) => {
                    errs.push(err);
                    continue;
                }
            };
            let mut valid = true;
            for demand in &chunk.specs {
                if self.graph.concreteness(&demand.inst)
                    == crate::instance_graph::Concreteness::Missing
                {
                    errs.push(CompilerError::new_with_label(
                        f.name_span,
                        format!(
                            "graph omitted added specialization `{}` demanded by `{}`",
                            demand.inst.symbol(ws, self.scx.interner()),
                            body.symbol(ws, self.scx.interner())
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
            fingerprint_identities.extend(chunk.code_identities());
            self.insert_chunk(body.clone(), Ok(chunk));
            self.edit.changed_since_full.insert(body);
        }
        (errs, fingerprint_identities)
    }

    pub(super) fn combine_errors(errors: Vec<CompilerError>) -> Option<CompilerError> {
        let mut errors = errors.into_iter();
        Some(errors.next()?.with_siblings(errors))
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
            // A constant emitted without a global because nothing read it needs one
            // again once an edited inline Lua body spells it.
            for name in cache.cg.refresh_unread_constants(ws) {
                cache
                    .edit
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
        cache.edit.repair_stats = IncrementalRepairStats::default();
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
            let mut current_roots = cache.roots.emission_roots.clone();
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
            cache.roots.emission_roots = current_roots;
        }
        cache.roots.shader_roots = current_shader_roots;
        // The view's update rows must describe the graph before this update.
        cache.lua_view.prepare_update(&cache.graph);
        let update = cache.graph.update_bodies_with_root_delta(
            ws,
            &root_added,
            &root_removed,
            reinferred,
            edits,
        );
        cache.edit.repair_stats.graph_update_ns = t_graph.elapsed().as_nanos() as u64;
        let t_concrete = Instant::now();
        let concrete_update = cache.concrete.update_local(
            ws,
            &cache.graph,
            &update.revisited,
            &update.removed,
            &update.revisited_owners,
        );
        cache.edit.repair_stats.concrete_update_ns = t_concrete.elapsed().as_nanos() as u64;

        // Only the graph knows an instance was stranded; its chunk must go, or
        // reassembly re-emits a definition no call reaches.
        for gone in &update.removed {
            cache.forget_instance(gone);
        }

        let t_artifact = Instant::now();
        cache.lua_view.update_local_with_root_delta(
            ws,
            &cache.graph,
            &crate::artifact::ArtifactRequest::new(
                crate::target_projection::LUAJIT_DESKTOP,
                cache.roots.emission_roots.as_slice(),
            )
            .with_scalar_context(&cache.scx),
            &update.revisited,
            &update.added,
            &update.removed,
            &root_added,
            &root_removed,
        );
        cache.edit.repair_stats.artifact_update_ns = t_artifact.elapsed().as_nanos() as u64;

        let t_affected = Instant::now();
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
        // A flipped scalar ABI re-emits a function even if its body is unchanged.
        changed.extend(flipped_frefs.iter().copied());

        // A changed callee memory effect reruns every caller's bounds elimination,
        // even through a residual call whose inline eligibility is unchanged.
        let effect_changed: FxHashSet<FuncRef> =
            if cache.edit.effect_snapshot.is_empty() && ws.vec_effects.get().is_none() {
                FxHashSet::default()
            } else {
                let effects = ws.effects();
                cache
                    .edit
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
                // Bone hashes only source text, so a stale baked comptime input is a body
                // edit only this check can see.
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
        let candidate_table = update_inline_candidate_table(
            ws,
            &cache.scx,
            &cache.concrete,
            previous_candidate_table.as_ref(),
            warm_keys,
            &concrete_update.candidate_keys_removed,
        )?;
        // A residual call embeds no callee body, so its caller keeps the chunk when
        // every candidate's decision inputs are unchanged.
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
        cache.edit.repair_stats.candidate_update_ns = t_candidate.elapsed().as_nanos() as u64;
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
                // A changed effect invalidates every dependent, whatever its inline decision.
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
        cache.edit.repair_stats.affected_selection_ns = t_affected.elapsed().as_nanos() as u64;

        let t_relow = Instant::now();
        for body in &affected {
            // `affected` holds every lowered body that can have changed.
            cache.inline_cache.invalidate(body.func);
        }
        for body in &affected {
            // Re-emission can change the deps, or drop the chunk entirely.
            cache.unindex_chunk(body);
            let fref = body.func;
            let f = &ws.module(fref.module).functions[fref.func];
            if is_cached_emitted_body(f, body) {
                let chunk = cache.rebuild_chunk(ws, body);
                if let Ok(chunk) = &chunk {
                    fingerprint_identities.extend(chunk.code_identities());
                }
                cache.insert_chunk(body.clone(), chunk);
            } else {
                cache.chunks.remove(body);
            }
        }
        // After invalidation, so a newly reached body cannot reuse an inline decision
        // made for the old body.
        let (added_errors, added_fingerprint_identities) =
            cache.build_graph_added_chunks(ws, &update.added);
        fingerprint_identities.extend(added_fingerprint_identities);
        cache.refresh_effect_snapshot(ws);
        cache.edit.repair_stats.relowering_ns = t_relow.elapsed().as_nanos() as u64;
        Ok((affected, update, added_errors, fingerprint_identities))
    }
}

pub fn emit_incremental(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    let t_reemit = Instant::now();
    // Shader registrations first: they are graph roots, so the cache update must
    // see this edit's set.
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
    cache.retain_assembly(ws, &emit, emitted, unit_reads, built_specs)?;
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
