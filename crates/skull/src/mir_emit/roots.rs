use super::*;

/// A library prunes to what roots reach; the target's own sources are roots, so
/// compiling a file of helpers still emits its functions.
/// Whether a module is a library: the stdlib, or a file outside `project_root`.
pub(super) fn is_library_module(file: &str, project_root: Option<&str>) -> bool {
    if is_stdlib_module_file(file) {
        return true;
    }
    match project_root {
        Some(root) => !std::path::Path::new(file).starts_with(root),
        None => false,
    }
}

pub(crate) fn is_emission_root(
    file: &str,
    project_root: Option<&str>,
    program: bool,
    f: &bone_semantic::hir::Function,
    decorators: &DecoratorTable,
) -> bool {
    let own = !is_library_module(file, project_root);
    if own && !program {
        return true;
    }
    let declared = skull_mir::mangle::declared_name(f.name.as_str());
    if f.name.as_str() == "main" || (own && declared == "main") {
        return true;
    }
    let has = |n: &str| f.annotations.iter().any(|a| a.is_builtin(n));
    if has("sim") || has("test") {
        return true;
    }
    is_global_emission_root(f, decorators)
}

/// Generated text or outside code names these by their original name, with no
/// MIR call edge: `@nomangle` (other inline(lua) bodies, compiler codegen and
/// string-keyed dispatch), extern bindings and decorated functions.
fn is_global_emission_root(f: &bone_semantic::hir::Function, decorators: &DecoratorTable) -> bool {
    if f.json_field_for.is_some() || f.annotations.iter().any(|a| a.is_builtin("nomangle")) {
        return true;
    }
    if f.is_extern {
        return true;
    }
    decorators.is_decorated(f)
}

/// Collect sim roots with the same wire-name rules as CLI dispatch.
pub fn collect_sim_fns(ws: &Workspace) -> Vec<(Ustr, &bone_semantic::hir::Function, String)> {
    let mut sims = Vec::new();
    for module in ws.source_modules() {
        for (_fid, f) in module.functions.iter() {
            let Some(wire) = skull_mir::lifecycle::sim_wire_name(f) else {
                continue;
            };
            sims.push((module.file, f, wire.as_str().to_string()));
        }
    }
    sims
}

/// Shared by the sim roots and `sim_registration_lines`, so the registration
/// call and the retained definition cannot drift.
fn register_sim_fn(ws: &Workspace) -> Option<FuncRef> {
    match register_sim_candidates(ws).as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

fn register_sim_candidates(ws: &Workspace) -> Vec<FuncRef> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.functions
                .iter()
                .filter(|(_, f)| {
                    skull_mir::mangle::declared_name(f.name.as_str()) == "register_sim"
                })
                .map(|(fid, _)| FuncRef {
                    module: m.id,
                    func: fid,
                })
        })
        .collect()
}

/// A one-parameter sim registers as `provider(sim)`; the provider resolves the
/// argument when the runner starts the sim, keeping the engine registry
/// game-agnostic.
fn sim_entry_providers(ws: &Workspace) -> Vec<FuncRef> {
    ws.source_modules()
        .flat_map(|m| {
            m.functions
                .iter()
                .filter(|(_, f)| f.annotations.iter().any(|a| a.is_builtin("sim_entry")))
                .map(|(fid, _)| FuncRef {
                    module: m.id,
                    func: fid,
                })
        })
        .collect()
}

pub(super) fn sim_glue_roots(ws: &Workspace) -> Vec<FuncRef> {
    if collect_sim_fns(ws).is_empty() {
        return Vec::new();
    }
    let mut roots: Vec<FuncRef> = register_sim_fn(ws).into_iter().collect();
    roots.extend(sim_entry_providers(ws));
    roots
}

/// Every function this build roots from, by the emitter's own predicate.
pub fn emission_roots(ws: &Workspace, project_root: Option<&str>) -> Vec<FuncRef> {
    let decorators = DecoratorTable::collect(ws);
    let program = is_program_target(ws, project_root);
    let mut roots = Vec::new();
    for module in ws.source_modules() {
        for (fid, f) in module.functions.iter() {
            if is_emission_root(module.file.as_str(), project_root, program, f, &decorators) {
                roots.push(FuncRef {
                    module: module.id,
                    func: fid,
                });
            }
        }
    }
    roots
}

