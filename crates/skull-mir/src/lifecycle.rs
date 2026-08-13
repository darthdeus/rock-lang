use bone_semantic::hir::{AnnoValue, BodyOwner, Expr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{CompilerError, Idx, ModuleId};
use ustr::Ustr;

use crate::code_identity::CodeIdentity;
use crate::instance::Instance;
use crate::mir::FuncRef;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleEntry {
    pub phase: Ustr,
    pub owner: Ustr,
    pub identity: CodeIdentity,
    pub diagnostic_name: Ustr,
    pub stage: i64,
    pub declaration_ordinal: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecycleCensus {
    pub declarations: usize,
    pub applications: usize,
    pub admitted: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecyclePlan {
    pub entries: Vec<LifecycleEntry>,
    pub census: LifecycleCensus,
}

fn phase_rank(phase: Ustr) -> Option<u8> {
    Some(match phase.as_str() {
        "early_init" => 0,
        "init" => 1,
        "late_init" => 2,
        "wire" => 3,
        "update" => 4,
        "late_update" => 5,
        "draw" => 6,
        "late_draw" => 7,
        "reset" => 8,
        "shutdown" => 9,
        _ => return None,
    })
}

fn string_arg(ann: &bone_semantic::hir::Annotation, name: &str) -> Option<Ustr> {
    ann.args.iter().find_map(|a| {
        (a.name.as_ref().is_some_and(|n| n.name.as_str() == name)).then(|| match a.value {
            AnnoValue::String(s) => Some(s),
            _ => None,
        }).flatten()
    })
}

fn int_expr(body: &bone_semantic::hir::Body, e: Idx<bone_semantic::hir::Expr>) -> Option<i64> {
    match body.expr_arena[e] {
        Expr::Int(n) => Some(n),
        Expr::UnaryMinus(inner) => int_expr(body, inner)?.checked_neg(),
        _ => None,
    }
}

pub fn collect(ws: &Workspace) -> Result<LifecyclePlan, CompilerError> {
    let mut roles = std::collections::HashMap::new();
    let mut census = LifecycleCensus::default();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (did, deco) in module.decorators.iter() {
            let Some(meta) = deco.annotations.iter().find(|a| a.name.name.as_str() == "lifecycle") else { continue };
            census.declarations += 1;
            let phase = string_arg(meta, "phase").expect("validated lifecycle phase");
            if phase_rank(phase).is_none() {
                return Err(CompilerError::new_with_label(meta.span, format!("unsupported lifecycle phase `{phase}`")));
            }
            let owner = string_arg(meta, "owner").expect("validated lifecycle owner");
            if owner.as_str() != "session" {
                return Err(CompilerError::new_with_label(meta.span, format!("unsupported lifecycle owner `{owner}`; expected `session`")));
            }
            let stage_param = string_arg(meta, "stage_param");
            let stage_index = match stage_param {
                Some(name) => Some(deco.param_decls.iter().position(|p| p.name == name).ok_or_else(|| CompilerError::new_with_label(meta.span, format!("lifecycle stage_param `{name}` is not a parameter of `{}`", deco.name)))?),
                None if deco.param_decls.is_empty() => None,
                None => return Err(CompilerError::new_with_label(meta.span, format!("lifecycle decorator `{}` has parameters but no stage_param", deco.name))),
            };
            roles.insert(deco.name, (module.id, did, phase, owner, stage_index));
        }
    }

    let mut entries = Vec::new();
    let mut ordinal = 0usize;
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, f) in module.functions.iter() {
            for app in &f.annotations {
                let Some(&(dmid, did, phase, owner, stage_index)) = roles.get(&app.name.name) else { continue };
                census.applications += 1;
                if !f.generics.is_empty() || !f.param_decls.is_empty() || f.return_ty.is_some() {
                    return Err(CompilerError::new_with_label(f.span, format!("lifecycle target `{}` must be concrete, non-generic, zero-argument, and unit-returning", f.name)));
                }
                if f.backend != bone_semantic::hir::FnBackend::Any {
                    return Err(CompilerError::new_with_label(f.span, format!("lifecycle target `{}` must have a backend-neutral Rock body", f.name)));
                }
                let deco = &ws.module(dmid).decorators[did];
                let stage = if let Some(i) = stage_index {
                    let pname = deco.param_decls[i].name;
                    if let Some(arg) = app.args.iter().find(|a| a.name.as_ref().is_some_and(|n| n.name == pname)) {
                        match arg.value {
                            AnnoValue::Int(n) => n,
                            _ => return Err(CompilerError::new_with_label(app.span, format!("lifecycle stage `{pname}` must be an integer literal"))),
                        }
                    } else {
                        let body = &ws.module(dmid).bodies[&BodyOwner::Decorator(did)];
                        let e = body.param_defaults.get(i).and_then(|e| *e).ok_or_else(|| CompilerError::new_with_label(app.span, format!("lifecycle stage `{pname}` has no default")))?;
                        int_expr(body, e).ok_or_else(|| CompilerError::new_with_label(deco.span, format!("lifecycle stage default `{pname}` must be an integer literal")))?
                    }
                } else { 0 };
                entries.push(LifecycleEntry {
                    phase, owner,
                    identity: CodeIdentity::Named(Instance::base(FuncRef { module: module.id, func: fid })),
                    diagnostic_name: f.name,
                    stage,
                    declaration_ordinal: ordinal,
                });
                ordinal += 1;
                census.admitted += 1;
            }
        }
    }
    entries.sort_by_key(|e| (phase_rank(e.phase).expect("validated phase"), e.stage, e.declaration_ordinal));
    Ok(LifecyclePlan { entries, census })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::semantic_passes::semantic_pass;

    fn workspace(sources: &[(&str, &str)]) -> Workspace {
        let mut modules = sources.iter().map(|(file, src)| {
            skull_parser::parse_module_clean(src, Ustr::from(*file)).expect("parse")
        }).collect::<Vec<_>>();
        semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn plan_reads_stage_defaults_and_orders_equal_stage_by_declaration() {
        let ws = workspace(&[("plan.rock", r#"
            @lifecycle(phase="update", owner="session", stage_param="stage")
            deco init tick(stage: int = 100) {}
            @tick(stage=20) fn later() {}
            @tick(stage=10) fn first() {}
            @tick(stage=10) fn second() {}
            @tick fn defaulted() {}
        "#)]);
        let plan = collect(&ws).expect("plan");
        assert_eq!(plan.census.declarations, 1);
        assert_eq!(plan.census.applications, 4);
        assert_eq!(plan.census.admitted, 4);
        assert_eq!(plan.entries.iter().map(|e| (e.diagnostic_name.as_str(), e.stage)).collect::<Vec<_>>(),
            vec![("first", 10), ("second", 10), ("later", 20), ("defaulted", 100)]);
        assert!(plan.entries[0].declaration_ordinal < plan.entries[1].declaration_ordinal);
    }

    #[test]
    fn same_named_cross_module_targets_keep_distinct_logical_identities() {
        let ws = workspace(&[
            ("role.rock", "@lifecycle(phase=\"draw\", owner=\"session\") deco init paint {}"),
            ("a.rock", "@paint fn render() {}"),
            ("b.rock", "@paint fn render() {}"),
        ]);
        let plan = collect(&ws).expect("plan");
        assert_eq!(plan.entries.len(), 2);
        assert_ne!(plan.entries[0].identity, plan.entries[1].identity);
    }

    #[test]
    fn lifecycle_target_shape_rejects_before_emission() {
        let ws = workspace(&[("bad.rock", r#"
            @lifecycle(phase="update", owner="session") deco init tick {}
            @tick fn bad(x: int) {}
        "#)]);
        let err = collect(&ws).expect_err("parameterized lifecycle target must reject");
        assert!(err.message.contains("zero-argument"), "{err:?}");
    }
}
