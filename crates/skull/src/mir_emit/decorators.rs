//! Decorator units and decorator application lines.

use super::*;

/// Lower + emit one decorator definition (`deco <name>` -> `__deco_<name>`).
pub(super) fn emit_decorator_unit(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    cg: &CodegenCtx,
    concrete: &crate::concrete_body::ConcreteBodies,
    mid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
) -> UnitOut {
    let mut out = UnitOut::default();
    let d = &ws.module(mid).decorators[did];
    let deco_name = crate::decorator_fn_name(d.def_key.as_str());
    out.attribution.owner = format!("m{}:deco{}", mid.index(), u32::from(did.into_raw()));
    let tl = Instant::now();
    let owner = skull_mir::concrete::ConcreteOwner::Decorator {
        module: mid,
        index: did.into_raw().into_u32(),
    };
    let concrete_plan = concrete.plan_for_owner(&owner);
    let lowered = concrete_plan
        .ok_or_else(|| {
            CompilerError::new_with_label(d.span, "no concrete plan for decorator".to_string())
        })
        .and_then(|plan| {
            skull_mir::lower::lower_decorator_with_plans(
                ws,
                scx,
                plan,
                &concrete.plans,
                ustr::Ustr::from(deco_name.as_str()),
            )
        });
    out.times.lower = tl.elapsed();
    match lowered {
        Ok(mut mf) => {
            out.attribution.examined = true;
            out.attribution.hir_nodes = hir_nodes_for_owner(ws, mid, BodyOwner::Decorator(did));
            out.attribution.mir_instructions = mf.insts.len();
            out.attribution.lower_facts = mf.lower_attribution.clone();
            out.times.lower_work = out.attribution.mir_instructions > 0;
            let cleanup_before = mir_live_instructions(&mf);
            let tc = Instant::now();
            if let Err(e) = skull_transform::cleanup_with_nobox(ws, &mut mf) {
                out.hard_error = Some(e);
                return out;
            }
            out.times.cleanup = tc.elapsed();
            out.times.cleanup_work = mir_live_instructions(&mf) != cleanup_before;
            // Decorator bodies can carry lambdas too -- convert like an ordinary fn.
            let pre_nested = mf.nested_fns.len();
            let tclosure = Instant::now();
            skull_transform::convert_closures(&mut mf, &deco_name);
            out.times.closure = tclosure.elapsed();
            out.times.closure_work = mf.nested_fns.len() > pre_nested;
            for nf in &mf.nested_fns[pre_nested..] {
                cg.mint_synthetic(nf.name.to_string());
            }
            let owner = bone_semantic::hir::BodyOwner::Decorator(did);
            let plan = concrete_plan.expect("a lowered decorator has a concrete plan");
            out.attribution.always_lowered = true;
            out.attribution
                .always_callee_keys
                .extend(plan.callees.values().cloned());
            let ctx = skull_mir::bound::concrete_body_ctx(
                ws,
                mid,
                owner,
                &plan.bound_tree,
                scx.interner(),
            );
            let tv = Instant::now();
            let check =
                check_unit_spec_bindings(ws, &ctx, mid, &deco_name, &mf, d.span, &mut out.specs);
            out.times.validation += tv.elapsed();
            out.times.validation_work = true;
            if let Err(e) = check {
                out.hard_error = Some(e);
                return out;
            }
            skull_codegen_lua::regalloc_time_reset();
            let te = Instant::now();
            // A decorator / global-initializer body runs once at chunk load,
            // outside the definitions section the binding pass covers.
            let emission = emit_named_with_allocation_report(
                ws,
                cg,
                scx.interner(),
                &deco_name,
                &mf,
                &[],
                None,
            );
            let mut lua = emission.lua;
            if let Some(body) = crate::allocation_report::collect_emitted_body(
                ws,
                scx.interner(),
                &deco_name,
                d.name.as_str(),
                &mf,
                emission.allocation_sites,
            ) {
                out.allocation_bodies.push(body);
            }
            let root_emit = te.elapsed();
            out.times.emit += root_emit;
            out.times.emit_work |= root_emit > Duration::ZERO;
            let regalloc = skull_codegen_lua::regalloc_time_elapsed().min(root_emit);
            out.times.regalloc += regalloc;
            out.times.regalloc_work |= regalloc > Duration::ZERO;
            if let Err(e) = emit_pass_nested_fns(
                ws,
                cg,
                scx,
                &mut mf.nested_fns,
                &mf.statics,
                Some(&mut out.allocation_bodies),
                &mut lua,
                &mut out.times,
            ) {
                out.hard_error = Some(e);
                return out;
            }
            emit_lowered(
                ws,
                &mut out,
                &mf,
                lua,
                &deco_name,
                Some(Ustr::from(&deco_name)),
                Duration::ZERO,
            );
            out.attribution.times = out.times;
        }
        Err(e) => out.hard_error = Some(e),
    }
    out
}