pub(super) fn explicit_and_global_roots(ws: &Workspace, explicit: &[FuncRef]) -> Vec<FuncRef> {
    let decorators = DecoratorTable::collect(ws);
    let mut roots = explicit.to_vec();
    roots.extend(ws.source_modules().flat_map(|m| {
        m.functions.iter().filter_map(|(fid, f)| {
            is_global_emission_root(f, &decorators).then_some(FuncRef {
                module: m.id,
                func: fid,
            })
        })
    }));
    roots.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.dedup();
    roots
}

pub(super) fn sim_refs_by_wire(ws: &Workspace) -> FxHashMap<String, FuncRef> {
    ws.modules
        .iter()
        .flat_map(|m| {
            m.functions.iter().filter_map(|(fid, f)| {
                skull_mir::lifecycle::sim_wire_name(f).map(|wire| {
                    (
                        wire.as_str().to_owned(),
                        FuncRef {
                            module: m.id,
                            func: fid,
                        },
                    )
                })
            })
        })
        .collect()
}

pub(super) fn named_sim_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    owner: BodyOwner,
    body: &bone_semantic::hir::Body,
    sims: &FxHashMap<String, FuncRef>,
) -> Result<Vec<FuncRef>, CompilerError> {
    use bone_semantic::hir::Expr;
    let mut roots = Vec::new();
    for (_, expr) in body.expr_arena.iter() {
        if let Expr::String(name) = expr {
            if let Some(func) = sims.get(name.as_str()) {
                roots.push(*func);
            }
        }
    }
    // The engine dispatcher consumes the runner's resolved entry plan; other
    // registry reads must name their dependencies statically.
    let engine_dispatch = match owner {
        BodyOwner::Function(fid) => matches!(
            ws.module(mid).functions[fid].name.as_str(),
            "stone::register_sim" | "stone::sim_start_entry" | "stone::clear_all_systems"
        ),
        _ => false,
    };
    if !engine_dispatch {
        for (id, expr) in body.expr_arena.iter() {
            let Expr::Global(name) = expr else { continue };
            if name.as_str() != "stone::SIMS" {
                continue;
            }
            let literal_lookup = body.expr_arena.iter().any(|(_, expr)| {
                let key = match expr {
                    Expr::Index { base, index } if *base == id => Some(*index),
                    Expr::Call { args, family, .. }
                        if skull_mir::mangle::declared_name(family.key().as_str()) == "get" =>
                    {
                        let mut args = *args;
                        if args.next() == Some(id) {
                            args.next()
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                key.is_some_and(|key| matches!(body.expr_arena[key], Expr::String(_)))
            });
            if !literal_lookup {
                return Err(CompilerError::new_with_label(body.expr_spans[id],
                    "scoped sim compile cannot resolve dynamic SIMS access; select the whole suite or use a literal sim name".to_string()));
            }
        }
    }
    Ok(roots)
}

/// A single-file compile (no project root) keeps every function.
fn is_program_target(ws: &Workspace, project_root: Option<&str>) -> bool {
    project_root.is_some()
        && ws.source_modules().any(|m| {
            !is_library_module(m.file.as_str(), project_root)
                && m.functions
                    .iter()
                    .any(|(_, f)| skull_mir::mangle::declared_name(f.name.as_str()) == "main")
        })
}

/// Composed from facts cached by the full build and explicit shader deltas;
/// never asks the workspace for its declarations.
pub(super) fn cached_roots(
    interface_roots: &[FuncRef],
    sim_roots: &[FuncRef],
    shader_roots: &[FuncRef],
) -> Vec<FuncRef> {
    let mut roots = interface_roots.to_vec();
    let mut seen: FxHashSet<FuncRef> = roots.iter().copied().collect();
    let mut glue = sim_roots.to_vec();
    glue.extend(shader_roots.iter().copied());
    glue.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.extend(glue.into_iter().filter(|f| seen.insert(*f)));
    roots
}

/// Full builds only: sims run from a fresh `main.lua` process, never from an
/// overlay.
pub(super) fn sim_registration_lines(
    ws: &Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    selected: Option<&FxHashSet<Ustr>>,
) -> Result<String, CompilerError> {
    let sims = collect_sim_fns(ws)
        .into_iter()
        .filter(|(_, f, _)| selected.is_none_or(|names| names.contains(&f.name)))
        .collect::<Vec<_>>();
    if sims.is_empty() {
        return Ok(String::new());
    }

    let register_fn = register_sim_fn(ws).ok_or_else(|| {
        let candidates = register_sim_candidates(ws);
        if candidates.len() > 1 {
            let names: Vec<&str> = candidates
                .iter()
                .map(|f| ws.module(f.module).functions[f.func].name.as_str())
                .collect();
            return CompilerError::new_no_location(format!(
                "`@sim` registration is ambiguous: `register_sim` is defined as {}",
                names.join(", ")
            ));
        }
        CompilerError::new_no_location(
            "internal: `register_sim` is not in scope yet `@sim` annotations exist \
             (skull) -- stone/engine must be linked. Please report."
                .to_string(),
        )
    })?;
    let register_name = mangle_fn(
        ws,
        &ws.module(register_fn.module).functions[register_fn.func],
    );

    let mut out = String::new();
    for (_file, f, wire) in sims {
        if f.backend == bone_semantic::hir::FnBackend::Js {
            continue;
        }
        let fref = ws
            .modules
            .iter()
            .find_map(|module| {
                module.functions.iter().find_map(|(fid, candidate)| {
                    std::ptr::eq(candidate, f).then_some(FuncRef {
                        module: module.id,
                        func: fid,
                    })
                })
            })
            .ok_or_else(|| {
                CompilerError::new_no_location(format!("cannot locate @sim `{}`", f.name))
            })?;
        let identity = skull_mir::code_identity::CodeIdentity::Named(Instance::base(fref));
        let key = identity.logical_key(ws, scx.interner());
        let body = format!("__rock_code_slot_ref({:?})", key);
        let entry = match f.param_decls.len() {
            0 => body,
            1 => {
                let provider = sim_entry_provider_for(ws, f, &wire)?;
                format!("function() return {}({}) end", provider, body)
            }
            n => {
                return Err(CompilerError::new_with_label(
                    f.span,
                    format!(
                        "`@sim` `{wire}` takes {n} parameters; a sim takes none or one entry \
                         argument supplied by the game's `@sim_entry` provider"
                    ),
                ));
            }
        };
        let _ = writeln!(out, "{}({:?}, {})", register_name, wire, entry);
    }
    Ok(out)
}

/// The provider must be unique and its `body` parameter must take exactly the
/// sim's parameter type.
fn sim_entry_provider_for(
    ws: &Workspace,
    sim: &bone_semantic::hir::Function,
    wire: &str,
) -> Result<String, CompilerError> {
    use skull_mir::monoty::{MonoSubst, MonoTy, convert_decl};
    let providers = sim_entry_providers(ws);
    let provider_ref = match providers.as_slice() {
        [only] => *only,
        [] => {
            return Err(CompilerError::new_with_label(
                sim.span,
                format!(
                    "`@sim` `{wire}` takes an entry argument but no `@sim_entry` provider is \
                     defined; the game declares one \
                     `@sim_entry co fn(body: fn(T) -> Coroutine<Wait>) -> Coroutine<Wait>`"
                ),
            ));
        }
        many => {
            let names: Vec<&str> = many
                .iter()
                .map(|f| ws.module(f.module).functions[f.func].name.as_str())
                .collect();
            return Err(CompilerError::new_no_location(format!(
                "`@sim_entry` is ambiguous: {} are all providers",
                names.join(", ")
            )));
        }
    };
    let provider = &ws.module(provider_ref.module).functions[provider_ref.func];
    let subst = MonoSubst::default();
    let sim_param = convert_decl(ws, sim.param_decls[0].ty, &subst).ok();
    let provider_param = match provider.param_decls.as_slice() {
        [only] => convert_decl(ws, only.ty, &subst).ok(),
        _ => None,
    };
    let accepted = match &provider_param {
        Some(MonoTy::Function { params, .. }) if params.len() == 1 => Some(&params[0]),
        _ => None,
    };
    match (accepted, &sim_param) {
        (Some(accepted), Some(sim_param)) if accepted == sim_param => {}
        _ => {
            return Err(CompilerError::new_with_label(
                sim.span,
                format!(
                    "`@sim` `{wire}` takes `{}`, but the `@sim_entry` provider `{}` supplies `{}`",
                    sim_param
                        .as_ref()
                        .map_or("<unresolved>".to_string(), MonoTy::render),
                    provider.name,
                    accepted.map_or("<not a one-argument body>".to_string(), MonoTy::render),
                ),
            ));
        }
    }
    Ok(mangle_fn(ws, provider))
}
