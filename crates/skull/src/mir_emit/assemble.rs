//! Assembly of the complete program text and its artifact identity.

use super::*;

/// Comptime reads made by ALWAYS-EMITTED units, which have no Chunk to carry
/// them: per-global (keyed so the overlay can re-init exactly the stale one)
/// plus decorator definitions (files only -- decorators re-apply on full
/// rebuild, so the watch set is all they need).
#[derive(Clone, Default)]
pub(crate) struct UnitComptimeReads {
    pub(super) globals: FxHashMap<
        (ModuleId, Idx<bone_semantic::hir::GlobalDef>),
        Vec<skull_mir::mir::ComptimeRead>,
    >,
    pub(super) decorators: Vec<skull_mir::mir::ComptimeRead>,
}

pub(super) fn renderer_fingerprint_tag(target: RendererTarget) -> &'static str {
    match target {
        RendererTarget::Gl => "gl",
    }
}

pub(super) fn artifact_comptime_reads(
    cache: &SkullEmitCache,
    built_specs: &[(Instance, Chunk)],
    unit_reads: &UnitComptimeReads,
    replace_all_units: bool,
) -> Vec<skull_mir::mir::ComptimeRead> {
    let mut reads = Vec::new();
    for chunk in cache.chunks.values().flatten() {
        reads.extend(chunk.comptime_reads.iter().cloned());
    }
    for (_, chunk) in built_specs {
        reads.extend(chunk.comptime_reads.iter().cloned());
    }
    if replace_all_units {
        reads.extend(unit_reads.globals.values().flatten().cloned());
        reads.extend(unit_reads.decorators.iter().cloned());
    } else {
        for (key, old_reads) in &cache.unit_comptime_reads.globals {
            if !unit_reads.globals.contains_key(key) {
                reads.extend(old_reads.iter().cloned());
            }
        }
        reads.extend(unit_reads.globals.values().flatten().cloned());
        if unit_reads.decorators.is_empty() {
            reads.extend(cache.unit_comptime_reads.decorators.iter().cloned());
        } else {
            reads.extend(unit_reads.decorators.iter().cloned());
        }
    }
    reads
}

pub(super) fn artifact_fingerprint(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    artifact: &skull_mir::code_identity::CodeArtifact,
    lifecycle: &skull_mir::lifecycle::LifecyclePlan,
    definitions: &skull_mir::definition::DefinitionPlan,
    global_layout: &str,
    reads: &[skull_mir::mir::ComptimeRead],
    renderer_target: RendererTarget,
) -> Result<String, CompilerError> {
    crate::artifact_fingerprint::compute(
        ws,
        scx.interner(),
        artifact,
        lifecycle,
        definitions,
        global_layout,
        reads,
        &ws.comptime_inputs.volatile_vars,
        renderer_fingerprint_tag(renderer_target),
    )
    .map_err(|error| {
        CompilerError::new_no_location(format!("Artifact fingerprint generation failed: {error}"))
    })
}

/// What one assembly accumulates while it pulls every emitted definition into
/// the program text, in the order `assemble` visits them.
struct Assembly {
    out: String,
    // Per-stage lower/cleanup/emit/regalloc times: chunk times (captured during the
    // parallel `emit_full`/`emit_incremental` chunk build) summed in as chunks are
    // pulled, plus the decorator/global units re-emitted here. Under parallel emit
    // these are summed per-unit elapsed time across workers (so they exceed the wall-clock emit
    // total `backend-stats` measures). `t_scaffold` is the directly-timed
    // SEQUENTIAL remainder (prelude/value-structs/type-tables + global-init
    // ordering + deco application).
    times: PhaseTimes,
    t_scaffold: Duration,
    attribution: AttributionAccumulator,
    needed_specs: Vec<skull_mir::bound::SpecDemand>,
    rejections: Rejections,
    direct_globals: FxHashMap<Ustr, Vec<Ustr>>,
    direct_calls: FxHashMap<Ustr, Vec<Ustr>>,
    all_statics: Vec<skull_mir::mir::StaticCell>,
    code_artifact: skull_mir::code_identity::CodeArtifact,
    allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    always_emitted_calls: Vec<FuncRef>,
    always_emitted_names: Vec<Ustr>,
    unit_reads: UnitComptimeReads,
    definitions: LuaDefinitionBuffer,
    // Second pass of the callee binding: every factory's
    // `<name> = __rock_mk_<name>(<callees>)` line, emitted after EVERY
    // definition and in ascending call-graph component rank, so each captured
    // callee is already its final closure.
    callee_binds: Vec<skull_codegen_lua::CalleeBindingPlan>,
    emits_main: bool,
}

impl Assembly {
    fn new(cache: &SkullEmitCache) -> Self {
        Self {
            out: String::new(),
            times: PhaseTimes::default(),
            t_scaffold: Duration::ZERO,
            attribution: candidate_setup_attribution(cache),
            needed_specs: Vec::new(),
            rejections: Rejections::default(),
            direct_globals: FxHashMap::default(),
            direct_calls: FxHashMap::default(),
            all_statics: Vec::new(),
            code_artifact: skull_mir::code_identity::CodeArtifact::default(),
            allocation_bodies: Vec::new(),
            always_emitted_calls: Vec::new(),
            always_emitted_names: Vec::new(),
            unit_reads: UnitComptimeReads::default(),
            definitions: LuaDefinitionBuffer::new(),
            callee_binds: Vec::new(),
            emits_main: false,
        }
    }

    /// Fold parallel `UnitOut`s back into the chunk IN ORDER: append each unit's Lua,
    /// collect call-graph facts / pending inits, sum phase times.
    fn merge_units(
        &mut self,
        results: Vec<UnitOut>,
        out: &mut String,
        pending_inits: &mut Vec<(Ustr, Ustr, String)>,
    ) {
        for u in results {
            out.push_str(&u.lua);
            if let Some((k, gs, cs)) = u.refs {
                self.direct_globals.insert(k, gs);
                self.direct_calls.insert(k, cs);
            }
            if let Some(pi) = u.pending_init {
                pending_inits.push(pi);
            }
            self.always_emitted_calls.extend(u.calls);
            self.always_emitted_names.extend(u.global_names);
            self.code_artifact.merge(u.code_artifact);
            self.allocation_bodies.extend(u.allocation_bodies);
            self.attribution.add_unit(&u.attribution);
            self.times.add(&u.times);
        }
    }

    /// Decorator DEFINITIONS, workspace-wide (lowered by
    /// [`prepare_early_assembly`]): each `deco <name> { .. }` -> a fn
    /// `__deco_<name>(target, <params>)`. Unlike JS, Lua emits every
    /// program-defined decorator, applied or not.
    fn push_decorator_definitions(&mut self, deco_results: Vec<UnitOut>) {
        self.rejections.extend(unit_errors(&deco_results));
        self.needed_specs
            .extend(deco_results.iter().flat_map(|u| u.specs.iter().cloned()));
        for u in &deco_results {
            self.unit_reads
                .decorators
                .extend(u.comptime_reads.iter().cloned());
        }
        let mut decorator_lua = String::new();
        self.merge_units(deco_results, &mut decorator_lua, &mut Vec::new());
        let mut decorator_definitions = LuaDefinitionBuffer::new();
        decorator_definitions.push_str(&decorator_lua);
        decorator_definitions.append_to(&mut self.out);
    }

