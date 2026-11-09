//! Emission roots: library filtering, sim discovery and sim registration.

use super::*;

/// Whether `f` must be emitted no matter what the call graph says.
///
/// `@nomangle` is the load-bearing one. It is precisely the annotation for "this
/// fn is called by its ORIGINAL name from outside the MIR call graph" (CLAUDE.md:
/// from another `inline(lua)` body, from compiler codegen, or from itself), so
/// rooting all of them covers every invisible edge class at once -- including the
/// the string-keyed `on_loop_jump` dispatch site, whose targets appear only as
/// string literals. CodeRef-backed `@on_reload` values are rooted by their
/// decorator application. Measured cost on an application: 24 fns / 7.2 KB, 0.3% of the
/// chunk. Sound beats clever at that price.
///
/// The rest name a fn from generated top-level text rather than from a body:
/// `main` (the CLI appends `__rock_main_wrapper(main)`), a decorated fn (its
/// mangled name goes in a `decorator_application_lines` call), `@sim`
/// (`sim_registration_lines`), and `@test` (the harness calls it bare).
/// Whether a module is a LIBRARY -- something the compilation target consumes --
/// rather than part of the target itself.
///
/// The distinction is what makes whole-program DCE both safe and worthwhile. A
/// library is prunable: nothing outside it depends on a definition no root
/// reaches, and libraries are where the dead weight is (a game chunk is mostly
/// stdlib + engine). The target's OWN sources are roots, because compiling a unit
/// and getting its functions is a real contract here -- codegen tests compile a
/// fragment with no entry point and expect its fns, and `rock compile foo.rock`
/// on a file of helpers should not emit an empty program.
///
/// The stdlib is always a library. Beyond that the caller supplies the target's
/// root directory: a module whose file lives outside it came from a declared
/// dependency (a host library via stick.toml). `None` means a single-file compile, whose
/// only library is the stdlib.
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
    f: &bone_semantic::hir::Function,
    decorators: &DecoratorTable,
) -> bool {
    if !is_library_module(file, project_root) {
        return true;
    }
    if f.name.as_str() == "main" {
        return true;
    }
    let has = |n: &str| f.annotations.iter().any(|a| a.is_builtin(n));
    if has("sim") || has("test") {
        return true;
    }
    is_global_emission_root(f, decorators)
}

fn is_global_emission_root(f: &bone_semantic::hir::Function, decorators: &DecoratorTable) -> bool {
    if f.json_field_for.is_some() || f.annotations.iter().any(|a| a.is_builtin("nomangle")) {
        return true;
    }
    // `extern` decls bind a bare name an inline(lua) body may call directly.
    if f.is_extern {
        return true;
    }
    decorators.is_decorated(f)
}

/// Every `@sim`-annotated fn in the workspace, paired with its module file and
/// wire name (the `@sim("name")` string arg, else the fn name). The SINGLE
/// source of the `@sim` walk + wire derivation, shared by the codegen registry
/// ([`sim_registration_lines`]) and the rock-cli sim runner
/// (`collect_skull_sims`) so the registered name and the name the runner asks
/// to dispatch cannot drift.
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

/// `register_sim("<wire>", <mangled_fn>)` for every `@sim`-annotated fn,
/// workspace-wide.
/// Emitted at module-init time (after fn + global defs) so a skull-compiled game
/// registers its sims and the runtime's `ROCK_RUN_SIM=<wire>` dispatch resolves.
///
/// `register_sim` is itself a regular Rock fn (the host simulation registry), so it
/// is mangled the same way as its definition (every fn mangles by name +
/// signature, so definition and call agree). Every `@sim` implies the host engine
/// is linked, so a missing
/// `register_sim` is an internal error rather than a silent nil-call (mirrors
/// rock's discipline).
///
/// Full-emit only: sims run from a fresh `main.lua` process via `ROCK_RUN_SIM`,
/// never from the live dev game's overlay, so `assemble_overlay` needs none.
/// The engine's `register_sim`, which [`sim_registration_lines`] calls by MANGLED
/// name from generated top-level text. Shared with the reachability roots so the
/// registration and the root cannot drift: emitting the call while DCE prunes the
/// definition is a nil-call at load, which is exactly what happened.
///
/// Matched by DECLARED name: under modules v1 a function's name is its DefKey.
/// More than one candidate is ambiguous and yields `None`;
/// [`sim_registration_lines`] reports the candidates.
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

/// The game's `@sim_entry` providers. A provider is how a game hands a sim its
/// entry argument while the engine's registry stays game-agnostic: a sim that
/// takes one parameter is registered as `provider(sim)`, and the provider
/// (a `co fn(body: fn(P) -> Coroutine<Wait>) -> Coroutine<Wait>`) resolves the
/// argument when the runner starts the sim and awaits the body with it.
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

