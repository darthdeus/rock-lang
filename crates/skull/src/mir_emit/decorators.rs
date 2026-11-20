use super::*;

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
    let deco_name = decorator_symbol(d.def_key.as_str());
    out.attribution.owner = format!("m{}:deco{}", mid.index(), u32::from(did.into_raw()));
    out.attribution.source = BodySource::Decorator(mid, did);
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
            let unit = UnitBody {
                mid,
                owner: BodyOwner::Decorator(did),
                symbol: &deco_name,
                check_name: &deco_name,
                source_name: d.name.as_str(),
                span: d.span,
            };
            let Some(lua) = unit.emit(ws, scx, cg, &mut mf, &mut out) else {
                return out;
            };
            emit_lowered(
                ws,
                cg,
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

pub(super) fn decorator_application_lines(
    ws: &Workspace,
    decorators: &DecoratorTable,
    scx: &skull_mir::lower::ScalarCtx,
) -> Result<String, CompilerError> {
    let mut deco_lines = String::new();
    for app in decorators.applications(ws, bone_semantic::hir::FnBackend::Lua) {
        let (dmid, did) = app.decorator;
        let identity = skull_mir::code_identity::CodeIdentity::Named(Instance::base(app.func));
        let key = identity.logical_key(ws, scx.interner());
        let target = format!(
            "{{ name = {:?}, params = {{}}, func = __rock_code_slot_ref({:?}) }}",
            bone_core::modules::display_name(app.function.name.as_str()),
            key
        );
        let mut call_args = vec![target];
        call_args.extend(crate::decorator_call_args(ws, dmid, did, app.annotation)?);
        let _ = writeln!(
            deco_lines,
            "{}({})",
            decorator_symbol(ws.module(dmid).decorators[did].def_key.as_str()),
            call_args.join(", ")
        );
    }
    Ok(deco_lines)
}