    /// Global initializers are emitted later, in their original slot, but they
    /// are LOWERED early (by [`prepare_early_assembly`]): their call edges are
    /// reachability roots (a global init is always emitted), so the walk needs
    /// them before the fn-def loop runs. Folds the lowered units in and returns
    /// the initializer definitions and their pending init lines.
    fn fold_global_units(
        &mut self,
        global_keys: &[GlobalKey],
        global_results: Vec<UnitOut>,
    ) -> (String, Vec<(Ustr, Ustr, String)>) {
        let mut pending_inits: Vec<(Ustr, Ustr, String)> = Vec::new();
        // par_map preserves input order, so keys and results zip positionally --
        // the same pairing merge_units relies on for output order.
        for (key, u) in global_keys.iter().zip(global_results.iter()) {
            if !u.comptime_reads.is_empty() {
                self.unit_reads
                    .globals
                    .insert(*key, u.comptime_reads.clone());
            }
        }
        self.rejections.extend(unit_errors(&global_results));
        self.needed_specs
            .extend(global_results.iter().flat_map(|u| u.specs.iter().cloned()));
        let mut globals_lua = String::new();
        self.merge_units(global_results, &mut globals_lua, &mut pending_inits);
        (globals_lua, pending_inits)
    }

    /// Function definitions, every module (cached chunk for ordinary/coroutine,
    /// regenerated for extern / inline(lua)). The cached chunks were built in
    /// parallel by `emit_full`/`emit_incremental`; here we pull their Lua and sum
    /// their captured phase times back into the breakdown.
    fn push_function_definitions(
        &mut self,
        ws: &Workspace,
        cache: &SkullEmitCache,
        needed_foreign: &FxHashSet<Instance>,
    ) -> Result<(), CompilerError> {
        for module in ws.source_modules() {
            let mid = module.id;
            for (fid, f) in module.functions.iter() {
                let self_ref = FuncRef {
                    module: mid,
                    func: fid,
                };
                // A JS-backend decl (`inline(js)` / `extern(js)`) is inert on luajit:
                // its lua sibling (if any) carries the binding, and a js-only call from
                // lua-reachable code is rejected at call lowering (resolve_funcref).
                if f.backend == bone_semantic::hir::FnBackend::Js {
                    continue;
                }
                // Whole-program DCE gates both chunk construction and definition
                // emission. Retained bodies still fail hard when their plan or chunk
                // is missing; pruned dependency bodies have neither by design.
                let emit_def = should_emit_via_view(ws, &cache.lua_view, self_ref);
                if f.is_extern {
                    // An `extern(ffi, "lib")` fn needs a thin wrapper binding it to the
                    // loaded library handle. A non-ffi extern is a pre-existing global.
                    //
                    // Gated by the VIEW like every other definition. `is_emission_root`
                    // roots every `extern`, so in practice the view retains them all
                    // and this changes no output -- but an ungated write is a second
                    // population input, and one is the whole point of the cut.
                    if let Some(lib) = f.ffi_lib {
                        let (nm, params, body) = ffi_extern_wrapper(ws, f, lib)?;
                        // The rejection above runs for EVERY extern; only the WRITE
                        // is gated. Pruning may change which definitions are emitted
                        // and nothing else -- gating the whole arm suppressed
                        // `extern_ret`'s unbound-generic rejection, whose subject is
                        // a generic declaration the view holds no base instance for.
                        if emit_def {
                            let prologue = default_fill_prologue(ws, mid, fid, f);
                            self.definitions.push_str(&format!(
                                "function {nm}({params})\n{prologue}{body}\nend\n"
                            ));
                        }
                    }
                    continue;
                }
                if let Some((_, body)) = f.inline_body {
                    // Retention alone (`emit_def`) over-emits here -- see
                    // `needed_foreign`'s doc. Three independent reasons still force
                    // emission regardless of splicing eligibility: a publication
                    // obligation (`@nomangle`), a residual (non-spliced) demand, or
                    // being a ROOT in its own right (an own-project inline(lua) fn
                    // is unconditionally rooted by `is_emission_root` even with no
                    // caller at all -- same reasoning as `main`/`@sim`/`@test`;
                    // found empirically, `anvil`'s `sv_close`/`sv_is_eof` etc. have
                    // no call site anywhere in the corpus and are rooted purely by
                    // living in the project's own file).
                    if !emits_foreign_definition(ws, &cache.lua_view, needed_foreign, self_ref) {
                        continue;
                    }
                    self.definitions.push_str(&inline_lua_definition(
                        ws,
                        &cache.scx,
                        self_ref,
                        f,
                        body.as_str(),
                    ));
                    continue;
                }
                // An `Iterator`-bounded fn has no standalone emission (it exists
                // only inlined at its call sites) -- deliberately absent.
                if fn_has_bounds(f) {
                    continue;
                }
                // A generic declaration has no definition of its own. Its bodies are
                // its instances, rendered below by `emit_specialized_callees`.
                if is_generic(f) {
                    continue;
                }
                self.pull_cached_chunk(ws, cache, self_ref, f, emit_def);
            }
        }
        Ok(())
    }

    /// Ordinary / coroutine: pull the cached chunk.
    fn pull_cached_chunk(
        &mut self,
        ws: &Workspace,
        cache: &SkullEmitCache,
        self_ref: FuncRef,
        f: &bone_semantic::hir::Function,
        emit_def: bool,
    ) {
        match cache.chunks.get(&shared(self_ref)) {
            Some(Ok(chunk)) => {
                let key = Ustr::from(&mangle_fn(ws, f));
                self.direct_globals
                    .insert(key, chunk.direct_globals.clone());
                self.direct_calls.insert(key, chunk.direct_calls.clone());
                self.times.add(&chunk.times);
                self.attribution.add_unit(&chunk.attribution);
                if emit_def {
                    self.emits_main |= key.as_str() == "main";
                    self.definitions.push_chunk(chunk);
                    self.callee_binds.extend(chunk.binds.iter().cloned());
                    self.allocation_bodies
                        .extend(chunk.allocation_bodies.iter().cloned());
                    self.code_artifact.merge_ref(&chunk.code_artifact);
                    // A dropped definition's `static` cells do not exist, so
                    // they must not be registered for hot-reload preservation.
                    self.all_statics.extend(chunk.statics.iter().cloned());
                    // Only a body that is actually emitted can call one.
                    self.needed_specs.extend(chunk.specs.iter().cloned());
                }
            }
            Some(Err(_)) if !emit_def => {}
            Some(Err(err)) => self.rejections.push(err.clone()),
            None if !emit_def => {}
            None => self.rejections.push(CompilerError::new_with_label(
                f.name_span,
                format!("internal: `{}` has no emitted chunk", f.name),
            )),
        }
    }