/// Functions the generated sim registration text names: `register_sim` and the
/// `@sim_entry` providers. Empty when the build has no sims.
pub(super) fn sim_glue_roots(ws: &Workspace) -> Vec<FuncRef> {
    if collect_sim_fns(ws).is_empty() {
        return Vec::new();
    }
    let mut roots: Vec<FuncRef> = register_sim_fn(ws).into_iter().collect();
    roots.extend(sim_entry_providers(ws));
    roots
}

/// Every function this build roots from, by the emitter's own predicate.
///
/// It lives in the DRIVER, not in `skull_graph`: which declarations a build
/// roots from is a production decision (project layout, decorators, `@sim`,
/// `@test`), and the graph takes roots as an INPUT precisely so it cannot make
/// that decision itself. See [`all_roots`], which adds the generated-glue
/// obligations on top.
pub fn emission_roots(ws: &Workspace, project_root: Option<&str>) -> Vec<FuncRef> {
    emission_roots_with_census(ws, project_root, None)
}

/// Harness roots plus the global roots every build keeps. Package tests and
/// selected sim receipts have no public interface to root wholesale.
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
    // The engine dispatcher consumes the runner's resolved entry plan. Other
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

fn emission_roots_with_census(
    ws: &Workspace,
    project_root: Option<&str>,
    mut census: Option<&mut RootEnumerationCensus>,
) -> Vec<FuncRef> {
    let decorators = DecoratorTable::collect(ws);
    let mut roots = Vec::new();
    for module in ws.source_modules() {
        if let Some(census) = census.as_deref_mut() {
            census.modules_examined += 1;
            census.functions_examined += module.functions.len();
        }
        for (fid, f) in module.functions.iter() {
            if is_emission_root(module.file.as_str(), project_root, f, &decorators) {
                roots.push(FuncRef {
                    module: module.id,
                    func: fid,
                });
            }
        }
    }
    roots
}

/// The roots one Lua build is taken over: the declared emission roots plus the
/// obligations GENERATED GLUE TEXT carries.
///
/// `sim_registration_lines` writes `register_sim("<wire>", <mangled>)` and
/// `shader_registration_lines` writes a program whose stage entries and GLSL
/// helpers are named by its graph-owned view. Neither is a Rock call site, so
/// no amount of ordinary graph reachability can find them -- they are an
/// artifact ROOT (plan S 11: "the target may change roots"), which is what
/// `ArtifactRequest::roots` is for. Emitting the generated call while the
/// definition is pruned is a nil call at load, which is exactly what happened.
///
/// The shader collection is therefore built BEFORE the graph, not beside the
/// assembly that renders it.
#[cfg(test)]
pub(super) fn all_roots(
    ws: &Workspace,
    project_root: Option<&str>,
    shader_roots: &[FuncRef],
) -> Vec<FuncRef> {
    all_roots_with_census(ws, project_root, shader_roots, None)
}

pub(super) fn all_roots_with_census(
    ws: &Workspace,
    project_root: Option<&str>,
    shader_roots: &[FuncRef],
    mut census: Option<&mut RootEnumerationCensus>,
) -> Vec<FuncRef> {
    let mut roots = emission_roots_with_census(ws, project_root, census.as_deref_mut());
    let mut seen: FxHashSet<FuncRef> = roots.iter().copied().collect();
    let mut glue: Vec<FuncRef> = sim_glue_roots(ws);
    glue.extend(shader_roots.iter().copied());
    // Deterministic: `dependencies` is a hash set, so its iteration order is not.
    glue.sort_by_key(|f| (f.module.index(), f.func.into_raw()));
    roots.extend(glue.into_iter().filter(|f| seen.insert(*f)));
    if let Some(census) = census {
        census.roots_reseeded += roots.len();
    }
    roots
}

/// Compose the retained root set from facts cached by full construction and
/// explicit body-local shader deltas. Unlike [`all_roots`], this never asks the
/// workspace for its declarations or sim functions.
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

#[derive(Default)]
pub(super) struct RootEnumerationCensus {
    pub(super) modules_examined: usize,
    pub(super) functions_examined: usize,
    pub(super) roots_reseeded: usize,
}

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
                // The provider is called when the runner STARTS the sim, so the
                // argument is resolved against the world live at that moment.
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

/// The mangled name of the `@sim_entry` provider that supplies `sim`'s single
/// parameter. The provider must be unique and its `body` parameter must take
/// exactly the sim's parameter type, so a sim cannot be started with an
/// argument of the wrong type.
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