/// Assemble the full Lua program: prelude + decorator defs + the function section
/// (cached chunks for ordinary/coroutine bodies, regenerated externs/inline(lua))
/// plus dependency-ordered global inits + decorator applications. Everything except
/// the cached function chunks is regenerated here, so this is shared verbatim by
/// the full and incremental paths and there is no stale prelude/ordering.
///
/// The decorator and global definitions are re-lowered+emitted in PARALLEL
/// ([`par_map`]); cached function chunks are pulled and their captured phase
/// times summed in, so `backend-stats` still sees the full per-stage breakdown.
pub(crate) type DecoMap = FxHashMap<Ustr, (ModuleId, Idx<bone_semantic::hir::DecoratorDef>)>;

/// Name -> definition map for every `deco` in the workspace, plus the def list
/// (an application usually names a decorator declared in another module). Shared
/// by `assemble` (which also emits the def bodies) and `assemble_overlay`.
pub(crate) fn build_deco_map(
    ws: &Workspace,
) -> (
    DecoMap,
    Vec<(ModuleId, Idx<bone_semantic::hir::DecoratorDef>)>,
) {
    let mut deco_map: DecoMap = Default::default();
    let mut deco_items: Vec<(ModuleId, Idx<bone_semantic::hir::DecoratorDef>)> = Vec::new();
    for module in ws.modules.iter() {
        for (did, d) in module.decorators.iter() {
            deco_map.insert(d.def_key, (module.id, did));
            if !skull_mir::definition::is_compiler_owned_decorator(d) {
                deco_items.push((module.id, did));
            }
        }
    }
    (deco_map, deco_items)
}

/// Decorator APPLICATION lines (`__deco_<name>({ name, func }, ...)`) for every
/// decorated fn workspace-wide -- registration calls only, no fn bodies. Shared
/// by `assemble` (full) and `assemble_overlay` (the incremental rebind that
/// re-points system registries at the current globals after a body edit).
pub(super) fn decorator_application_lines(
    ws: &Workspace,
    deco_map: &DecoMap,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<String, CompilerError> {
    let mut deco_lines = String::new();
    for module in ws.modules.iter() {
        if module.id == ModuleId::builtin() {
            continue;
        }
        for (_fid, f) in module.functions.iter() {
            if f.backend == bone_semantic::hir::FnBackend::Js {
                continue;
            }
            for ann in &f.annotations {
                let app_key = ann.resolved_name.unwrap_or(ann.name.name);
                let Some(&(dmid, did)) = deco_map.get(&app_key) else {
                    continue;
                };
                let d = &ws.module(dmid).decorators[did];
                if skull_mir::definition::is_compiler_owned_decorator(d) {
                    continue;
                }
                let identity =
                    skull_mir::code_identity::CodeIdentity::Named(Instance::base(FuncRef {
                        module: module.id,
                        func: _fid,
                    }));
                let key = identity.logical_key(ws, scx.interner());
                let target = format!(
                    "{{ name = {:?}, params = {{}}, func = __rock_code_slot_ref({:?}) }}",
                    f.name.as_str(),
                    key
                );
                let mut call_args = vec![target];
                call_args.extend(crate::decorator_call_args(ws, dmid, did, ann)?);
                let _ = writeln!(
                    deco_lines,
                    "{}({})",
                    crate::decorator_fn_name(app_key.as_str()),
                    call_args.join(", ")
                );
            }
        }
    }
    Ok(deco_lines)
}