    /// Order the init lines by their transitive global-read closure over the full
    /// call graph (now that every fn's direct facts are recorded), then append the
    /// static resets, the initializer calls and the remaining load-time
    /// registrations.
    fn push_initializers(
        &mut self,
        ws: &Workspace,
        cache: &SkullEmitCache,
        pending_inits: Vec<(Ustr, Ustr, String)>,
        decorators: &DecoratorTable,
        has_code_slot_publication: bool,
        has_definition_plan: bool,
        lifecycle_declarations: bool,
    ) -> Result<(), CompilerError> {
        let scx = &cache.scx;
        let ts = Instant::now();
        let reach =
            skull_mir::reach::transitive_global_reads(&self.direct_globals, &self.direct_calls);
        let leaf_owner = global_leaf_owners(ws, scx)?;
        let inits: Vec<PendingInit<String>> = pending_inits
            .into_iter()
            .map(|(name, init_name, line)| PendingInit {
                name,
                deps: reach.deps_of(init_name),
                item: line,
            })
            .collect();
        let static_resets = static_reset_lines(ws, scx, &self.all_statics)?;
        let out = &mut self.out;
        out.push_str(&static_resets);
        let init_lines = sort_global_init_lines(inits, &leaf_owner)?;
        out.push_str(&state_initializer_lua(&init_lines));

        // Decorator APPLICATIONS (after global inits so a deco body can push a global).
        out.push_str(&decorator_application_lines(ws, decorators, scx)?);
        // `@sim` registrations (after fn + global defs; same init slot as rock).
        if cache.build_kind == BuildKind::Program {
            out.push_str(&sim_registration_lines(
                ws,
                scx,
                cache.selected_sim_functions.as_ref(),
            )?);
        }
        // `rock_shader` pipeline registrations:
        // one top-level `__rock_shader_register(key, glsl)` per distinct pair, so a
        // reload re-`dofile`s them with the new source (hot-reload parity).
        out.push_str(&cache.shader_registrations.lua);
        if has_code_slot_publication {
            out.push_str(code_slot_publish_lua(
                &self.code_artifact,
                has_definition_plan,
                lifecycle_declarations,
            ));
        }
        // Closes the block the runtime prelude opens (the storage cell becomes a
        // closed upvalue here); anything appended after this line runs outside it.
        out.push_str("end\n");
        self.t_scaffold += ts.elapsed();
        Ok(())
    }

    /// `times.emit` brackets emit_function/emit_named/emit_coroutine; `times.regalloc`
    /// is the subset spent in `color_values` inside them (per-unit reset/read, exact
    /// even under parallel emit). stringify = the remainder.
    fn phases(&self) -> Vec<(&'static str, Duration)> {
        let times = &self.times;
        let t_stringify = times.emit - times.regalloc;
        vec![
            ("lower hir->mir", times.lower),
            ("inline plan+splice", times.inline),
            ("cleanup mir", times.cleanup),
            ("closure conversion", times.closure),
            ("counted-loop recovery", times.counted_loop),
            ("hoisted-body inline", times.hoisted_inline),
            ("post-transform validation", times.validation),
            ("emit: regalloc", times.regalloc),
            ("emit: stringify", t_stringify),
            ("scaffold+callgraph+order", self.t_scaffold),
        ]
    }
}

/// The inline-candidate table's setup counters, the starting point of this
/// assembly's attribution.
fn candidate_setup_attribution(cache: &SkullEmitCache) -> AttributionAccumulator {
    let mut attribution = AttributionAccumulator {
        census_enabled: cache.inline_cache.census_enabled(),
        ..AttributionAccumulator::default()
    };
    if let Some(table) = cache.inline_cache.candidate_table() {
        let stats = table.stats();
        let inline = &mut attribution.result.inline;
        inline.candidate_setup_keys = stats.keys;
        inline.candidate_setup_warmed = stats.warmed;
        inline.candidate_setup_reused = stats.reused;
        inline.candidate_setup_eligible = stats.eligible;
        inline.candidate_setup_rejected = stats.rejected;
        inline.candidate_setup_lower_ns = stats.lower_ns;
        inline.candidate_setup_cleanup_ns = stats.cleanup_ns;
        inline.candidate_setup_eligibility_ns = stats.eligibility_ns;
        inline.candidate_setup_worker_derivations = stats.worker_derivations.clone();
        inline.candidate_setup_rejection_reasons = stats.rejection_reasons.clone();
        if attribution.census_enabled {
            attribution.candidate_published_keys.extend(table.keys());
            let population = &mut inline.population;
            for (key, profile) in &stats.profiles {
                population.candidate_lower_attempted += usize::from(profile.lower_attempted);
                population.candidate_lowered_keys += usize::from(profile.lowered);
                if profile.eligible {
                    attribution.candidate_eligible_keys.insert(key.clone());
                }
                population.candidate_rejected_before_lower +=
                    usize::from(profile.rejected_before_lower);
                population.candidate_full_clone_ns += profile.full_clone_ns;
                population.candidate_compact_ns += profile.compact_ns;
                population.candidate_raw_instructions += profile.raw_instructions;
                population.candidate_raw_cells += profile.raw_cells;
                population.candidate_raw_blocks += profile.raw_blocks;
                population.candidate_cleaned_instructions += profile.cleaned_instructions;
                population.candidate_cleaned_cells += profile.cleaned_cells;
                population.candidate_cleaned_blocks += profile.cleaned_blocks;
                population.candidate_full_clone_capacity.0 += profile.full_clone_inst_capacity;
                population.candidate_full_clone_capacity.1 += profile.full_clone_cell_capacity;
                population.candidate_full_clone_capacity.2 += profile.full_clone_block_capacity;
                population.candidate_compact_capacity.0 += profile.compact_inst_capacity;
                population.candidate_compact_capacity.1 += profile.compact_cell_capacity;
                population.candidate_compact_capacity.2 += profile.compact_block_capacity;
            }
            for (key, lower_ns) in &stats.lower_ns_by_key {
                attribution
                    .candidate_lower_ns_by_key
                    .entry(key.clone())
                    .or_default()
                    .push(*lower_ns);
            }
        }
    }
    attribution
}

