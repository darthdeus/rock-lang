use super::*;
use bone_semantic::prelude::Span;

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
        for (key, old_reads) in &cache.edit.unit_comptime_reads.globals {
            if !unit_reads.globals.contains_key(key) {
                reads.extend(old_reads.iter().cloned());
            }
        }
        reads.extend(unit_reads.globals.values().flatten().cloned());
        if unit_reads.decorators.is_empty() {
            reads.extend(cache.edit.unit_comptime_reads.decorators.iter().cloned());
        } else {
            reads.extend(unit_reads.decorators.iter().cloned());
        }
    }
    reads
}

pub(super) fn artifact_validation_error(
    error: crate::artifact_fingerprint::FingerprintError,
) -> CompilerError {
    CompilerError::new_no_location(format!("Artifact fingerprint generation failed: {error}"))
}

struct Assembly {
    out: String,
    times: PhaseTimes,
    t_scaffold: Duration,
    attribution: AttributionAccumulator,
    needed_specs: Vec<skull_mir::bound::SpecDemand>,
    rejections: Rejections,
    unit_facts: Vec<(Ustr, Vec<Ustr>, Vec<Ustr>)>,
    all_statics: Vec<skull_mir::mir::StaticCell>,
    code_artifact: skull_mir::code_identity::CodeArtifact,
    allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    pc_owners: BTreeMap<String, (&'static str, Span)>,
    always_emitted_calls: Vec<FuncRef>,
    always_emitted_names: Vec<Ustr>,
    unit_reads: UnitComptimeReads,
    emits_main: bool,
}

impl Assembly {
    fn new(cache: &SkullEmitCache, units: UnitFacts) -> Self {
        let mut attribution = AttributionAccumulator {
            enabled: cache.attribution,
            ..AttributionAccumulator::default()
        };
        for unit in &units.attribution {
            attribution.add_unit(unit);
        }
        let mut rejections = Rejections::default();
        rejections.extend(units.rejections);
        Self {
            out: String::new(),
            times: units.times,
            t_scaffold: Duration::ZERO,
            attribution,
            needed_specs: units.specs,
            rejections,
            unit_facts: units.facts,
            all_statics: Vec::new(),
            code_artifact: units.code_artifact,
            allocation_bodies: units.allocation_bodies,
            pc_owners: BTreeMap::new(),
            always_emitted_calls: units.calls,
            always_emitted_names: units.names,
            unit_reads: units.reads,
            emits_main: false,
        }
    }

    fn push_function_definitions<'c>(
        &mut self,
        ws: &Workspace,
        cache: &'c SkullEmitCache,
        needed_foreign: &FxHashSet<Instance>,
        definitions: &mut DefinitionOrder<'c>,
        facts: &mut Vec<(Ustr, &'c Chunk)>,
    ) {
        let fns: Vec<(FuncRef, &bone_semantic::hir::Function)> = source_functions(ws).collect();
        let allocation =
            crate::allocation_report::enabled().then(crate::allocation_report::activation_enabled);
        let placements: Vec<Placement<'c>> = {
            use bone_core::prelude::*;
            fns.par_iter()
                .map(|&(self_ref, f)| {
                    place_function(ws, cache, needed_foreign, self_ref, f, allocation)
                })
                .collect()
        };
        let pc_sources = allocation == Some(false) && !cache.cg.line_directives();
        for ((function, f), placement) in fns.into_iter().zip(placements) {
            match placement {
                Placement::None => {}
                Placement::Text(Ok((text, allocation))) => {
                    if pc_sources {
                        let kind = if f.is_extern {
                            "ffi-boundary"
                        } else if f.name.as_str() == "__map_col_new_spine"
                            && ws.module(function.module).file.as_str() == "<stdlib>/unified.rock"
                        {
                            "map-construction"
                        } else {
                            "inline-lua"
                        };
                        self.pc_owners.insert(mangle_fn(ws, f), (kind, f.name_span));
                    }
                    definitions.push_text(text);
                    self.allocation_bodies.extend(allocation);
                }
                Placement::Text(Err(error)) => self.rejections.push(error),
                Placement::Chunk(emit_def, chunk) => {
                    self.pull_cached_chunk(f, chunk, emit_def, definitions, facts)
                }
            }
        }
    }

