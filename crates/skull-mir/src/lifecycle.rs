use bone_semantic::hir::{AnnoValue, BodyOwner, Expr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{CompilerError, Idx, ModuleId};
use ustr::Ustr;

use crate::code_identity::{CodeIdentity, CodeOwner, CodeOwnerKind};
use crate::code_identity::CodeArtifact;
use crate::instance::Instance;
use crate::mir::FuncRef;
use crate::monoty::{convert_decl, MonoCtor, MonoSubst, MonoTy};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleDependency {
    pub identity: CodeIdentity,
    pub fingerprint: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleQualification {
    Plain,
    OnChange { dependencies: Vec<LifecycleDependency> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleReplayRole {
    Presentation,
    State,
}

impl LifecycleReplayRole {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Presentation => "presentation",
            Self::State => "state",
        }
    }
}

impl LifecycleQualification {
    pub fn is_gated(&self) -> bool {
        matches!(self, Self::OnChange { .. })
    }

    pub fn dependencies(&self) -> &[LifecycleDependency] {
        match self {
            Self::Plain => &[],
            Self::OnChange { dependencies } => dependencies,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleEntry {
    pub phase: Ustr,
    pub owner: Ustr,
    pub root_type: Option<MonoTy>,
    pub identity: CodeIdentity,
    pub diagnostic_name: Ustr,
    pub stage: i64,
    pub replay_role: LifecycleReplayRole,
    pub declaration_ordinal: usize,
    pub qualification: LifecycleQualification,
    pub sim_wires: Vec<Ustr>,
    pub body_fingerprint: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecycleCensus {
    pub declarations: usize,
    pub applications: usize,
    pub admitted: usize,
    pub session_owned: usize,
    pub scene_owned: usize,
    pub plain: usize,
    pub gated: usize,
    pub dependency_edges: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecyclePlan {
    pub entries: Vec<LifecycleEntry>,
    pub census: LifecycleCensus,
}

impl LifecyclePlan {
    pub fn structure_changed(&self, next: &Self) -> bool {
        if self.entries.len() != next.entries.len() {
            return true;
        }
        self.entries.iter().zip(&next.entries).any(|(old, candidate)| {
            old.phase != candidate.phase
                || old.owner != candidate.owner
                || old.root_type != candidate.root_type
                || old.identity != candidate.identity
                || old.stage != candidate.stage
                || old.replay_role != candidate.replay_role
                || old.declaration_ordinal != candidate.declaration_ordinal
                || old.sim_wires != candidate.sim_wires
                || qualification_structure_changed(&old.qualification, &candidate.qualification)
        })
    }

    pub fn removed_gated_wire(&self, next: &Self) -> bool {
        self.entries.iter().any(|old| {
            old.phase == "wire"
                && old.qualification.is_gated()
                && !next.entries.iter().any(|candidate| candidate.identity == old.identity)
        })
    }
}

fn qualification_structure_changed(
    old: &LifecycleQualification,
    candidate: &LifecycleQualification,
) -> bool {
    match (old, candidate) {
        (LifecycleQualification::Plain, LifecycleQualification::Plain) => false,
        (
            LifecycleQualification::OnChange { dependencies: old },
            LifecycleQualification::OnChange { dependencies: candidate },
        ) => old.iter().map(|dependency| &dependency.identity).ne(
            candidate.iter().map(|dependency| &dependency.identity),
        ),
        _ => true,
    }
}

impl LifecycleQualification {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::OnChange { .. } => "onchange",
        }
    }
}

pub const LIFECYCLE_PHASES: [&str; 12] = [
    "early_init",
    "init",
    "late_init",
    "wire",
    "update",
    "late_update",
    "draw",
    "late_draw",
    "post_imgui_draw",
    "reset",
    "shutdown",
    "window_resized",
];

fn phase_rank(phase: Ustr) -> Option<u8> {
    LIFECYCLE_PHASES
        .iter()
        .position(|candidate| *candidate == phase.as_str())
        .map(|rank| rank as u8)
}

pub fn sim_wire_name(function: &bone_semantic::hir::Function) -> Option<Ustr> {
    let annotation = function
        .annotations
        .iter()
        .find(|annotation| annotation.is_builtin("sim"))?;
    Some(match annotation.args.first().map(|argument| &argument.value) {
        Some(AnnoValue::String(name)) => *name,
        _ => function.name,
    })
}

fn is_sim_fixture_module(file: &str) -> bool {
    let mut previous = "";
    for part in file.split(['/', '\\']) {
        if previous == "tests" && part == "sims" {
            return true;
        }
        previous = part;
    }
    false
}

fn is_frame_phase(phase: Ustr) -> bool {
    matches!(
        phase.as_str(),
        "update" | "late_update" | "draw" | "late_draw" | "post_imgui_draw"
    )
}

/// The one emitted identity for a validated typed Scene lifecycle root. The
/// same canonical `MonoTy` encoding is embedded by `scene_create<T>`.
pub fn scene_root_type_key(root_type: &MonoTy) -> Option<String> {
    match root_type {
        MonoTy::App {
            ctor: MonoCtor::Nominal(_),
            params,
        } if params.is_empty() => Some(root_type.canonical()),
        _ => None,
    }
}

fn string_arg(ann: &bone_semantic::hir::Annotation, name: &str) -> Option<Ustr> {
    ann.args.iter().find_map(|a| {
        (a.name.as_ref().is_some_and(|n| n.name.as_str() == name))
            .then(|| match a.value {
                AnnoValue::String(s) => Some(s),
                _ => None,
            })
            .flatten()
    })
}

fn int_expr(body: &bone_semantic::hir::Body, e: Idx<bone_semantic::hir::Expr>) -> Option<i64> {
    match body.expr_arena[e] {
        Expr::Int(n) => Some(n),
        Expr::UnaryMinus(inner, _) => int_expr(body, inner)?.checked_neg(),
        _ => None,
    }
}

fn string_expr(body: &bone_semantic::hir::Body, e: Idx<bone_semantic::hir::Expr>) -> Option<Ustr> {
    match body.expr_arena[e] {
        Expr::String(value) => Some(value),
        _ => None,
    }
}

fn lifecycle_replay_role(
    ws: &Workspace,
    dmid: ModuleId,
    did: Idx<bone_semantic::hir::DecoratorDef>,
    app: &bone_semantic::hir::Annotation,
    replay_param: Option<Ustr>,
) -> Result<LifecycleReplayRole, CompilerError> {
    let Some(param_name) = replay_param else {
        return Ok(LifecycleReplayRole::Presentation);
    };
    let deco = &ws.module(dmid).decorators[did];
    let index = deco
        .param_decls
        .iter()
        .position(|param| param.name == param_name)
        .ok_or_else(|| {
            CompilerError::new_with_label(
                app.span,
                format!(
                    "lifecycle replay_param `{param_name}` is not a parameter of `{}`",
                    deco.name
                ),
            )
        })?;
    let value = if let Some(arg) = app
        .args
        .iter()
        .find(|arg| arg.name.as_ref().is_some_and(|name| name.name == param_name))
    {
        match arg.value {
            AnnoValue::String(value) => value,
            _ => {
                return Err(CompilerError::new_with_label(
                    app.span,
                    format!("lifecycle replay role `{param_name}` must be a string literal"),
                ));
            }
        }
    } else {
        let body = &ws.module(dmid).bodies[&BodyOwner::Decorator(did)];
        let e = body.param_defaults.get(index).and_then(|e| *e).ok_or_else(|| {
            CompilerError::new_with_label(
                app.span,
                format!("lifecycle replay role `{param_name}` has no default"),
            )
        })?;
        string_expr(body, e).ok_or_else(|| {
            CompilerError::new_with_label(
                deco.span,
                format!("lifecycle replay role default `{param_name}` must be a string literal"),
            )
        })?
    };
    match value.as_str() {
        "presentation" => Ok(LifecycleReplayRole::Presentation),
        "state" => Ok(LifecycleReplayRole::State),
        _ => Err(CompilerError::new_with_label(
            app.span,
            format!("unsupported lifecycle replay role `{value}`; expected `presentation` or `state`"),
        )),
    }
}

fn global_identity(
    ws: &Workspace,
    module: ModuleId,
    global: Idx<bone_semantic::hir::GlobalDef>,
) -> CodeIdentity {
    CodeIdentity::Artifact(CodeOwner::Artifact {
        kind: CodeOwnerKind::Global,
        module: ws.module(module).file,
        name: ws.module(module).globals[global].name,
    })
}

fn global_locations(
    ws: &Workspace,
    name: Ustr,
) -> Vec<(ModuleId, Idx<bone_semantic::hir::GlobalDef>)> {
    ws.modules
        .iter()
        .filter_map(|module| module.global_by_name.get(&name).copied().map(|gid| (module.id, gid)))
        .collect()
}

fn user_function_named(ws: &Workspace, name: Ustr) -> bool {
    ws.functions_by_name.get(&name).is_some_and(|functions| {
        functions
            .iter()
            .any(|(module, _)| *module != ModuleId::builtin())
    })
}

fn ordered_global_locations(
    ws: &Workspace,
    names: impl IntoIterator<Item = Ustr>,
) -> Result<Vec<(Ustr, ModuleId, Idx<bone_semantic::hir::GlobalDef>)>, CompilerError> {
    let mut out = Vec::new();
    for name in names {
        let locations = global_locations(ws, name);
        match locations.as_slice() {
            [] => continue,
            [(module, global)] => out.push((name, *module, *global)),
            _ => {
                return Err(CompilerError::new_no_location(format!(
                    "@wire dependency `{name}` has an ambiguous global identity"
                )));
            }
        }
    }
    out.sort_by_key(|(_, module, global)| (module.index(), u32::from(global.into_raw())));
    Ok(out)
}

fn wire_dependency_identities(
    ws: &Workspace,
    names: &[skull_parser::Ident],
    resolved_names: Option<&[Ustr]>,
    span: bone_semantic::prelude::Span,
) -> Result<Vec<CodeIdentity>, CompilerError> {
    let mut seen = std::collections::HashSet::new();
    let mut identities = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let key = resolved_names
            .and_then(|resolved| resolved.get(index).copied())
            .unwrap_or(name.name);
        if !seen.insert(key) {
            return Err(CompilerError::new_with_label(
                span,
                format!("@wire(onchange=...) repeats dependency `{}`", name.name),
            ));
        }
        let locations = global_locations(ws, key);
        if !locations.is_empty() && user_function_named(ws, key) {
            return Err(CompilerError::new_with_label(
                name.span,
                format!("@wire dependency `{}` has an ambiguous identity", name.name),
            ));
        }
        let (module, global) = match locations.as_slice() {
            [] => {
                if user_function_named(ws, key) {
                    return Err(CompilerError::new_with_label(
                        name.span,
                        format!("@wire dependency `{}` names a function; expected a top-level const", name.name),
                    ));
                }
                return Err(CompilerError::new_with_label(
                    name.span,
                    format!("@wire dependency `{}` is unknown", name.name),
                ));
            }
            [(module, global)] => (*module, *global),
            _ => {
                return Err(CompilerError::new_with_label(
                    name.span,
                    format!("@wire dependency `{}` has an ambiguous identity", name.name),
                ));
            }
        };
        if !ws.module(module).globals[global].is_const {
            return Err(CompilerError::new_with_label(
                name.span,
                format!("@wire dependency `{}` is mutable; only top-level const is supported", name.name),
            ));
        }
        identities.push(global_identity(ws, module, global));
    }
    if identities.is_empty() {
        return Err(CompilerError::new_with_label(
            span,
            "@wire(onchange=...) requires a non-empty dependency list".to_string(),
        ));
    }
    Ok(identities)
}

fn dependency_names_for_const(
    ws: &Workspace,
    module: ModuleId,
    global: Idx<bone_semantic::hir::GlobalDef>,
) -> Result<Vec<(Ustr, ModuleId, Idx<bone_semantic::hir::GlobalDef>)>, CompilerError> {
    let Some(inference) = ws
        .inference(module)
        .and_then(|inference| inference.per_fn.get(&BodyOwner::Global(global)))
    else {
        return Err(CompilerError::new_no_location(format!(
            "@wire cannot fingerprint const `{}` because its initializer has no dependency record",
            ws.module(module).globals[global].name
        )));
    };
    let dependencies = ordered_global_locations(ws, inference.deps.iter().copied())?;
    for (name, dependency_module, dependency_global) in &dependencies {
        if !ws.module(*dependency_module).globals[*dependency_global].is_const {
            return Err(CompilerError::new_no_location(format!(
                "@wire const closure for `{}` reaches mutable global `{name}`",
                ws.module(module).globals[global].name
            )));
        }
    }
    for name in inference.deps.iter().copied() {
        let locations = global_locations(ws, name);
        let user_function = user_function_named(ws, name);
        if !locations.is_empty() && user_function {
            return Err(CompilerError::new_no_location(format!(
                "@wire const closure for `{}` reaches ambiguous identity `{name}`",
                ws.module(module).globals[global].name
            )));
        }
        if locations.is_empty()
            && user_function
            && ws.functions_by_name.get(&name).is_some_and(|functions| {
                functions.iter().any(|(module, function)| {
                    *module != ModuleId::builtin()
                        && bone_semantic::hir::operator_entry(crate::mangle::declared_name(name.as_str())).is_none()
                        && !crate::intrinsics::is_intrinsic(&ws.module(*module).functions[*function])
                })
            })
        {
            return Err(CompilerError::new_no_location(format!(
                "@wire const closure for `{}` reaches function `{name}`, whose value is not a const/comptime identity",
                ws.module(module).globals[global].name
            )));
        }
    }
    Ok(dependencies)
}

fn dependency_closure(
    ws: &Workspace,
    identities: &[CodeIdentity],
) -> Result<Vec<CodeIdentity>, CompilerError> {
    let mut locations = std::collections::HashMap::new();
    for module in ws.modules.iter() {
        for (global, definition) in module.globals.iter() {
            if definition.is_const {
                locations.insert(definition.name, (module.id, global));
            }
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut pending = std::collections::VecDeque::from(identities.to_vec());
    while let Some(identity) = pending.pop_front() {
        if !seen.insert(identity.clone()) {
            continue;
        }
        out.push(identity.clone());
        let CodeIdentity::Artifact(CodeOwner::Artifact { kind: CodeOwnerKind::Global, name, .. }) = identity else {
            continue;
        };
        let Some(&(module, global)) = locations.get(&name) else {
            return Err(CompilerError::new_no_location(format!(
                "@wire dependency `{name}` has no compiler-owned const declaration"
            )));
        };
        let dependencies = dependency_names_for_const(ws, module, global)?;
        for (_, dependency_module, dependency_global) in dependencies {
            pending.push_back(global_identity(ws, dependency_module, dependency_global));
        }
    }
    Ok(out)
}

fn wire_qualification(
    ws: &Workspace,
    app: &bone_semantic::hir::Annotation,
    phase: Ustr,
) -> Result<LifecycleQualification, CompilerError> {
    // `wire` is a compiler-owned lifecycle role, not a globally reserved
    // annotation spelling. Its qualification follows the admitted role's
    // metadata, so a v1 alias keeps the behavior while an unrelated decorator
    // named `wire` does not.
    if phase != "wire" {
        return Ok(LifecycleQualification::Plain);
    }
    if app.args.is_empty() {
        return Ok(LifecycleQualification::Plain);
    }
    if app.args.len() != 1
        || app.args[0]
            .name
            .as_ref()
            .is_none_or(|name| name.name.as_str() != "onchange")
    {
        return Err(CompilerError::new_with_label(
            app.span,
            "@wire accepts either no arguments or one named onchange=[CONST, ...] argument".to_string(),
        ));
    }
    let names = match &app.args[0].value {
        AnnoValue::IdentList(names) => names,
        _ => {
            return Err(CompilerError::new_with_label(
                app.span,
                "@wire(onchange=...) requires a structured identifier list".to_string(),
            ));
        }
    };
    let declared = wire_dependency_identities(ws, names, app.resolved_ident_list.as_deref(), app.span)?;
    let dependencies = dependency_closure(ws, &declared)?
        .into_iter()
        .map(|identity| LifecycleDependency { identity, fingerprint: 0 })
        .collect();
    Ok(LifecycleQualification::OnChange { dependencies })
}

pub fn attach_artifact(
    mut plan: LifecyclePlan,
    artifact: &CodeArtifact,
) -> Result<LifecyclePlan, CompilerError> {
    for entry in &mut plan.entries {
        let Some(fingerprint) = artifact.body_fingerprint(&entry.identity) else {
            if entry.phase == "wire" {
                return Err(CompilerError::new_no_location(format!(
                    "LifecyclePlan has no emitted body fingerprint for wire `{}`",
                    entry.diagnostic_name
                )));
            }
            continue;
        };
        if entry.phase == "wire" && fingerprint == 0 {
            return Err(CompilerError::new_no_location(format!(
                "LifecyclePlan has no canonical body fingerprint for wire `{}`",
                entry.diagnostic_name
            )));
        }
        entry.body_fingerprint = fingerprint;
        if let LifecycleQualification::OnChange { dependencies } = &mut entry.qualification {
            for dependency in dependencies {
                let Some(fingerprint) = artifact.body_fingerprint(&dependency.identity) else {
                    return Err(CompilerError::new_no_location(format!(
                        "LifecyclePlan has no canonical fingerprint for dependency of wire `{}`",
                        entry.diagnostic_name
                    )));
                };
                if fingerprint == 0 {
                    return Err(CompilerError::new_no_location(format!(
                        "LifecyclePlan has no canonical fingerprint for dependency of wire `{}`",
                        entry.diagnostic_name
                    )));
                }
                dependency.fingerprint = fingerprint;
            }
        }
    }
    Ok(plan)
}

pub fn collect_with_artifact(
    ws: &Workspace,
    artifact: &CodeArtifact,
) -> Result<LifecyclePlan, CompilerError> {
    attach_artifact(collect(ws)?, artifact)
}

pub fn collect(ws: &Workspace) -> Result<LifecyclePlan, CompilerError> {
    let sim_wires_by_module = ws
        .modules
        .iter()
        .filter(|module| {
            module.id != ModuleId::builtin() && is_sim_fixture_module(module.file.as_str())
        })
        .filter_map(|module| {
            let wires = module
                .functions
                .values()
                .filter_map(sim_wire_name)
                .collect::<Vec<_>>();
            (!wires.is_empty()).then_some((module.id, wires))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut roles = std::collections::HashMap::new();
    let mut census = LifecycleCensus::default();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (did, deco) in module.decorators.iter() {
            let Some(meta) = deco
                .annotations
                .iter()
                .find(|a| a.is_builtin("lifecycle"))
            else {
                continue;
            };
            census.declarations += 1;
            let phase = string_arg(meta, "phase").expect("validated lifecycle phase");
            if phase_rank(phase).is_none() {
                return Err(CompilerError::new_with_label(
                    meta.span,
                    format!("unsupported lifecycle phase `{phase}`"),
                ));
            }
            let owner = string_arg(meta, "owner").expect("validated lifecycle owner");
            if owner.as_str() != "session" && owner.as_str() != "scene" {
                return Err(CompilerError::new_with_label(
                    meta.span,
                    format!("unsupported lifecycle owner `{owner}`; expected `session` or `scene`"),
                ));
            }
            let stage_param = string_arg(meta, "stage_param");
            let replay_param = string_arg(meta, "replay_param");
            let stage_index = match stage_param {
                Some(name) => Some(
                    deco.param_decls
                        .iter()
                        .position(|p| p.name == name)
                        .ok_or_else(|| {
                            CompilerError::new_with_label(
                                meta.span,
                                format!(
                                    "lifecycle stage_param `{name}` is not a parameter of `{}`",
                                    deco.name
                                ),
                            )
                        })?,
                ),
                None if deco.param_decls.is_empty() => None,
                None => {
                    return Err(CompilerError::new_with_label(
                        meta.span,
                        format!(
                            "lifecycle decorator `{}` has parameters but no stage_param",
                            deco.name
                        ),
                    ));
                }
            };
            roles.insert(
                deco.def_key,
                (module.id, did, phase, owner, stage_index, replay_param),
            );
        }
    }

    let mut entries = Vec::new();
    let mut ordinal = 0usize;
    let mut seen = std::collections::HashSet::<(CodeIdentity, Ustr)>::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, f) in module.functions.iter() {
            for app in &f.annotations {
                let app_key = app.resolved_name.unwrap_or(app.name.name);
                let Some(&(dmid, did, phase, owner, stage_index, replay_param)) = roles.get(&app_key)
                else {
                    continue;
                };
                let identity = CodeIdentity::Named(Instance::base(FuncRef {
                    module: module.id,
                    func: fid,
                }));
                if !seen.insert((identity.clone(), app_key)) {
                    continue;
                }
                census.applications += 1;
                let root_type = match owner.as_str() {
                    "session" => {
                        if !f.generics.is_empty() || !f.param_decls.is_empty() || f.return_ty.is_some() {
                            return Err(CompilerError::new_with_label(
                                f.span,
                                format!(
                                    "session lifecycle target `{}` must be concrete, non-generic, zero-argument, and unit-returning",
                                    f.name
                                ),
                            ));
                        }
                        census.session_owned += 1;
                        None
                    }
                    "scene" => {
                        if phase.as_str() != "update" {
                            return Err(CompilerError::new_with_label(
                                app.span,
                                format!(
                                    "scene lifecycle target `{}` uses unsupported phase `{phase}`; expected `update`",
                                    f.name
                                ),
                            ));
                        }
                        if !f.generics.is_empty() || f.param_decls.len() != 1 || f.return_ty.is_some() {
                            return Err(CompilerError::new_with_label(
                                f.span,
                                format!(
                                    "scene lifecycle target `{}` must be concrete, non-generic, take exactly one reference-struct root, and return unit",
                                    f.name
                                ),
                            ));
                        }
                        let root = convert_decl(
                            ws,
                            f.param_decls[0].ty,
                            &MonoSubst::default(),
                        )
                        .map_err(|reason| {
                            CompilerError::new_with_label(
                                f.span,
                                format!(
                                    "scene lifecycle target `{}` has a non-concrete root type ({})",
                                    f.name,
                                    reason.bucket(),
                                ),
                            )
                        })?;
                        let MonoTy::App {
                            ctor: MonoCtor::Nominal(root_name),
                            params,
                        } = &root
                        else {
                            return Err(CompilerError::new_with_label(
                                f.span,
                                format!(
                                    "scene lifecycle target `{}` root must be a reference struct, got `{}`",
                                    f.name,
                                    root.render(),
                                ),
                            ));
                        };
                        let Some(struct_id) = ws.struct_by_name.get(root_name) else {
                            return Err(CompilerError::new_with_label(
                                f.span,
                                format!(
                                    "scene lifecycle target `{}` root `{}` is not a struct",
                                    f.name, root_name,
                                ),
                            ));
                        };
                        let definition = &ws.module(struct_id.module).structs[struct_id.def];
                        if definition.is_value || !params.is_empty() {
                            return Err(CompilerError::new_with_label(
                                f.span,
                                format!(
                                    "scene lifecycle target `{}` root must be a non-generic reference struct, got `{}`",
                                    f.name,
                                    root.render(),
                                ),
                            ));
                        }
                        census.scene_owned += 1;
                        Some(root)
                    }
                    _ => unreachable!("validated lifecycle owner"),
                };
                if f.backend != bone_semantic::hir::FnBackend::Any {
                    return Err(CompilerError::new_with_label(
                        f.span,
                        format!(
                            "lifecycle target `{}` must have a backend-neutral Rock body",
                            f.name
                        ),
                    ));
                }
                let deco = &ws.module(dmid).decorators[did];
                let stage = if let Some(i) = stage_index {
                    let pname = deco.param_decls[i].name;
                    if let Some(arg) = app
                        .args
                        .iter()
                        .find(|a| a.name.as_ref().is_some_and(|n| n.name == pname))
                    {
                        match arg.value {
                            AnnoValue::Int(n) => n,
                            _ => {
                                return Err(CompilerError::new_with_label(
                                    app.span,
                                    format!("lifecycle stage `{pname}` must be an integer literal"),
                                ));
                            }
                        }
                    } else {
                        let body = &ws.module(dmid).bodies[&BodyOwner::Decorator(did)];
                        let e = body.param_defaults.get(i).and_then(|e| *e).ok_or_else(|| {
                            CompilerError::new_with_label(
                                app.span,
                                format!("lifecycle stage `{pname}` has no default"),
                            )
                        })?;
                        int_expr(body, e).ok_or_else(|| {
                            CompilerError::new_with_label(
                                deco.span,
                                format!(
                                    "lifecycle stage default `{pname}` must be an integer literal"
                                ),
                            )
                        })?
                    }
                } else {
                    0
                };
                let replay_role = lifecycle_replay_role(ws, dmid, did, app, replay_param)?;
                let qualification = wire_qualification(ws, app, phase)?;
                if qualification.is_gated() {
                    census.gated += 1;
                    census.dependency_edges += qualification.dependencies().len();
                } else {
                    census.plain += 1;
                }
                entries.push(LifecycleEntry {
                    phase,
                    owner,
                    root_type,
                    identity,
                    diagnostic_name: f.name,
                    stage,
                    replay_role,
                    declaration_ordinal: ordinal,
                    qualification,
                    sim_wires: if is_frame_phase(phase) {
                        sim_wires_by_module.get(&module.id).cloned().unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    body_fingerprint: 0,
                });
                ordinal += 1;
                census.admitted += 1;
            }
        }
    }
    entries.sort_by_key(|e| {
        (
            phase_rank(e.phase).expect("validated phase"),
            e.stage,
            e.declaration_ordinal,
        )
    });
    Ok(LifecyclePlan { entries, census })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::semantic_passes::semantic_pass;

    fn workspace(sources: &[(&str, &str)]) -> Workspace {
        let mut modules = sources
            .iter()
            .map(|(file, src)| {
                skull_parser::parse_module_clean(src, Ustr::from(*file)).expect("parse")
            })
            .collect::<Vec<_>>();
        semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn plan_reads_stage_defaults_and_orders_equal_stage_by_declaration() {
        let ws = workspace(&[(
            "plan.rock",
            r#"
            @lifecycle(phase="update", owner="session", stage_param="stage")
            deco init tick(stage: int = 100) {}
            @tick(stage=20) fn later() {}
            @tick(stage=10) fn first() {}
            @tick(stage=10) fn second() {}
            @tick fn defaulted() {}
        "#,
        )]);
        let plan = collect(&ws).expect("plan");
        assert_eq!(plan.census.declarations, 1);
        assert_eq!(plan.census.applications, 4);
        assert_eq!(plan.census.admitted, 4);
        assert_eq!(
            plan.entries
                .iter()
                .map(|e| (e.diagnostic_name.as_str(), e.stage))
                .collect::<Vec<_>>(),
            vec![
                ("first", 10),
                ("second", 10),
                ("later", 20),
                ("defaulted", 100)
            ]
        );
        assert!(plan.entries[0].declaration_ordinal < plan.entries[1].declaration_ordinal);
    }

    #[test]
    fn qualified_wire_keeps_a_structured_identifier_list() {
        let ws = workspace(&[(
            "wire.rock",
            r#"
            @lifecycle(phase="wire", owner="session")
            deco init wire {}
            const GAME_VOLUME: int = 1
            const ART_FILTER: int = 2
            @wire(onchange=[GAME_VOLUME, ART_FILTER])
            fn apply_game_config() {}
            "#,
        )]);
        let function = ws
            .modules
            .iter()
            .flat_map(|module| module.functions.values())
            .find(|function| function.name.as_str() == "apply_game_config")
            .expect("qualified wire function");
        let annotation = function
            .annotations
            .iter()
            .find(|annotation| annotation.is_builtin("wire"))
            .expect("wire annotation");
        let argument = annotation
            .args
            .iter()
            .find(|argument| argument.name.as_ref().is_some_and(|name| name.name.as_str() == "onchange"))
            .expect("onchange argument");
        match &argument.value {
            AnnoValue::IdentList(names) => {
                assert_eq!(names.iter().map(|name| name.name.as_str()).collect::<Vec<_>>(), vec!["GAME_VOLUME", "ART_FILTER"]);
            }
            other => panic!("qualified wire list was lowered as {other:?}"),
        }
    }

    #[test]
    fn qualified_wire_collects_declared_and_transitive_const_dependencies() {
        let ws = workspace(&[(
            "wire.rock",
            r#"
            @lifecycle(phase="wire", owner="session")
            deco init wire {}
            const BASE: int = 1
            const DERIVED: int = BASE
            const OTHER: int = 9
            @wire(onchange=[DERIVED])
            fn apply_game_config() {}
            "#,
        )]);
        let plan = collect(&ws).expect("lifecycle plan");
        let entry = plan
            .entries
            .iter()
            .find(|entry| entry.diagnostic_name.as_str() == "apply_game_config")
            .expect("qualified wire entry");
        assert!(entry.qualification.is_gated());
        assert_eq!(plan.census.gated, 1);
        assert_eq!(plan.census.plain, 0);
        assert_eq!(entry.qualification.dependencies().len(), 2);
    }

    fn wire_error(annotation: &str) -> String {
        let ws = workspace(&[(
            "wire.rock",
            &format!(
                r#"
                @lifecycle(phase="wire", owner="session")
                deco init wire {{}}
                const IMMUTABLE: int = 1
                global MUTABLE: int = 2
                fn callable() {{}}
                @wire(onchange={annotation})
                fn apply_game_config() {{}}
                "#
            ),
        )]);
        collect(&ws).expect_err("unsupported dependency must reject").message
    }

    #[test]
    fn qualified_wire_rejects_unsupported_dependency_identities() {
        let cases = [
            ("[IMMUTABLE, IMMUTABLE]", "repeats dependency"),
            ("[]", "requires a non-empty dependency list"),
            ("[MUTABLE]", "mutable"),
            ("[callable]", "names a function"),
            ("[UNKNOWN]", "is unknown"),
            ("[IMMUTABLE.value]", "structured identifier list"),
            ("[1]", "structured identifier list"),
        ];
        for (annotation, expected) in cases {
            let message = wire_error(annotation);
            assert!(message.contains(expected), "{annotation}: {message}");
        }
    }

    #[test]
    fn wire_structure_ignores_fingerprint_changes_but_rejects_shape_changes() {
        let plain = workspace(&[(
            "wire.rock",
            r#"
            @lifecycle(phase="wire", owner="session") deco init wire {}
            @wire fn apply_game_config() {}
            "#,
        )]);
        let gated = workspace(&[(
            "wire.rock",
            r#"
            @lifecycle(phase="wire", owner="session") deco init wire {}
            const A: int = 1
            @wire(onchange=[A]) fn apply_game_config() {}
            "#,
        )]);
        let plain_plan = collect(&plain).expect("plain plan");
        let mut same_shape = plain_plan.clone();
        same_shape.entries[0].body_fingerprint = 17;
        assert!(!plain_plan.structure_changed(&same_shape));
        assert!(plain_plan.structure_changed(&collect(&gated).expect("gated plan")));
        assert!(!plain_plan.removed_gated_wire(&same_shape));
        same_shape.entries[0].qualification = LifecycleQualification::OnChange {
            dependencies: Vec::new(),
        };
        assert!(same_shape.removed_gated_wire(&LifecyclePlan::default()));
    }

    #[test]
    fn same_named_cross_module_targets_keep_distinct_logical_identities() {
        let ws = workspace(&[
            (
                "role.rock",
                "@lifecycle(phase=\"draw\", owner=\"session\") deco init paint {}",
            ),
            ("a.rock", "@paint fn render() {}"),
            ("b.rock", "@paint fn render() {}"),
        ]);
        let plan = collect(&ws).expect("plan");
        assert_eq!(plan.entries.len(), 2);
        assert_ne!(plan.entries[0].identity, plan.entries[1].identity);
    }

    #[test]
    fn frame_hooks_in_sim_modules_are_scoped_to_that_modules_wire_names() {
        let ws = workspace(&[
            (
                "roles.rock",
                "@lifecycle(phase=\"update\", owner=\"session\") deco init tick {}",
            ),
            (
                "/repo/game/tests/sims/owned.rock",
                r#"
                @tick fn owned_tick() {}
                @sim("first-wire") fn first_sim() {}
                @sim fn second_sim() {}
                "#,
            ),
            (
                "/repo/game/src/gameplay.rock",
                r#"
                @tick fn production_tick() {}
                @sim fn embedded_diagnostic() {}
                "#,
            ),
        ]);
        let plan = collect(&ws).expect("plan");
        let owned = plan
            .entries
            .iter()
            .find(|entry| entry.diagnostic_name.as_str() == "owned_tick")
            .expect("sim-owned lifecycle entry");
        assert_eq!(
            owned.sim_wires.iter().map(Ustr::as_str).collect::<Vec<_>>(),
            vec!["first-wire", "second_sim"]
        );
        let production = plan
            .entries
            .iter()
            .find(|entry| entry.diagnostic_name.as_str() == "production_tick")
            .expect("production lifecycle entry");
        assert!(production.sim_wires.is_empty());

        let mut changed = plan.clone();
        changed
            .entries
            .iter_mut()
            .find(|entry| entry.diagnostic_name.as_str() == "owned_tick")
            .expect("sim-owned lifecycle entry")
            .sim_wires
            .pop();
        assert!(plan.structure_changed(&changed));
    }

    #[test]
    fn lifecycle_target_shape_rejects_before_emission() {
        let ws = workspace(&[(
            "bad.rock",
            r#"
            @lifecycle(phase="update", owner="session") deco init tick {}
            @tick fn bad(x: int) {}
        "#,
        )]);
        let err = collect(&ws).expect_err("parameterized lifecycle target must reject");
        assert!(err.message.contains("zero-argument"), "{err:?}");
    }

    #[test]
    fn scene_lifecycle_accepts_one_concrete_reference_root() {
        let ws = workspace(&[(
            "scene.rock",
            r#"
            @lifecycle(phase="update", owner="scene", stage_param="stage")
            deco init scene_update(stage: int = 100) {}
            struct Dungeon { ticks: int }
            @scene_update fn tick_dungeon(root: Dungeon) { root.ticks += 1 }
            "#,
        )]);
        let plan = collect(&ws).expect("typed scene lifecycle target");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].owner.as_str(), "scene");
        assert_eq!(plan.entries[0].diagnostic_name.as_str(), "tick_dungeon");
        assert_eq!(plan.census.session_owned, 0);
        assert_eq!(plan.census.scene_owned, 1);
        assert_eq!(
            plan.entries[0].root_type,
            Some(MonoTy::App {
                ctor: MonoCtor::Nominal(Ustr::from("Dungeon")),
                params: Vec::new(),
            })
        );
    }

    #[test]
    fn scene_lifecycle_rejects_non_reference_and_generic_roots() {
        let cases = [
            ("root: int", "reference struct"),
            ("root: Vec2", "reference struct"),
            ("root: Generic<int>", "non-generic reference struct"),
        ];
        for (parameter, expected) in cases {
            let ws = workspace(&[(
                "bad-scene.rock",
                &format!(
                    r#"
                    value Vec2 {{ x: float, y: float }}
                    struct Generic<T> {{ value: T }}
                    @lifecycle(phase="update", owner="scene") deco init scene_update {{}}
                    @scene_update fn bad({parameter}) {{}}
                    "#
                ),
            )]);
            let error = collect(&ws).expect_err("unsupported Scene root must reject");
            assert!(error.message.contains(expected), "{parameter}: {error:?}");
        }
    }

    #[test]
    fn scene_root_type_is_lifecycle_structure() {
        let collect_root = |root: &str| {
            let ws = workspace(&[(
                "root.rock",
                &format!(
                    r#"
                    struct A {{ value: int }}
                    struct B {{ value: int }}
                    @lifecycle(phase="update", owner="scene") deco init scene_update {{}}
                    @scene_update fn tick(root: {root}) {{ root.value += 1 }}
                    "#
                ),
            )]);
            collect(&ws).expect("scene plan")
        };
        let a = collect_root("A");
        let same = collect_root("A");
        let b = collect_root("B");
        assert!(!a.structure_changed(&same));
        assert!(a.structure_changed(&b));
    }

    #[test]
    fn scene_and_session_updates_share_one_stage_order() {
        let ws = workspace(&[(
            "mixed.rock",
            r#"
            struct Root { value: int }
            @lifecycle(phase="update", owner="session", stage_param="stage")
            deco init update(stage: int = 100) {}
            @lifecycle(phase="update", owner="scene", stage_param="stage")
            deco init scene_update(stage: int = 100) {}
            @scene_update(stage=40) fn scene_first(root: Root) { root.value += 1 }
            @update(stage=41) fn session_middle() {}
            @scene_update(stage=42) fn scene_last(root: Root) { root.value += 1 }
            "#,
        )]);
        let plan = collect(&ws).expect("mixed lifecycle plan");
        assert_eq!(plan.census.session_owned, 1);
        assert_eq!(plan.census.scene_owned, 2);
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| (entry.diagnostic_name.as_str(), entry.owner.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("scene_first", "scene"),
                ("session_middle", "session"),
                ("scene_last", "scene"),
            ],
        );
    }

    #[test]
    fn post_imgui_draw_is_a_compiler_owned_role() {
        let ws = workspace(&[(
            "post-imgui.rock",
            r#"
            @lifecycle(phase="post_imgui_draw", owner="session", stage_param="stage")
            deco init post_imgui_draw(stage: int = 100) {}
            @post_imgui_draw(stage=20) fn later() {}
            @post_imgui_draw(stage=10) fn first() {}
        "#,
        )]);
        let plan = collect(&ws).expect("post_imgui_draw must be collected");
        assert_eq!(plan.census.declarations, 1);
        assert_eq!(plan.census.applications, 2);
        assert_eq!(plan.census.admitted, 2);
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| (entry.phase.as_str(), entry.diagnostic_name.as_str(), entry.stage))
                .collect::<Vec<_>>(),
            vec![
                ("post_imgui_draw", "first", 10),
                ("post_imgui_draw", "later", 20),
            ]
        );
    }

    #[test]
    fn late_draw_replay_role_is_compiler_owned_and_structural() {
        let ws = workspace(&[
            (
                "role.rock",
                r#"
                @lifecycle(phase="late_draw", owner="session", stage_param="stage", replay_param="replay")
                deco init late_draw(stage: int = 100, replay: string = "presentation") {}
                @late_draw(replay="state") fn stateful() {}
                @late_draw fn visual() {}
                "#,
            ),
        ]);
        let plan = collect(&ws).expect("late-draw replay roles");
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| (&entry.diagnostic_name, &entry.replay_role))
                .collect::<Vec<_>>(),
            vec![
                (&Ustr::from("stateful"), &LifecycleReplayRole::State),
                (&Ustr::from("visual"), &LifecycleReplayRole::Presentation),
            ]
        );

        let mut changed = plan.clone();
        changed.entries[0].replay_role = LifecycleReplayRole::Presentation;
        assert!(plan.structure_changed(&changed));
    }

    #[test]
    fn late_draw_replay_role_rejects_unknown_literals() {
        let ws = workspace(&[(
            "role.rock",
            r#"
            @lifecycle(phase="late_draw", owner="session", stage_param="stage", replay_param="replay")
            deco init late_draw(stage: int = 100, replay: string = "presentation") {}
            @late_draw(replay="snapshot") fn invalid() {}
            "#,
        )]);
        let error = collect(&ws).expect_err("unknown replay role must reject");
        assert!(error.message.contains("unsupported lifecycle replay role `snapshot`"), "{error:?}");
    }
}