/// **Foreign-body (`inline(lua)`/`extern`) DELIVERY, separate from
/// RETENTION.** `lua_view` (and `should_emit_via_view`'s call-serving half)
/// answers "is this declaration's canonical Instance part of the program",
/// which is not the same question as "does anything call it in RESIDUAL
/// (non-spliced) form" -- `@pure @inline` stdlib operators (`+`, `-`, ...)
/// are retained (something calls `+`) but almost always fully spliced away
/// at every call site, so a naive retention-only gate emitted them
/// standalone anyway (found empirically: `__op_add_int_int` and
/// siblings newly appeared in every project's output the moment retention
/// alone drove the inline-body gate). Splicing eligibility is a LOWERING
/// decision the graph does not model (same class of fact as a generic's
/// standalone-vs-spliced `SpecDemand`, just for a NON-generic body), so
/// this is collected from what already-lowered chunks record they still
/// call after inlining (`Chunk::calls`), exactly as `chunk.specs` seeds
/// `view_driven_specialized_chunks`.
fn collect_needed_foreign(
    ws: &Workspace,
    cache: &SkullEmitCache,
    always_emitted_calls: &[FuncRef],
    spec_chunks: &FxHashMap<Instance, Chunk>,
) -> FxHashSet<Instance> {
    let mut needed_foreign: FxHashSet<Instance> = FxHashSet::default();
    needed_foreign.extend(always_emitted_calls.iter().map(|f| shared(*f)));
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
                needed_foreign.extend(c.calls.iter().map(|f| shared(*f)));
            }
        }
    }
    for c in spec_chunks.values() {
        needed_foreign.extend(c.calls.iter().map(|f| shared(*f)));
    }
    needed_foreign
}

/// The name, parameter list and body of the thin wrapper binding an
/// `extern(ffi, "lib")` fn to its loaded library handle.
fn ffi_extern_wrapper(
    ws: &Workspace,
    f: &bone_semantic::hir::Function,
    lib: Ustr,
) -> Result<(String, String, String), CompilerError> {
    let nm = mangle_fn(ws, f);
    let params = f
        .param_decls
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let body = match extern_ret(ws, f)? {
        // Returns an OWNING copy, never `__out[0]`. LuaJIT does
        // not keep the parent array alive through a reference
        // into it, so handing one back is a use-after-free that
        // reads fine until the memory is reused
        // (`tests/extern_value_return_ownership.rs`).
        ExternRet::ValueStruct(key) => {
            let sname = skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(&key));
            let call_args = if params.is_empty() {
                "__out".to_string()
            } else {
                format!("__out, {params}")
            };
            format!(
                "  local __out = ffi.new(\"{sname}[1]\")\n  {lib}.{nm}({call_args})\n{}",
                skull_codegen_lua::wrapper_return(&format!("ffi.new(\"{sname}\", __out[0])"))
            )
        }
        // `return <call>` is a `CALLT` into C, which a root trace
        // cannot leave through; the bound local makes it a `RET`
        // (`wrapper_return`). `Bool` ends in a compare, not a call.
        ExternRet::Int | ExternRet::Uint => {
            skull_codegen_lua::wrapper_return(&format!("tonumber({lib}.{nm}({params}))"))
        }
        ExternRet::Bool => format!("  return {lib}.{nm}({params}) ~= 0"),
        ExternRet::AsIs => skull_codegen_lua::wrapper_return(&format!("{lib}.{nm}({params})")),
        ExternRet::None => format!("  {lib}.{nm}({params})"),
    };
    Ok((nm, params, body))
}

/// inline(lua): emit the hand-written body verbatim, wrapping a
/// value-returning bodyless-`return` and prepending default fills.
/// (Js inline bodies were skipped by the backend guard.)
fn inline_lua_definition(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    body: &str,
) -> String {
    let name = mangle_fn(ws, f);
    let params =
        match skull_mir::lower::BackendFnAbiPlan::for_boundary_declaration(ws, scx, self_ref)
            .lua_param_names(ws)
        {
            Some(names) => names.join(", "),
            None => f
                .param_decls
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        };
    let body = if f.return_ty.is_some() && !body.contains("return") {
        format!("return {}", body.trim())
    } else {
        body.to_string()
    };
    let prologue = default_fill_prologue(ws, self_ref.module, self_ref.func, f);
    format!("function {name}({params})\n{prologue}{body}\nend\n")
}