    fn pull_cached_chunk<'c>(
        &mut self,
        f: &bone_semantic::hir::Function,
        chunk: Option<Result<(Ustr, &'c Chunk), &CompilerError>>,
        emit_def: bool,
        definitions: &mut DefinitionOrder<'c>,
        facts: &mut Vec<(Ustr, &'c Chunk)>,
    ) {
        match chunk {
            Some(Ok((key, chunk))) => {
                facts.push((key, chunk));
                self.times.add(&chunk.times);
                if let Some(unit) = &chunk.attribution {
                    self.attribution.add_unit(unit);
                }
                if emit_def {
                    self.emits_main |= key.as_str() == "main";
                    definitions.push_chunk(chunk);
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

    fn push_initializers(
        &mut self,
        ws: &Workspace,
        cache: &SkullEmitCache,
        pending_inits: Vec<(Ustr, Ustr, String)>,
        ordering: InitOrdering,
        decorators: &DecoratorTable,
        has_code_slot_publication: bool,
        has_definition_plan: bool,
        lifecycle_declarations: bool,
    ) -> Result<(), CompilerError> {
        let scx = &cache.scx;
        let ts = Instant::now();
        let InitOrdering {
            reach,
            leaf_owner,
            static_resets,
        } = ordering;
        let leaf_owner = leaf_owner?;
        let inits: Vec<PendingInit<String>> = pending_inits
            .into_iter()
            .map(|(name, init_name, line)| PendingInit {
                name,
                deps: reach.deps_of(init_name),
                item: line,
            })
            .collect();
        let static_resets = static_resets?;
        let out = &mut self.out;
        out.push_str(&static_resets);
        let init_lines = sort_global_init_lines(inits, &leaf_owner)?;
        out.push_str(&state_initializer_lua(&init_lines));

        // After the global inits, so a decorator body can push to a global.
        out.push_str(&decorator_application_lines(ws, decorators, scx)?);
        if cache.build_kind == BuildKind::Program {
            out.push_str(&sim_registration_lines(
                ws,
                scx,
                cache.selected_sim_functions.as_ref(),
            )?);
        }
        // Top-level, so a reload re-runs them with the new GLSL.
        out.push_str(&cache.shader_registrations.lua);
        if has_code_slot_publication {
            out.push_str(code_slot_publish_lua(
                &self.code_artifact,
                has_definition_plan,
                lifecycle_declarations,
            ));
        }
        // Close the block opened by the runtime prelude.
        out.push_str("end\n");
        self.t_scaffold += ts.elapsed();
        Ok(())
    }

    fn phases(&self) -> Vec<(&'static str, Duration)> {
        let times = &self.times;
        let t_stringify = times.emit - times.regalloc;
        vec![
            ("lower hir->mir", times.lower),
            ("inline plan+splice", times.inline),
            ("cleanup mir", times.cleanup),
            ("closure conversion", times.closure),
            ("counted-loop recovery", times.counted_loop),
            ("bounds-check elimination", times.bounds),
            ("hoisted-body inline", times.hoisted_inline),
            ("post-transform validation", times.validation),
            ("emit: regalloc", times.regalloc),
            ("emit: stringify", t_stringify),
            ("scaffold+callgraph+order", self.t_scaffold),
        ]
    }
}

fn collect_needed_foreign(
    ws: &Workspace,
    cache: &SkullEmitCache,
    always_emitted_calls: &[FuncRef],
    spec_chunks: &FxHashMap<Instance, Chunk>,
) -> FxHashSet<Instance> {
    const BODIES_PER_JOB: usize = 512;
    let foreign = |f: &FuncRef| ws.module(f.module).functions[f.func].inline_body.is_some();
    let fns: Vec<(FuncRef, &bone_semantic::hir::Function)> = source_functions(ws).collect();
    let specs: Vec<&Chunk> = spec_chunks.values().collect();
    let jobs: Vec<(&[(FuncRef, &bone_semantic::hir::Function)], &[&Chunk])> = fns
        .chunks(BODIES_PER_JOB)
        .map(|fns| (fns, &[][..]))
        .chain(specs.chunks(BODIES_PER_JOB).map(|specs| (&[][..], specs)))
        .collect();
    let found = par_map(jobs, |(fns, specs)| {
        let mut calls: FxHashSet<FuncRef> = FxHashSet::default();
        for &(self_ref, f) in fns {
            if !is_cached_emitted_body(f, &shared(self_ref))
                || !should_emit_via_view(ws, &cache.lua_view, self_ref)
            {
                continue;
            }
            if let Some(Ok(c)) = cache.chunks.get(&shared(self_ref)) {
                calls.extend(c.calls.iter().copied().filter(foreign));
            }
        }
        for c in specs {
            calls.extend(c.calls.iter().copied().filter(foreign));
        }
        calls
    });
    let always = always_emitted_calls.iter().copied().filter(foreign);
    always
        .chain(found.into_iter().flatten())
        .map(shared)
        .collect()
}

fn source_functions(
    ws: &Workspace,
) -> impl Iterator<Item = (FuncRef, &bone_semantic::hir::Function)> {
    ws.source_modules().flat_map(|module| {
        let mid = module.id;
        module.functions.iter().map(move |(fid, f)| {
            (
                FuncRef {
                    module: mid,
                    func: fid,
                },
                f,
            )
        })
    })
}

pub(super) fn ffi_extern_wrapper(
    ws: &Workspace,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    lib: Ustr,
    allocation_mode: Option<bool>,
) -> Result<LuaDefinition, CompilerError> {
    use crate::allocation_report::{self as report, AllocationResultUse};
    use skull_codegen_lua::EmittedAllocationKind as Kind;

    let nm = mangle_fn(ws, f);
    let params = f
        .param_decls
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let prologue = default_fill_prologue(ws, self_ref.module, self_ref.func, f);
    let mut allocation = None;
    let body = match extern_ret(ws, f)? {
        // Returns an owning copy: LuaJIT does not keep the parent
        // array alive through a reference into it.
        ExternRet::ValueStruct(key) => {
            let sname = skull_mir::mangle::type_symbol(ws, ustr::Ustr::from(&key));
            let call_args = if params.is_empty() {
                "__out".to_string()
            } else {
                format!("__out, {params}")
            };
            let checked = noalloc::marked(f);
            let sites = (checked || allocation_mode.is_some()).then(|| {
                let line = 2 + prologue.lines().count() as u32;
                [
                    report::generated_site(
                        &nm,
                        f.name_span,
                        Kind::FfiArray,
                        line,
                        AllocationResultUse::DirectCall,
                    )
                    .with_reason("allocates an FFI return buffer".to_string()),
                    report::generated_site(
                        &nm,
                        f.name_span,
                        Kind::FfiReturnValue,
                        line + 2,
                        AllocationResultUse::Returned,
                    )
                    .with_reason(format!("copies `{key}` returned by FFI")),
                ]
            });
            if let Some([array, value]) = &sites
                && checked
            {
                return Err(noalloc::reject_generated(f, array)
                    .with_siblings([noalloc::reject_generated(f, value)]));
            }
            let instrumented = sites.as_ref().filter(|_| allocation_mode == Some(true));
            let temp = report::instrument_generated(
                instrumented.map(|sites| &sites[0]),
                format!("ffi.new(\"{sname}[1]\")"),
            );
            let result = report::instrument_generated(
                instrumented.map(|sites| &sites[1]),
                format!("ffi.new(\"{sname}\", __out[0])"),
            );
            allocation = sites.map(|sites| report::AllocationBody {
                emitted_name: nm.clone(),
                diagnostic_name: f.name.to_string(),
                declaration_span: Some(f.name_span),
                direct_sites: sites.into(),
                call_edges: Vec::new(),
                pc_sources: Vec::new(),
            });
            format!(
                "  local __out = {temp}\n  {lib}.{nm}({call_args})\n{}",
                skull_codegen_lua::wrapper_return(&result)
            )
        }
        // `return <call>` is a `CALLT` into C, which a root trace cannot
        // leave through; the bound local makes it a `RET`.
        ExternRet::Int | ExternRet::Uint => {
            skull_codegen_lua::wrapper_return(&format!("tonumber({lib}.{nm}({params}))"))
        }
        // `Bool` ends in a compare, not a call.
        ExternRet::Bool => format!("  return {lib}.{nm}({params}) ~= 0"),
        ExternRet::AsIs => skull_codegen_lua::wrapper_return(&format!("{lib}.{nm}({params})")),
        ExternRet::None => format!("  {lib}.{nm}({params})"),
    };
    Ok((
        format!("function {nm}({params})\n{prologue}{body}\nend\n"),
        allocation,
    ))
}

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

struct InitOrdering {
    reach: skull_mir::reach::GlobalReach,
    leaf_owner: Result<FxHashMap<Ustr, Ustr>, CompilerError>,
    static_resets: Result<String, CompilerError>,
}

/// Initializer ordering uses the dependency set, not traversal order.
fn transitive_global_reads(
    inits: &[(Ustr, Ustr, String)],
    units: &[(Ustr, Vec<Ustr>, Vec<Ustr>)],
    declarations: &[(Ustr, &Chunk)],
) -> skull_mir::reach::GlobalReach {
    let mut facts: FxHashMap<Ustr, (&[Ustr], &[Ustr])> = FxHashMap::default();
    for (key, read, called) in units {
        facts.insert(*key, (read, called));
    }
    for (key, chunk) in declarations {
        facts.insert(*key, (&chunk.direct_globals, &chunk.direct_calls));
    }
    let mut globals: FxHashMap<Ustr, &[Ustr]> = FxHashMap::default();
    let mut calls: FxHashMap<Ustr, &[Ustr]> = FxHashMap::default();
    let mut pending: Vec<Ustr> = inits.iter().map(|(_, init, _)| *init).collect();
    while let Some(body) = pending.pop() {
        if calls.contains_key(&body) {
            continue;
        }
        let Some(&(read, called)) = facts.get(&body) else {
            continue;
        };
        globals.insert(body, read);
        calls.insert(body, called);
        pending.extend(called.iter().copied());
    }
    skull_mir::reach::transitive_global_reads(&globals, &calls)
}

/// A `nobox` global is read through its leaves, so a leaf dependency must
/// resolve back to its owning global or the dependent initializer sorts first.
fn global_leaf_owners(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<FxHashMap<Ustr, Ustr>, CompilerError> {
    let mut leaf_owner: FxHashMap<Ustr, Ustr> = FxHashMap::default();
    for module in ws.modules.iter() {
        for (_, g) in module.globals.iter() {
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            if let Some(leaves) = plan.leaf_names(skull_mir::mangle::symbol_of_key(g.name)) {
                for leaf in leaves {
                    leaf_owner.insert(leaf, g.name);
                }
            }
        }
    }
    Ok(leaf_owner)
}

/// On reload a static is reset only when its init or type key changed; an
/// unchanged key keeps the live cell.
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

type LuaDefinition = (String, Option<crate::allocation_report::AllocationBody>);

enum Placement<'c> {
    None,
    Text(Result<LuaDefinition, CompilerError>),
    Chunk(bool, Option<Result<(Ustr, &'c Chunk), &'c CompilerError>>),
}

/// A JS-backend declaration is inert here: a Lua sibling carries the binding,
/// and a JS-only call from Lua is rejected at call lowering.
fn place_function<'c>(
    ws: &Workspace,
    cache: &'c SkullEmitCache,
    needed_foreign: &FxHashSet<Instance>,
    self_ref: FuncRef,
    f: &bone_semantic::hir::Function,
    allocation_mode: Option<bool>,
) -> Placement<'c> {
    if f.backend == bone_semantic::hir::FnBackend::Js {
        return Placement::None;
    }
    let emit_def = should_emit_via_view(ws, &cache.lua_view, self_ref);
    if f.is_extern {
        let Some(lib) = f.ffi_lib else {
            return Placement::None;
        };
        // Pruning gates emission, not extern validation.
        return match ffi_extern_wrapper(ws, self_ref, f, lib, allocation_mode) {
            Ok(definition) if emit_def => Placement::Text(Ok(definition)),
            Ok(_) => Placement::None,
            Err(error) => Placement::Text(Err(error)),
        };
    }
    if let Some((_, body)) = f.inline_body {
        if noalloc::marked(f) {
            return Placement::Text(Err(noalloc::reject_inline(f)));
        }
        if !emits_foreign_definition(ws, &cache.lua_view, needed_foreign, self_ref) {
            return Placement::None;
        }
        return Placement::Text(Ok((
            inline_lua_definition(ws, &cache.scx, self_ref, f, body.as_str()),
            None,
        )));
    }
    if f.has_iterator_bounds() || is_generic(f) {
        return Placement::None;
    }
    let chunk = match cache.chunks.get(&shared(self_ref)) {
        Some(Ok(chunk)) => Some(Ok((chunk.name, chunk))),
        Some(Err(err)) => Some(Err(err)),
        None => None,
    };
    Placement::Chunk(emit_def, chunk)
}

fn release_one_shot_chunks(
    cache: &mut SkullEmitCache,
    built_specs: &mut Vec<(Instance, Chunk)>,
    compile_counts: &mut CompileCounts,
) {
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
    drop_in_background(std::mem::take(&mut cache.chunks));
    drop_in_background(std::mem::take(built_specs));
}

type GlobalKey = (ModuleId, Idx<bone_semantic::hir::GlobalDef>);

fn scaffold_prelude_lua(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
) -> Result<(String, crate::ValueCtors), CompilerError> {
    let mut out = String::from(crate::runtime_prelude());
    if skull_mir::verify::checks_enabled() {
        out.push_str(
            r#"
function __rock_value_integer_check(value, unsigned)
  if value == nil then return value end
  local word = bit.tobit(value)
  if unsigned and word < 0 then word = word + 4294967296 end
  if value == word then return value end
  error("value struct integer field out of range")
end
"#,
        );
    }
    if crate::allocation_report::activation_enabled() {
        out.push_str(skull_codegen_lua::ALLOCATION_ACTIVATION_PRELUDE);
    }
    out.push_str(&crate::enum_cdata_defs(ws));
    let rep_arena = scx.rep_arena();
    let vstructs = crate::canonical_value_structs(ws);
    let mut value_ctors = Vec::new();
    let (vdefs, value_emitted, value_gc) =
        crate::value_struct_defs(ws, &vstructs, rep_arena, &mut value_ctors)?;
    out.push_str(&vdefs);
    out.push_str(&crate::value_struct_gc_defs(
        ws,
        &vstructs,
        &value_gc,
        rep_arena,
        &mut value_ctors,
    )?);
    let mut metatype_syms = Vec::new();
    out.push_str(&crate::value_struct_metatypes(
        ws,
        &vstructs,
        &value_emitted,
        &mut metatype_syms,
    ));
    out.push_str(&crate::enum_cdata_metatypes(ws, &mut metatype_syms));
    out.push_str(&crate::cdata_metatype_trampolines(&metatype_syms));
    out.push_str(&crate::c_struct_inspect_defs(ws, &vstructs, &value_emitted));
    out.push_str(&crate::type_table_defs(ws, &value_gc, rep_arena)?);
    out.push_str(&cg.record_slot_metadata(ws));
    out.push_str(&crate::enum_runtime_defs(ws));
    Ok((out, value_ctors))
}

#[derive(Default)]
struct UnitFacts {
    facts: Vec<(Ustr, Vec<Ustr>, Vec<Ustr>)>,
    calls: Vec<FuncRef>,
    names: Vec<Ustr>,
    code_artifact: skull_mir::code_identity::CodeArtifact,
    allocation_bodies: Vec<crate::allocation_report::AllocationBody>,
    times: PhaseTimes,
    reads: UnitComptimeReads,
    rejections: Vec<CompilerError>,
    specs: Vec<skull_mir::bound::SpecDemand>,
    attribution: Vec<UnitAttribution>,
}

impl UnitFacts {
    fn merge_units(
        &mut self,
        results: Vec<UnitOut>,
        attribution: bool,
        out: &mut String,
        pending_inits: &mut Vec<(Ustr, Ustr, String)>,
    ) {
        self.rejections
            .extend(results.iter().filter_map(|u| u.hard_error.clone()));
        for u in results {
            out.push_str(&u.lua);
            if let Some(facts) = u.refs {
                self.facts.push(facts);
            }
            if let Some(pi) = u.pending_init {
                pending_inits.push(pi);
            }
            self.calls.extend(u.calls);
            self.names.extend(u.global_names);
            self.specs.extend(u.specs);
            self.code_artifact.merge_body(&u.code_artifact);
            self.allocation_bodies.extend(u.allocation_bodies);
            if attribution {
                self.attribution.push(u.attribution);
            }
            self.times.add(&u.times);
        }
    }
}

struct FoldedUnits {
    decorator_lua: String,
    globals_lua: String,
    pending_inits: Vec<(Ustr, Ustr, String)>,
    facts: UnitFacts,
}

fn fold_units(
    decorator_units: Vec<UnitOut>,
    global_keys: &[GlobalKey],
    global_units: Vec<UnitOut>,
    attribution: bool,
) -> FoldedUnits {
    let scoped = |lua: &str| {
        let mut definitions = LuaDefinitionWriter::new(String::new());
        definitions.push_str(lua);
        definitions.finish()
    };
    let mut facts = UnitFacts::default();
    for u in &decorator_units {
        facts
            .reads
            .decorators
            .extend(u.comptime_reads.iter().cloned());
    }
    let mut decorator_lua = String::new();
    facts.merge_units(
        decorator_units,
        attribution,
        &mut decorator_lua,
        &mut Vec::new(),
    );
    // `par_map` preserves input order, so keys and units pair positionally.
    for (key, u) in global_keys.iter().zip(global_units.iter()) {
        if !u.comptime_reads.is_empty() {
            facts.reads.globals.insert(*key, u.comptime_reads.clone());
        }
    }
    let mut globals_lua = String::new();
    let mut pending_inits = Vec::new();
    facts.merge_units(
        global_units,
        attribution,
        &mut globals_lua,
        &mut pending_inits,
    );
    FoldedUnits {
        decorator_lua: scoped(&decorator_lua),
        globals_lua: scoped(&globals_lua),
        pending_inits,
        facts,
    }
}

pub(super) struct EarlyAssembly {
    scaffold: Result<(String, crate::ValueCtors), CompilerError>,
    scaffold_time: Duration,
    decorators: DecoratorTable,
    units: FoldedUnits,
    // Results, reported where assembly reports them, so body rejections come first.
    global_layout: Result<String, String>,
    globals_directory: Result<GlobalsDirectoryLua, CompilerError>,
    leaf_owner: Result<FxHashMap<Ustr, Ustr>, CompilerError>,
}

impl EarlyAssembly {
    pub(super) fn demands(&self) -> &[skull_mir::bound::SpecDemand] {
        &self.units.facts.specs
    }
}

pub(super) fn prepare_early_assembly(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    attribution: bool,
    global_accessors: bool,
) -> EarlyAssembly {
    let (scaffold, units) = parallel_join(
        || {
            let ts = Instant::now();
            let scaffold = scaffold_prelude_lua(ws, scx, cg);
            let scaffold_time = ts.elapsed();
            let global_layout = skull_mir::global_storage::global_layout_id(ws, scx);
            let globals_directory = globals_directory_lua(ws, scx, cg, global_accessors);
            let leaf_owner = global_leaf_owners(ws, scx);
            (
                scaffold,
                scaffold_time,
                global_layout,
                globals_directory,
                leaf_owner,
            )
        },
        || {
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
            let units = fold_units(decorator_units, &global_keys, global_units, attribution);
            (decorators, units)
        },
    );
    let (scaffold, scaffold_time, global_layout, globals_directory, leaf_owner) = scaffold;
    let (decorators, units) = units;
    EarlyAssembly {
        scaffold,
        scaffold_time,
        decorators,
        units,
        global_layout,
        globals_directory,
        leaf_owner,
    }
}

pub(super) fn assemble(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    built_specs: &mut Vec<(Instance, Chunk)>,
    lifetime: CacheLifetime,
    early: Option<EarlyAssembly>,
    specs: Option<SpecChunks>,
) -> Result<(MirEmit, FxHashSet<Instance>, UnitComptimeReads), CompilerError> {
    let mut compile_counts = CompileCounts {
        concrete_plans: cache.concrete.plans.len() + cache.concrete.always_plans.len(),
        chunks: cache.chunks.len(),
        dependency_edges: None,
    };
    let mut steps: Vec<(&'static str, Duration)> = Vec::new();
    let mut step_started = Instant::now();
    let mut step = |name: &'static str, steps: &mut Vec<(&'static str, Duration)>| {
        let now = Instant::now();
        steps.push((name, now - step_started));
        step_started = now;
    };
    let EarlyAssembly {
        scaffold,
        scaffold_time,
        decorators,
        units,
        global_layout,
        globals_directory,
        leaf_owner,
    } = early.unwrap_or_else(|| {
        prepare_early_assembly(
            ws,
            &cache.scx,
            &cache.cg,
            &cache.concrete,
            cache.attribution,
            cache.global_accessors,
        )
    });
    let FoldedUnits {
        decorator_lua,
        globals_lua,
        pending_inits,
        facts: unit_facts,
    } = units;
    let mut asm = Assembly::new(cache, unit_facts);
    let (scaffold, value_ctors) = scaffold?;
    asm.out = scaffold;
    // Load-time callers must follow their value constructors.
    let value_ctors_at = asm.out.len();
    asm.t_scaffold += scaffold_time;
    asm.out.push_str(&decorator_lua);
    step("assembly: prelude+globals", &mut steps);

    let (mut spec_chunks, spec_rejected) = specs.unwrap_or_else(|| {
        view_driven_specialized_chunks(ws, &cache.cg, &cache.scx, cache, &asm.needed_specs)
    });
    step("assembly: specialized chunks", &mut steps);
    let needed_foreign =
        || collect_needed_foreign(ws, cache, &asm.always_emitted_calls, &spec_chunks);
    let (needed_foreign, early_definition_stage) = if lifetime == CacheLifetime::OneShot {
        let (needed_foreign, stage) = parallel_join(needed_foreign, || {
            let plan = skull_mir::definition::collect(ws)?;
            let concrete = &cache.concrete.plans;
            let stage = definition_plan_stage_lua(ws, &cache.cg, &cache.scx, concrete, &plan)?;
            Ok::<_, CompilerError>((plan, stage))
        });
        let stage = stage?;
        // The definition plan stage is the last reader of the concrete plans.
        drop_in_background(std::mem::take(&mut cache.concrete));
        (needed_foreign, Some(stage))
    } else {
        (needed_foreign(), None)
    };

    let head = std::mem::take(&mut asm.out);
    let specialized_instances: Vec<_> = spec_chunks.keys().cloned().collect();
    let (definitions_lua, binds_lua, init_ordering, mut runtime_refs, enum_names_lua) = {
        let unit_demands = std::mem::take(&mut asm.needed_specs);
        let mut order = DefinitionOrder::default();
        let mut facts: Vec<(Ustr, &Chunk)> = Vec::new();
        asm.push_function_definitions(ws, cache, &needed_foreign, &mut order, &mut facts);
        let declared = order.emitted.len();
        let mut demands: Vec<&skull_mir::bound::SpecDemand> = unit_demands
            .iter()
            .chain(order.emitted.iter().flat_map(|&chunk| chunk.specs.iter()))
            .collect();
        let (errors, moved) = order_specialized_callees(
            ws,
            &cache.scx,
            &mut demands,
            &cache.chunks,
            &spec_chunks,
            &spec_rejected,
            &mut order,
            &mut asm.attribution,
            &mut asm.times,
        );
        asm.rejections.extend(errors);
        let moved_set: FxHashSet<&Instance> = moved.iter().collect();
        let scx = &cache.scx;
        let Assembly {
            unit_facts,
            code_artifact,
            all_statics,
            allocation_bodies,
            always_emitted_names,
            ..
        } = &mut asm;
        let order = &order;
        let ((definitions_lua, binds_lua), ((), (init_ordering, (runtime_refs, enum_names_lua)))) =
            parallel_join(
                || {
                    parallel_join(
                        || order.definitions_lua(),
                        || {
                            let mut binds: Vec<&skull_codegen_lua::CalleeBindingPlan> =
                                order.emitted.iter().flat_map(|c| c.binds.iter()).collect();
                            callee_binding_pass_lua(&mut binds)
                        },
                    )
                },
                || {
                    parallel_join(
                        || {
                            let declared = order.emitted[..declared].iter().copied();
                            let unmoved = spec_chunks
                                .iter()
                                .filter(|(instance, _)| !moved_set.contains(instance))
                                .map(|(_, chunk)| chunk);
                            let built = built_specs.iter().map(|(_, chunk)| chunk);
                            let moved = moved.iter().map(|instance| &spec_chunks[instance]);
                            let chunks = declared.chain(unmoved).chain(built).chain(moved);
                            code_artifact.merge_bodies(chunks.map(|chunk| &chunk.code_artifact));
                        },
                        || {
                            parallel_join(
                                || {
                                    all_statics.extend(
                                        order
                                            .emitted
                                            .iter()
                                            .flat_map(|c| c.statics.iter().cloned()),
                                    );
                                    if crate::allocation_report::enabled() {
                                        allocation_bodies.extend(
                                            order
                                                .emitted
                                                .iter()
                                                .flat_map(|c| c.allocation_bodies.iter().cloned()),
                                        );
                                    }
                                    InitOrdering {
                                        reach: transitive_global_reads(
                                            &pending_inits,
                                            unit_facts,
                                            &facts,
                                        ),
                                        leaf_owner,
                                        static_resets: static_reset_lines(ws, scx, all_statics),
                                    }
                                },
                                || {
                                    let mut refs = order.runtime_refs();
                                    refs.extend(always_emitted_names.iter().filter(|name| {
                                        crate::is_enum_names_global(name)
                                            || crate::is_value_ctor_global(name)
                                    }));
                                    let lua = crate::enum_names_defs(ws, &refs);
                                    (refs, lua)
                                },
                            )
                        },
                    )
                },
            );
        built_specs.extend(moved.into_iter().map(|instance| {
            let chunk = spec_chunks
                .remove(&instance)
                .expect("a moved spec stays in the live set until assembly moves it");
            (instance, chunk)
        }));
        (
            definitions_lua,
            binds_lua,
            init_ordering,
            runtime_refs,
            enum_names_lua,
        )
    };
    let one_shot_reads = if lifetime == CacheLifetime::OneShot {
        let reads = cache
            .validate_artifact
            .then(|| artifact_comptime_reads(cache, built_specs, &asm.unit_reads, true));
        release_one_shot_chunks(cache, built_specs, &mut compile_counts);
        reads
    } else {
        None
    };
    drop(spec_chunks);
    step("assembly: definitions", &mut steps);

    // Report body rejections before scaffolding errors.
    std::mem::take(&mut asm.rejections).into_result()?;
    let cg = &cache.cg;
    let scx = &cache.scx;
    if let Some(err) = cg.take_rejection() {
        return Err(err);
    }
    asm.out.push_str(&binds_lua);
    asm.out.push_str(&enum_names_lua);
    let glue_start = asm.out.len();
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
        None => (skull_mir::definition::collect(ws)?, None),
    };
    let lifecycle_plan = skull_mir::lifecycle::collect_with_artifact(ws, &asm.code_artifact)?;
    let mut allocation_report = if crate::allocation_report::enabled() {
        let mut report = crate::allocation_report::AllocationReport::from_bodies(std::mem::take(
            &mut asm.allocation_bodies,
        ));
        report.populate_lifecycle_routes(&lifecycle_plan, &asm.code_artifact);
        report.pc_owners = std::mem::take(&mut asm.pc_owners);
        report
    } else {
        crate::allocation_report::AllocationReport::default()
    };
    let code_artifact = &asm.code_artifact;
    let publication = code_artifact.publication(
        !definition_plan.catalogs.is_empty(),
        lifecycle_plan.declarations > 0,
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
    let directory = globals_directory?;
    out.push_str(&wrap_large_lua_section(
        "__rock_globals_directory_part",
        &directory.publish,
    ));
    // A host attaches its own directory fields right after this line.
    out.push_str(GLOBALS_BRIDGE_LINE);
    let bridge_end_in_tail = out.len();
    if !cg.line_directives() {
        out.push_str(&directory.accessors);
    }

    out.push_str(&globals_lua);
    let has_definition_plan = !definition_plan.catalogs.is_empty();
    if has_code_slot_publication {
        out.push_str(skull_codegen_lua::FN_MT_PRELUDE);
    }
    code_artifact
        .check_stageable()
        .map_err(CompilerError::new_no_location)?;
    // Slot staging follows the global initializer bodies and precedes their calls.
    let code_slot_stage =
        code_slot_registration_lua_entries(ws, scx, code_artifact.entries.values(), false)?;
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
        init_ordering,
        &decorators,
        has_code_slot_publication,
        has_definition_plan,
        lifecycle_plan.declarations > 0,
    )?;
    let mut glue_calls = Vec::new();
    crate::push_value_ctor_calls(&asm.out[glue_start..], &mut glue_calls);
    runtime_refs.extend(glue_calls);
    runtime_refs.extend(directory.ctors);
    let value_ctor_lua = crate::value_ctor_defs(&value_ctors, &runtime_refs);
    if crate::allocation_report::enabled()
        && !crate::allocation_report::activation_enabled()
        && !cg.line_directives()
    {
        for s in crate::canonical_value_structs(ws) {
            let symbol = skull_mir::mangle::type_symbol(ws, s.name);
            for suffix in ["_vt", "_lct"] {
                let name = format!("{symbol}{suffix}");
                if runtime_refs.contains(&Ustr::from(&name)) {
                    allocation_report
                        .pc_owners
                        .insert(name, ("generated-value-bridge", s.name_span));
                }
            }
        }
    }
    let mut globals_bridge_end =
        Some(head.len() + value_ctor_lua.len() + definitions_lua.len() + bridge_end_in_tail);
    asm.out = par_concat(&[
        &head[..value_ctors_at],
        &value_ctor_lua,
        &head[value_ctors_at..],
        &definitions_lua,
        &asm.out,
    ]);
    cache.value_ctors = value_ctors;
    step("assembly: initializers", &mut steps);
    let mut phases = asm.phases();

    let emitted_bodies = if lifetime == CacheLifetime::Retained {
        emitted_bodies(ws, cache, &needed_foreign, specialized_instances)
    } else {
        FxHashSet::default()
    };
    let Assembly {
        mut out,
        attribution,
        code_artifact,
        unit_reads,
        emits_main,
        ..
    } = asm;
    if cache.validate_artifact {
        let reads = one_shot_reads
            .unwrap_or_else(|| artifact_comptime_reads(cache, built_specs, &unit_reads, true));
        crate::artifact_fingerprint::validate(ws, &code_artifact, &definition_plan, &reads)
            .map_err(artifact_validation_error)?;
    }
    step("assembly: fingerprint", &mut steps);
    if cache.cg.line_directives() {
        out = skull_codegen_lua::resolve_line_directive_resets(&out, "main.lua");
        if !directory.accessors.is_empty() {
            let at = out
                .find(GLOBALS_BRIDGE_LINE)
                .expect("assembled global bridge")
                + GLOBALS_BRIDGE_LINE.len();
            out.insert_str(at, &directory.accessors);
        }
        globals_bridge_end = None;
    }
    step("assembly: finalize", &mut steps);
    phases.extend(steps);
    Ok((
        MirEmit {
            runtime_changed: true,
            compile_counts,
            lua: out,
            globals_bridge_end,
            allocation_report,
            code_artifact: std::sync::Arc::new(code_artifact),
            lifecycle_plan,
            global_layout,
            definition_plan,
            phases,
            attribution: attribution.finish(),
            emits_main,
            shaders: std::sync::Arc::clone(&cache.shader_registrations.sources),
        },
        emitted_bodies,
        unit_reads,
    ))
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
    // `do ... end` keeps the ABI table local out of the enclosing
    // function's Lua local budget.
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
                .entries()
                .iter()
                .filter(|entry| entry.escaping)
            {
                let mut entry = entry.clone();
                if entry.body_name.is_none() {
                    entry = cache
                        .edit
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
        let Some(entry) = cache.edit.code_artifact.entries.get(&identity) else {
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
        let body = skull_codegen_lua::emit_named(ws, cg, scx.interner(), "", &mf, None);
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
            "[{}] = setmetatable({{ [0] = {}, catalog = {},",
            skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
            entries.len(),
            skull_codegen_lua::lua_string(catalog_plan.catalog.as_str()),
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
        out.push_str("}, __rock_box_mts.Vec),\n");
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
    if plan.declarations == 0 {
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
            "[{}] = setmetatable({{ [0] = {}, scene_owned = {},",
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
            // A scene thunk skips a Scene that closed since the population snapshot
            // (`__scene_root_any` returns nil). Membership in the type's population is
            // the only test; the value's runtime type is never asked.
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
                "[{}] = {{ phase = {}, owner = {}, root_type = {}, key = {}, name = {}, stage = {}, ordinal = {}, func = {}, qualification = {}, body_fingerprint = {}, dependencies = setmetatable({{ [0] = {},",
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
            out.push_str("}, __rock_box_mts.Vec) },\n");
        }
        out.push_str("}, __rock_box_mts.Vec),\n");
    }
    out.push_str("})\n");
    out
}