/// Init ordering is stated over SOURCE globals, but a `nobox` global is READ
/// through its leaves, so a leaf dep has to resolve back to the group that
/// owns it or the dependent initializer sorts before it. Built from the
/// globals that actually mint leaves -- never a name-shape guess.
fn global_leaf_owners(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<FxHashMap<Ustr, Ustr>, CompilerError> {
    let mut leaf_owner: FxHashMap<Ustr, Ustr> = FxHashMap::default();
    for module in ws.modules.iter() {
        for (_, g) in module.globals.iter() {
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            // Leaf names are the emitted symbols; the owner they resolve back
            // to is the global's LOGICAL name, which is what an init line and a
            // plain global read are keyed on.
            if let Some(leaves) = plan.leaf_names(skull_mir::mangle::symbol_of_key(g.name)) {
                for leaf in leaves {
                    leaf_owner.insert(leaf, g.name);
                }
            }
        }
    }
    Ok(leaf_owner)
}

/// `static` cells: on reload, RESET (nil the cell + clear the done-flag) when
/// the init/type key changed since last run, so the next reach re-inits with
/// the edited initializer; an unchanged key preserves the live cell. Dedup by
/// cell.
fn static_reset_lines(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    all_statics: &[skull_mir::mir::StaticCell],
) -> Result<String, CompilerError> {
    let mut seen_static: FxHashSet<Ustr> = FxHashSet::default();
    let mut static_resets = String::new();
    for sc in all_statics {
        if !seen_static.insert(sc.cell) {
            continue;
        }
        let key = static_cell_key(sc);
        let (value_slot, done_slot) =
            skull_mir::global_storage::static_cell_storage_keys(ws, scx.interner(), sc)
                .map_err(CompilerError::new_no_location)?;
        let _ = writeln!(
            static_resets,
            "__rock_static_prepare({value_slot:?}, {done_slot:?}, {key:?})"
        );
    }
    Ok(static_resets)
}

/// Every definition this assembly actually emitted: the source-order bodies
/// the same gates admitted, plus the specialized instances.
fn emitted_bodies(
    ws: &Workspace,
    cache: &SkullEmitCache,
    needed_foreign: &FxHashSet<Instance>,
    specialized_instances: Vec<Instance>,
) -> FxHashSet<Instance> {
    let mut emitted_bodies: FxHashSet<Instance> = FxHashSet::default();
    for module in ws.source_modules() {
        for (fid, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            let self_ref = FuncRef {
                module: module.id,
                func: fid,
            };
            let actually_emitted = if f.is_extern {
                should_emit_via_view(ws, &cache.lua_view, self_ref)
            } else if f.inline_body.is_some() {
                emits_foreign_definition(ws, &cache.lua_view, needed_foreign, self_ref)
            } else {
                should_emit_via_view(ws, &cache.lua_view, self_ref)
            };
            if actually_emitted {
                emitted_bodies.insert(shared(self_ref));
            }
        }
    }
    emitted_bodies.extend(specialized_instances);
    emitted_bodies
}

/// One-shot builds drop the chunk cache once its last reader is done: capture
/// the artifact's comptime reads and the chunk and dependency counts first.
fn release_one_shot_chunks(
    cache: &mut SkullEmitCache,
    built_specs: &mut Vec<(Instance, Chunk)>,
    unit_reads: &UnitComptimeReads,
    compile_counts: &mut CompileCounts,
    release_stats: &mut CompileReleaseStats,
) -> Vec<skull_mir::mir::ComptimeRead> {
    let reads = artifact_comptime_reads(cache, built_specs, unit_reads, true);
    compile_counts.chunks += built_specs
        .iter()
        .filter(|(key, _)| !cache.chunks.contains_key(key))
        .count();
    compile_counts.dependency_edges = Some(
        cache
            .chunks
            .values()
            .flatten()
            .map(|chunk| chunk.deps.len())
            .sum::<usize>()
            + built_specs
                .iter()
                .map(|(_, chunk)| chunk.deps.len())
                .sum::<usize>(),
    );
    let chunks = cache.chunks.len() + built_specs.len();
    drop_in_background(std::mem::take(&mut cache.chunks));
    drop_in_background(std::mem::take(built_specs));
    release_stats.chunks = chunks - cache.chunks.len() - built_specs.len();
    reads
}

type GlobalKey = (ModuleId, Idx<bone_semantic::hir::GlobalDef>);

/// The load-time scaffolding: runtime prelude, value struct and enum
/// definitions, type tables. IR-agnostic, so it needs no chunk.
fn scaffold_prelude_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<String, CompilerError> {
    let mut out = String::from(crate::runtime_prelude());
    if crate::allocation_report::activation_enabled() {
        out.push_str(skull_codegen_lua::ALLOCATION_ACTIVATION_PRELUDE);
    }
    out.push_str(&crate::enum_cdata_defs(ws));
    let rep_arena = scx.rep_arena();
    let (vdefs, value_emitted, value_gc) = crate::value_struct_defs(ws, &rep_arena)?;
    out.push_str(&vdefs);
    out.push_str(&crate::value_struct_gc_defs(ws, &value_gc, &rep_arena)?);
    out.push_str(&crate::value_struct_metatypes(ws, &value_emitted));
    out.push_str(&crate::enum_cdata_metatypes(ws));
    out.push_str(&crate::c_struct_inspect_defs(ws, &value_emitted));
    out.push_str(&crate::type_table_defs(ws, &value_gc, &rep_arena)?);
    out.push_str(&crate::enum_runtime_defs(ws));
    Ok(out)
}

/// The assembly inputs that need no chunk: the scaffold prelude, the lowered
/// decorator and global units, the global layout and the globals directory.
/// A full build prepares them beside its fan-out so the pool, not the
/// assembly tail, pays for them; an incremental build prepares them at the
/// start of assembly.
pub(super) struct EarlyAssembly {
    scaffold: Result<String, CompilerError>,
    scaffold_time: Duration,
    decorators: DecoratorTable,
    decorator_units: Vec<UnitOut>,
    global_keys: Vec<GlobalKey>,
    global_units: Vec<UnitOut>,
    // Kept as results and reported where assembly always reported them, so a
    // body rejection still comes first.
    global_layout: Result<String, String>,
    globals_directory: Result<String, CompilerError>,
}

pub(super) fn prepare_early_assembly(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
) -> EarlyAssembly {
    let (scaffold, units) = parallel_join(
        || {
            let ts = Instant::now();
            let scaffold = scaffold_prelude_lua(ws, scx);
            let scaffold_time = ts.elapsed();
            let global_layout = skull_mir::global_storage::global_layout_id(ws, scx);
            let globals_directory = globals_directory_lua(ws, scx, cg);
            (scaffold, scaffold_time, global_layout, globals_directory)
        },
        || {
            // The decorator table is built up front (sequential, cheap)
            // because an application usually names a decorator declared in
            // another module; the bodies lower+emit in parallel.
            let decorators = DecoratorTable::collect(ws);
            let decorator_units = par_map(decorators.definitions().to_vec(), |(mid, did)| {
                emit_decorator_unit(ws, scx, cg, concrete, mid, did)
            });
            let global_keys: Vec<GlobalKey> = ws
                .source_modules()
                .flat_map(|m| m.globals.iter().map(move |(gid, _)| (m.id, gid)))
                .collect();
            let global_units = par_map(global_keys.clone(), |(mid, gid)| {
                emit_global_unit(ws, scx, cg, concrete, mid, gid)
            });
            (decorators, decorator_units, global_keys, global_units)
        },
    );
    let (scaffold, scaffold_time, global_layout, globals_directory) = scaffold;
    let (decorators, decorator_units, global_keys, global_units) = units;
    EarlyAssembly {
        scaffold,
        scaffold_time,
        decorators,
        decorator_units,
        global_keys,
        global_units,
        global_layout,
        globals_directory,
    }
}

pub(super) fn assemble(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    built_specs: &mut Vec<(Instance, Chunk)>,
    lifetime: CacheLifetime,
    early: Option<EarlyAssembly>,
) -> Result<(MirEmit, FxHashSet<Instance>, UnitComptimeReads), CompilerError> {
    let mut release_stats = CompileReleaseStats {
        retained_dependency_edges: cache.dependents.values().map(|keys| keys.len()).sum(),
        retained_static_keys: cache.static_key_baseline.len(),
        ..CompileReleaseStats::default()
    };
    let mut compile_counts = CompileCounts {
        concrete_plans: cache.concrete.plans.len() + cache.concrete.always_plans.len(),
        chunks: cache.chunks.len(),
        dependency_edges: None,
    };
    // Serial assembly steps, each a wall window; the compile budget lists
    // them under the assembly window so no serial tail hides in "codegen".
    let mut steps: Vec<(&'static str, Duration)> = Vec::new();
    let mut step_started = Instant::now();
    let mut step = |name: &'static str, steps: &mut Vec<(&'static str, Duration)>| {
        let now = Instant::now();
        steps.push((name, now - step_started));
        step_started = now;
    };
    let mut asm = Assembly::new(cache);
    let EarlyAssembly {
        scaffold,
        scaffold_time,
        decorators,
        decorator_units,
        global_keys,
        global_units,
        global_layout,
        globals_directory,
    } = early.unwrap_or_else(|| prepare_early_assembly(ws, &cache.scx, &cache.cg, &cache.concrete));
    asm.out.push_str(&scaffold?);
    asm.t_scaffold += scaffold_time;
    asm.push_decorator_definitions(decorator_units);
    let (globals_lua, pending_inits) = asm.fold_global_units(&global_keys, global_units);
    step("assembly: prelude+globals", &mut steps);

    // **Whole-program population, driven by `cache.lua_view` -- Phase 2.**
    // `should_emit_via_view` decides every ordinary/coroutine/inline
    // definition below; `view_driven_specialized_chunks` decides every
    // specialized one. Corpus-verified before this cutover landed (every real
    // `stick.toml` project, both directions: zero instances the old walk
    // emitted that the view did not retain, and the reverse direction's only
    // difference was instances nothing ever DEMANDS standalone -- see that
    // fn's own doc). `always_emitted_calls` / `always_emitted_names`:
    // still collected above (decorator/global lowering needs its own direct
    // calls for OTHER bookkeeping -- global-read ordering, static baselines --
    // untouched by this cutover), but no longer fed to a root-discovery walk.
    // Generated-glue obligations (`register_sim`, shader dependencies) are part
    // of the ROOT INPUT the graph and the view were built from -- see
    // [`all_roots`] -- so there is no second population source here.
    let (spec_chunks, spec_rejected) =
        view_driven_specialized_chunks(ws, &cache.cg, &cache.scx, cache, &asm.needed_specs);
    if lifetime == CacheLifetime::OneShot {
        let instances = cache.graph.instances.len();
        drop_in_background(cache.graph.take_records());
        release_stats.graph_instances = instances - cache.graph.instances.len();
    }
    step("assembly: specialized chunks", &mut steps);
    let needed_foreign = collect_needed_foreign(ws, cache, &asm.always_emitted_calls, &spec_chunks);
    // Factory lowering is the last concrete-plan reader. Keep its text in the
    // original output slot, but release one-shot plans before copying definitions.
    let early_definition_stage = if lifetime == CacheLifetime::OneShot {
        let plan = skull_mir::definition::collect_complete(ws)?;
        let stage =
            definition_plan_stage_lua(ws, &cache.cg, &cache.scx, &cache.concrete.plans, &plan)?;
        let plans = cache.concrete.plans.len() + cache.concrete.always_plans.len();
        drop_in_background(std::mem::take(&mut cache.concrete));
        release_stats.concrete_plans =
            plans - cache.concrete.plans.len() - cache.concrete.always_plans.len();
        Some((plan, stage))
    } else {
        None
    };

    asm.push_function_definitions(ws, cache, &needed_foreign)?;
    asm.rejections.extend(emit_specialized_callees(
        ws,
        &cache.scx,
        &mut asm.needed_specs,
        &cache.chunks,
        &spec_chunks,
        &spec_rejected,
        built_specs,
        &mut asm.allocation_bodies,
        &mut asm.all_statics,
        &mut asm.definitions,
        &mut asm.callee_binds,
        &mut asm.attribution,
        &mut asm.times,
    ));
    for chunk in spec_chunks.values() {
        asm.code_artifact.merge_ref(&chunk.code_artifact);
    }
    for (_, chunk) in built_specs.iter() {
        asm.code_artifact.merge_ref(&chunk.code_artifact);
    }
    let specialized_instances: Vec<_> = spec_chunks.keys().cloned().collect();
    let one_shot_reads = (lifetime == CacheLifetime::OneShot).then(|| {
        release_one_shot_chunks(
            cache,
            built_specs,
            &asm.unit_reads,
            &mut compile_counts,
            &mut release_stats,
        )
    });
    drop(spec_chunks);
    step("assembly: definitions", &mut steps);

    // Every emitted definition has now been visited, so this is the earliest
    // point where the whole rejection list exists. Anything past here is
    // scaffolding (init ordering, deco application) whose own errors are
    // unrelated to a body that cannot emit.
    std::mem::take(&mut asm.rejections).into_result()?;
    let cg = &cache.cg;
    let scx = &cache.scx;
    if let Some(err) = cg.take_rejection() {
        return Err(err);
    }
    std::mem::replace(&mut asm.definitions, LuaDefinitionBuffer::new()).append_to(&mut asm.out);
    asm.out
        .push_str(&callee_binding_pass_lua(&mut asm.callee_binds));
    let sims = match (cache.build_kind, cache.selected_sim_functions.as_ref()) {
        (BuildKind::Program, Some(selected)) => SimValues::Selected(selected),
        (BuildKind::Program, None) => SimValues::All,
        _ => SimValues::None,
    };
    decorators.mark_generated_code_values(
        ws,
        bone_semantic::hir::FnBackend::Lua,
        sims,
        &mut asm.code_artifact,
    );
    let (definition_plan, early_stage) = match early_definition_stage {
        Some((plan, stage)) => (plan, Some(stage)),
        None => (skull_mir::definition::collect_complete(ws)?, None),
    };
    let lifecycle_plan = skull_mir::lifecycle::collect_with_artifact(ws, &asm.code_artifact)?;
    if lifecycle_plan.census.applications != lifecycle_plan.census.admitted {
        return Err(CompilerError::new_no_location(format!(
            "incomplete LifecyclePlan census: {:?}",
            lifecycle_plan.census
        )));
    }
    let allocation_report = if crate::allocation_report::enabled() {
        let mut report = crate::allocation_report::AllocationReport::from_bodies(std::mem::take(
            &mut asm.allocation_bodies,
        ));
        report.populate_lifecycle_routes(&lifecycle_plan, &asm.code_artifact);
        report
    } else {
        crate::allocation_report::AllocationReport::default()
    };
    let code_artifact = &asm.code_artifact;
    let publication = code_artifact.publication(
        !definition_plan.catalogs.is_empty(),
        lifecycle_plan.census.declarations > 0,
    );
    let has_code_slot_publication = publication.publish;
    let global_layout = global_layout.map_err(CompilerError::new_no_location)?;
    let static_count = asm
        .all_statics
        .iter()
        .map(|cell| cell.cell)
        .collect::<FxHashSet<_>>()
        .len();
    let out = &mut asm.out;
    out.push_str(&globals_open_lua(&global_layout, cg, static_count));
    out.push_str(&wrap_large_lua_section(
        "__rock_globals_directory_part",
        &globals_directory?,
    ));
    // A host attaches its own directory fields right after this line.
    out.push_str("__rock_globals_install_bridge()\n");

    // Globals: an init fn + a dependency-ordered `<NAME> = __skull_ginit_<NAME>()`.
    // (Lowered above, so the walk could see their call edges; appended here, in
    // the original slot, so the emitted text is unchanged.)
    let mut global_definitions = LuaDefinitionBuffer::new();
    global_definitions.push_str(&globals_lua);
    global_definitions.append_to(out);
    let has_definition_plan = !definition_plan.catalogs.is_empty();
    if has_code_slot_publication {
        out.push_str(skull_codegen_lua::FN_MT_PRELUDE);
    }
    // Global initializer bodies (including their escaping lambdas) are now
    // defined, so slot staging must follow them but precede initializer calls.
    let code_slot_stage = code_slot_registration_lua(ws, scx, code_artifact)?;
    out.push_str(&wrap_large_lua_section(
        "__rock_code_slot_stage_part",
        &code_slot_stage,
    ));
    if publication.explicit_begin {
        out.push_str("__rock_code_slot_begin()\n");
    }
    let definition_plan_stage = match early_stage {
        Some(stage) => stage,
        None => definition_plan_stage_lua(ws, cg, scx, &cache.concrete.plans, &definition_plan)?,
    };
    out.push_str(&wrap_large_lua_section(
        "__rock_definition_plan_stage_part",
        &definition_plan_stage,
    ));
    let lifecycle_plan_stage = lifecycle_plan_stage_lua(ws, scx, &lifecycle_plan);
    out.push_str(&wrap_large_lua_section(
        "__rock_lifecycle_plan_stage_part",
        &lifecycle_plan_stage,
    ));
    step("assembly: staging", &mut steps);
    asm.push_initializers(
        ws,
        cache,
        pending_inits,
        &decorators,
        has_code_slot_publication,
        has_definition_plan,
        lifecycle_plan.census.declarations > 0,
    )?;
    step("assembly: initializers", &mut steps);
    let mut phases = asm.phases();

    // A synthetic top-level name (`<fn>__lambda<k>`, `<co>__step`/`__on_*`/...)
    // that collides with a user fn/global was recorded during chunk emit (above,
    // and in `emit_incremental`'s re-emit). Reject with a spanned diagnostic
    // rather than emit two clobbering top-level defs. Covers `emit_full` +
    // `emit_incremental` (both funnel through here); the overlay fast path drains
    // in `emit_overlay` and propagates the same error.
    let emitted_bodies = emitted_bodies(ws, cache, &needed_foreign, specialized_instances);
    let Assembly {
        mut out,
        attribution,
        code_artifact,
        unit_reads,
        emits_main,
        ..
    } = asm;
    let artifact_reads = one_shot_reads
        .unwrap_or_else(|| artifact_comptime_reads(cache, built_specs, &unit_reads, true));
    let artifact_fingerprint = artifact_fingerprint(
        ws,
        scx,
        &code_artifact,
        &lifecycle_plan,
        &definition_plan,
        &global_layout,
        &artifact_reads,
        cache.renderer_target,
    )?;
    step("assembly: fingerprint", &mut steps);
    // The file name a host writes the program to; generated code after a
    // numbered definition reports its position in that file.
    if cache.cg.line_directives() {
        out = skull_codegen_lua::resolve_line_directive_resets(&out, "main.lua");
    }
    cg.report_trace_frames(&out);
    step("assembly: finalize", &mut steps);
    phases.extend(steps);
    Ok((
        MirEmit {
            release_stats,
            compile_counts,
            lua: out,
            allocation_report,
            code_artifact: std::sync::Arc::new(code_artifact),
            lifecycle_plan,
            global_layout,
            definition_plan,
            artifact_fingerprint,
            phases,
            abi_plan_stats: scx.abi_plan_stats(),
            attribution: attribution.finish(),
            arena_traffic: ArenaTraffic::default(),
            setup_lanes: SetupLaneAttribution::default(),
            graph_census: crate::instance_graph::RunStats::default(),
            concrete_stats: crate::concrete_body::ConcreteBuildStats::default(),
            emits_main,
            shaders: std::sync::Arc::clone(&cache.shader_registrations.sources),
        },
        emitted_bodies,
        unit_reads,
    ))
}

fn code_slot_registration_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    artifact: &skull_mir::code_identity::CodeArtifact,
) -> Result<String, CompilerError> {
    artifact
        .check_stageable()
        .map_err(CompilerError::new_no_location)?;
    code_slot_registration_lua_entries(ws, scx, artifact.entries.values(), false)
}

pub(super) fn code_slot_registration_lua_entries<'a>(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    source: impl IntoIterator<Item = &'a skull_mir::code_identity::CodeEntry>,
    begin_when_empty: bool,
) -> Result<String, CompilerError> {
    let slots = skull_mir::code_identity::staged_code_slots(ws, scx.interner(), source)
        .map_err(CompilerError::new_no_location)?;
    if slots.is_empty() {
        return Ok(if begin_when_empty {
            "__rock_code_slot_begin()\n".to_string()
        } else {
            String::new()
        });
    }
    // Thousands of slots share a few hundred call ABIs, so each distinct ABI is
    // spelled once in a table. `do ... end` keeps that local out of the
    // enclosing function's local budget.
    let mut abi_index: FxHashMap<&str, usize> = FxHashMap::default();
    let mut abis = String::new();
    let mut stages = String::new();
    for slot in slots {
        let next = abi_index.len() + 1;
        let index = *abi_index.entry(slot.call_abi).or_insert_with(|| {
            writeln!(abis, "{},", skull_codegen_lua::lua_string(slot.call_abi))
                .expect("writing a String cannot fail");
            next
        });
        writeln!(
            stages,
            "__rock_code_slot_stage({}, {}, __rock_code_slot_abis[{index}])",
            skull_codegen_lua::lua_string(&slot.key),
            slot.body,
        )
        .expect("writing a String cannot fail");
    }
    Ok(format!(
        "__rock_code_slot_begin()\ndo\nlocal __rock_code_slot_abis = {{\n{abis}}}\n{stages}end\n"
    ))
}

pub(super) fn overlay_code_artifact(
    ws: &Workspace,
    cache: &SkullEmitCache,
    bodies: &[Instance],
) -> skull_mir::code_identity::CodeArtifact {
    let mut artifact = skull_mir::code_identity::CodeArtifact::default();
    for body in bodies {
        if let Some(Ok(chunk)) = cache.chunks.get(body) {
            for entry in chunk
                .code_artifact
                .entries
                .values()
                .filter(|entry| entry.escaping)
            {
                let mut entry = entry.clone();
                if entry.body_name.is_none() {
                    // An unchanged named value needs the resolved ABI as well as its body.
                    entry = cache
                        .code_artifact
                        .entries
                        .get(&entry.identity)
                        .expect("every escaping chunk entry belongs to the resolved code artifact")
                        .clone();
                }
                artifact.insert(entry);
            }
        }
        if !body.args.is_empty() {
            continue;
        }
        let fref = body.func;
        let identity = skull_mir::code_identity::CodeIdentity::Named(Instance::base(fref));
        let Some(entry) = cache.code_artifact.entries.get(&identity) else {
            continue;
        };
        let mut entry = entry.clone();
        let body_name = mangle_fn(ws, &ws.module(fref.module).functions[fref.func]);
        entry.body_name = Some(Ustr::from(&body_name));
        artifact.insert(entry);
    }
    artifact
}

pub(super) fn code_slot_publish_lua(
    artifact: &skull_mir::code_identity::CodeArtifact,
    has_definition_plan: bool,
    has_lifecycle_plan: bool,
) -> &'static str {
    if artifact
        .publication(has_definition_plan, has_lifecycle_plan)
        .publish
    {
        "__rock_code_slot_publish()\n"
    } else {
        ""
    }
}

fn definition_plan_stage_lua(
    ws: &Workspace,
    cg: &CodegenCtx,
    scx: &skull_mir::lower::ScalarCtx,
    concrete: &skull_mir::concrete::ConcretePlans,
    plan: &skull_mir::definition::DefinitionPlan,
) -> Result<String, CompilerError> {
    if plan.catalogs.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    let entries_by_catalog = plan.entries_by_catalog();
    let mut materializers = std::collections::BTreeMap::new();
    for entry in &plan.entries {
        let skull_mir::code_identity::CodeIdentity::Named(factory) = &entry.identity else {
            return Err(CompilerError::new_no_location(
                "definition factory has no named identity".to_string(),
            ));
        };
        let mut mf = skull_mir::lower::lower_definition_factory(ws, scx, concrete, factory)?;
        skull_transform::cleanup_with_nobox(ws, &mut mf)?;
        let body =
            skull_codegen_lua::emit_named_with_statics(ws, cg, scx.interner(), "", &mf, &[], None);
        if let Some(error) = cg.take_rejection() {
            return Err(error);
        }
        materializers.insert(entry.identity.clone(), body);
    }
    for catalog_plan in &plan.catalogs {
        if let Some(accessor) = catalog_plan.accessor {
            let function = &ws.module(accessor.module).functions[accessor.func];
            let name = mangle_fn(ws, function);
            let _ = writeln!(
                out,
                "function {}()\n  return __rock_definition_catalog({})\nend",
                name,
                skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            );
        }
    }
    out.push_str("__rock_definition_plan_stage({\n");
    for catalog_plan in &plan.catalogs {
        let entries = entries_by_catalog
            .get(&catalog_plan.catalog)
            .cloned()
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "[{}] = {{ catalog = {}, len = {}, _data = {{",
            skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            entries.len()
        );
        for (index, entry) in entries.into_iter().enumerate() {
            let key = entry.identity.logical_key(ws, scx.interner());
            let _ = writeln!(
                out,
                "[{}] = {{ catalog = {}, key = {}, name = {}, ordinal = {}, factory = __rock_code_slot_pending_ref({}) }},",
                index + 1,
                skull_codegen_lua::lua_string(entry.catalog.as_str()),
                skull_codegen_lua::lua_string(entry.key.as_str()),
                skull_codegen_lua::lua_string(entry.diagnostic_name.as_str()),
                entry.declaration_ordinal,
                skull_codegen_lua::lua_string(&key),
            );
        }
        out.push_str("} },\n");
    }
    out.push_str("}, {\n");
    for (identity, body) in materializers {
        let _ = writeln!(
            out,
            "[{}] = {},",
            skull_codegen_lua::lua_string(&identity.logical_key(ws, scx.interner())),
            body
        );
    }
    out.push_str("})\n");
    Ok(out)
}

pub(super) fn lifecycle_plan_stage_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    plan: &skull_mir::lifecycle::LifecyclePlan,
) -> String {
    if plan.census.declarations == 0 {
        return String::new();
    }
    let mut out = String::from("__rock_lifecycle_plan_stage({\n");
    for (phase, entries) in plan.entries_by_phase() {
        let scene_owned = entries
            .iter()
            .filter(|entry| entry.owner.as_str() == "scene")
            .count();
        let _ = writeln!(
            out,
            "[{}] = {{ len = {}, scene_owned = {}, _data = {{",
            skull_codegen_lua::lua_string(phase.as_str()),
            entries.len(),
            scene_owned
        );
        for (index, entry) in entries.into_iter().enumerate() {
            let key = entry.identity.logical_key(ws, scx.interner());
            let dependencies = entry.qualification.dependencies();
            let root_type = match &entry.root_type {
                Some(root) => skull_codegen_lua::lua_string(
                    &skull_mir::lifecycle::scene_root_type_key(root).unwrap_or_else(|| {
                        panic!("validated scene lifecycle root reached emission as {root:?}")
                    }),
                ),
                None => "nil".to_string(),
            };
            // A compiler-synthesized thunk of one uniform signature
            // (`scene_id: int -> ()`) per entry, process- and scene-owned alike
            // -- never the raw slot. A process thunk ignores its argument; a
            // scene thunk resolves the live root for `scene_id` and calls
            // through, skipping a Scene that closed since the population
            // snapshot was taken (`__scene_root_any` returns nil there). Neither
            // arm asks the value its runtime type: membership in the type's live
            // population, established once at Scene creation
            // (`__scene_type_bucket_insert`), is the only test.
            let sim_guard = (!entry.sim_wires.is_empty()).then(|| {
                entry
                    .sim_wires
                    .iter()
                    .map(|wire| {
                        format!(
                            "_G.__ROCK_ACTIVE_SIM == {}",
                            skull_codegen_lua::lua_string(wire.as_str())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" or ")
            });
            let func_thunk = if entry.owner.as_str() == "scene" {
                let call = "local __root = __scene_root_any(scene_id); if __root ~= nil then __s(__root) end";
                let body = sim_guard
                    .as_ref()
                    .map(|guard| format!("if {guard} then {call} end"))
                    .unwrap_or_else(|| call.to_string());
                format!(
                    "(function() local __s = __rock_code_slot_ref({key}); return function(scene_id) {body} end end)()",
                    key = skull_codegen_lua::lua_string(&key),
                )
            } else {
                let call = "__s()";
                let body = sim_guard
                    .as_ref()
                    .map(|guard| format!("if {guard} then {call} end"))
                    .unwrap_or_else(|| call.to_string());
                format!(
                    "(function() local __s = __rock_code_slot_ref({key}); return function(__scene_id) {body} end end)()",
                    key = skull_codegen_lua::lua_string(&key),
                )
            };
            let _ = writeln!(
                out,
                "[{}] = {{ phase = {}, owner = {}, root_type = {}, key = {}, name = {}, stage = {}, ordinal = {}, func = {}, qualification = {}, body_fingerprint = {}, dependencies = {{ len = {}, _data = {{",
                index + 1,
                skull_codegen_lua::lua_string(entry.phase.as_str()),
                skull_codegen_lua::lua_string(entry.owner.as_str()),
                root_type,
                skull_codegen_lua::lua_string(&key),
                skull_codegen_lua::lua_string(entry.diagnostic_name.as_str()),
                entry.stage,
                entry.declaration_ordinal,
                func_thunk,
                skull_codegen_lua::lua_string(entry.qualification.tag()),
                skull_codegen_lua::lua_string(&format!("{:016x}", entry.body_fingerprint)),
                dependencies.len()
            );
            for (dependency_index, dependency) in dependencies.iter().enumerate() {
                let dependency_key = dependency.identity.logical_key(ws, scx.interner());
                let _ = writeln!(
                    out,
                    "[{}] = {{ key = {}, fingerprint = {} }},",
                    dependency_index + 1,
                    skull_codegen_lua::lua_string(&dependency_key),
                    skull_codegen_lua::lua_string(&format!("{:016x}", dependency.fingerprint))
                );
            }
            out.push_str("} } },\n");
        }
        out.push_str("} },\n");
    }
    out.push_str("})\n");
    out
}
