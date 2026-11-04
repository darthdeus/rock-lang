use bone_semantic::hir::{AnnoValue, TyId, TypeCtor, TypeExpr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::CompilerError;
use bone_semantic::prelude::ModuleId;
use ustr::Ustr;

use crate::code_identity::CodeIdentity;
use crate::instance::Instance;
use crate::mir::FuncRef;
use crate::monoty::{MonoSubst, MonoTy, convert_decl};

pub fn is_artifact_definition(decorator: &bone_semantic::hir::DecoratorDef) -> bool {
    decorator
        .annotations
        .iter()
        .any(|ann| ann.is_builtin("artifact_definition"))
}

pub fn is_compiler_owned_decorator(decorator: &bone_semantic::hir::DecoratorDef) -> bool {
    is_artifact_definition(decorator)
        || decorator
            .annotations
            .iter()
            .any(|ann| ann.is_builtin("lifecycle"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionEntry {
    pub catalog: Ustr,
    pub key: Ustr,
    pub identity: CodeIdentity,
    pub diagnostic_name: Ustr,
    pub declaration_ordinal: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefinitionCensus {
    pub declarations: usize,
    pub accessors: usize,
    pub applications: usize,
    pub admitted: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionCatalog {
    pub catalog: Ustr,
    pub accessor: Option<FuncRef>,
    pub definition_type: Option<MonoTy>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefinitionPlan {
    pub catalogs: Vec<DefinitionCatalog>,
    pub entries: Vec<DefinitionEntry>,
    pub census: DefinitionCensus,
}

#[derive(Clone)]
struct Role {
    catalog: Ustr,
    key_param: Ustr,
}

fn string_arg(ann: &bone_semantic::hir::Annotation, name: &str) -> Option<Ustr> {
    ann.args.iter().find_map(|arg| {
        (arg.name.as_ref().is_some_and(|n| n.name.as_str() == name)).then(|| match arg.value {
            AnnoValue::String(value) => Some(value),
            _ => None,
        })?
    })
}

fn annotation<'a>(
    annotations: &'a [bone_semantic::hir::Annotation],
    name: &str,
) -> Result<Option<&'a bone_semantic::hir::Annotation>, CompilerError> {
    let mut found = annotations.iter().filter(|ann| ann.is_builtin(name));
    let first = found.next();
    if let Some(second) = found.next() {
        return Err(CompilerError::new_with_label(
            second.span,
            format!("function has more than one `@{name}` annotation"),
        ));
    }
    Ok(first)
}

fn static_type(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(module, index) => Some(ws.module(module).ty_arena[index]),
        TyId::Local(_) => None,
    }
}

fn artifact_definition_type(ws: &Workspace, return_ty: TyId) -> Option<TyId> {
    let TypeExpr::App {
        ctor: TypeCtor::Array,
        params,
    } = static_type(ws, return_ty)?
    else {
        return None;
    };
    let mut params = params.into_iter();
    let element = return_ty.with_idx(params.next()?);
    if params.next().is_some() {
        return None;
    }
    let TypeExpr::App {
        ctor: TypeCtor::Nominal(name),
        params,
    } = static_type(ws, element)?
    else {
        return None;
    };
    if !bone_semantic::prelude::is_intrinsic_key(name, "ArtifactDefinition") {
        return None;
    }
    let mut params = params.into_iter();
    let definition = element.with_idx(params.next()?);
    params.next().is_none().then_some(definition)
}

fn metadata<'a>(
    decorator: &'a bone_semantic::hir::DecoratorDef,
) -> Result<Option<&'a bone_semantic::hir::Annotation>, CompilerError> {
    let mut found = decorator
        .annotations
        .iter()
        .filter(|ann| ann.is_builtin("artifact_definition"));
    let first = found.next();
    if let Some(second) = found.next() {
        return Err(CompilerError::new_with_label(
            second.span,
            format!(
                "decorator `{}` has more than one `@artifact_definition` role",
                decorator.name
            ),
        ));
    }
    Ok(first)
}

fn role_for(
    ws: &Workspace,
    decorator: &bone_semantic::hir::DecoratorDef,
    ann: &bone_semantic::hir::Annotation,
) -> Result<Role, CompilerError> {
    let Some(catalog) = string_arg(ann, "catalog") else {
        return Err(CompilerError::new_with_label(
            ann.span,
            "`@artifact_definition` requires a string catalog=".to_string(),
        ));
    };
    let Some(key_param) = string_arg(ann, "key_param") else {
        return Err(CompilerError::new_with_label(
            ann.span,
            "`@artifact_definition` requires a string key_param=".to_string(),
        ));
    };
    if catalog.is_empty() || key_param.is_empty() {
        return Err(CompilerError::new_with_label(
            ann.span,
            "`@artifact_definition` catalog= and key_param= must not be empty".to_string(),
        ));
    }
    let Some((index, param)) = decorator
        .param_decls
        .iter()
        .enumerate()
        .find(|(_, param)| param.name == key_param)
    else {
        return Err(CompilerError::new_with_label(
            ann.span,
            format!(
                "artifact definition key_param `{key_param}` is not a parameter of `{}`",
                decorator.name
            ),
        ));
    };
    if decorator.param_decls.len() != 1 || index != 0 {
        return Err(CompilerError::new_with_label(
            ann.span,
            format!(
                "artifact definition decorator `{}` must have exactly the required string key parameter `{key_param}`",
                decorator.name
            ),
        ));
    }
    if param.has_default {
        return Err(CompilerError::new_with_label(
            ann.span,
            format!("artifact definition key parameter `{key_param}` cannot have a default"),
        ));
    }
    if param.ty != ws.builtins.string() {
        return Err(CompilerError::new_with_label(
            ann.span,
            format!("artifact definition key parameter `{key_param}` must have type string"),
        ));
    }
    Ok(Role { catalog, key_param })
}

pub fn collect(ws: &Workspace) -> Result<DefinitionPlan, CompilerError> {
    let mut roles = std::collections::HashMap::new();
    let mut census = DefinitionCensus::default();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, decorator) in module.decorators.iter() {
            let Some(ann) = metadata(decorator)? else {
                continue;
            };
            census.declarations += 1;
            let role = role_for(ws, decorator, ann)?;
            if roles.insert(decorator.def_key, role).is_some() {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    format!(
                        "duplicate artifact definition decorator `{}`",
                        decorator.name
                    ),
                ));
            }
        }
    }

    let mut catalogs = std::collections::HashMap::<Ustr, DefinitionCatalog>::new();
    for role in roles.values() {
        catalogs
            .entry(role.catalog)
            .or_insert_with(|| DefinitionCatalog {
                catalog: role.catalog,
                accessor: None,
                definition_type: None,
            });
    }

    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, function) in module.functions.iter() {
            let Some(ann) = annotation(&function.annotations, "artifact_catalog")? else {
                continue;
            };
            census.accessors += 1;
            let Some(catalog) = string_arg(ann, "catalog") else {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    "`@artifact_catalog` requires a string catalog=".to_string(),
                ));
            };
            if catalog.is_empty() {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    "`@artifact_catalog` catalog= must not be empty".to_string(),
                ));
            }
            if !catalogs.contains_key(&catalog) {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    format!(
                        "artifact catalog accessor `{}` names unknown catalog `{catalog}`",
                        function.name
                    ),
                ));
            }
            if !function.generics.is_empty()
                || !function.param_decls.is_empty()
                || !function.is_extern
                || function.inline_body.is_some()
                || !function
                    .annotations
                    .iter()
                    .any(|candidate| candidate.is_builtin("intrinsic"))
            {
                return Err(CompilerError::new_with_label(
                    function.span,
                    format!(
                        "artifact catalog accessor `{}` must be a compiler-owned @intrinsic extern fn with no parameters",
                        function.name
                    ),
                ));
            }
            let Some(return_ty) = function.return_ty else {
                return Err(CompilerError::new_with_label(
                    function.span,
                    format!(
                        "artifact catalog accessor `{}` must return [ArtifactDefinition<T>]",
                        function.name
                    ),
                ));
            };
            let Some(definition_ty) = artifact_definition_type(ws, return_ty) else {
                return Err(CompilerError::new_with_label(
                    function.span,
                    format!(
                        "artifact catalog accessor `{}` must return [ArtifactDefinition<T>], got `{}`",
                        function.name,
                        ws.render(None, return_ty)
                    ),
                ));
            };
            let definition_type =
                convert_decl(ws, definition_ty, &MonoSubst::default()).map_err(|reason| {
                    CompilerError::new_with_label(
                        function.span,
                        format!(
                            "artifact catalog accessor `{}` has non-concrete definition type ({})",
                            function.name,
                            reason.bucket(),
                        ),
                    )
                })?;
            let Some(plan_catalog) = catalogs.get_mut(&catalog) else {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    format!(
                        "artifact catalog accessor `{}` names unknown catalog `{catalog}`",
                        function.name
                    ),
                ));
            };
            if plan_catalog.accessor.is_some() {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    format!("catalog `{catalog}` has more than one typed accessor"),
                ));
            }
            plan_catalog.accessor = Some(FuncRef {
                module: module.id,
                func: fid,
            });
            plan_catalog.definition_type = Some(definition_type);
        }
    }

    let mut entries = Vec::new();
    let mut ordinal = 0usize;
    let mut seen = std::collections::HashMap::<(Ustr, Ustr), Ustr>::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, function) in module.functions.iter() {
            for application in &function.annotations {
                let application_key = application.resolved_name.unwrap_or(application.name.name);
                let Some(role) = roles.get(&application_key) else {
                    continue;
                };
                census.applications += 1;
                if !function.generics.is_empty() || !function.param_decls.is_empty() {
                    return Err(CompilerError::new_with_label(
                        function.span,
                        format!(
                            "artifact definition target `{}` must be concrete, non-generic, and zero-argument",
                            function.name
                        ),
                    ));
                }
                if function.backend != bone_semantic::hir::FnBackend::Any {
                    return Err(CompilerError::new_with_label(
                        function.span,
                        format!(
                            "artifact definition target `{}` must have a backend-neutral Rock body",
                            function.name
                        ),
                    ));
                }
                let Some(return_ty) = function.return_ty else {
                    return Err(CompilerError::new_with_label(
                        function.span,
                        format!(
                            "artifact definition target `{}` must return a concrete definition",
                            function.name
                        ),
                    ));
                };
                if return_ty == ws.builtins.unit() {
                    return Err(CompilerError::new_with_label(
                        function.span,
                        format!(
                            "artifact definition target `{}` must return a concrete definition",
                            function.name
                        ),
                    ));
                }
                if let Some(expected_type) = catalogs
                    .get(&role.catalog)
                    .and_then(|catalog| catalog.definition_type.as_ref())
                {
                    let actual_type = convert_decl(ws, return_ty, &MonoSubst::default()).map_err(|reason| {
                        CompilerError::new_with_label(
                            function.span,
                            format!(
                                "artifact definition target `{}` has non-concrete definition type ({})",
                                function.name,
                                reason.bucket(),
                            ),
                        )
                    })?;
                    if &actual_type != expected_type {
                        let actual_display = actual_type.render();
                        let expected_display = expected_type.render();
                        return Err(CompilerError::new_with_label(
                            function.span,
                            format!(
                                "artifact definition target `{}` has definition type `{actual_display}` (identity `{}`), but catalog `{}` requires `{expected_display}` (identity `{}`)",
                                function.name,
                                actual_type.canonical(),
                                role.catalog,
                                expected_type.canonical(),
                            ),
                        ));
                    }
                }
                if application.args.len() != 1
                    || application.args[0].name.as_ref().map(|name| name.name)
                        != Some(role.key_param)
                {
                    return Err(CompilerError::new_with_label(
                        application.span,
                        format!(
                            "artifact definition `{}` requires one named literal `{}` argument",
                            application.name.name, role.key_param
                        ),
                    ));
                }
                let key = match application.args[0].value {
                    AnnoValue::String(value) if !value.is_empty() => value,
                    _ => {
                        return Err(CompilerError::new_with_label(
                            application.span,
                            format!(
                                "artifact definition key `{}` must be a non-empty string literal",
                                role.key_param
                            ),
                        ));
                    }
                };
                if let Some(previous) = seen.insert((role.catalog, key), function.name) {
                    return Err(CompilerError::new_with_label(
                        application.span,
                        format!(
                            "duplicate artifact definition key `{key}` in catalog `{}` (already defined by `{previous}`)",
                            role.catalog
                        ),
                    ));
                }
                entries.push(DefinitionEntry {
                    catalog: role.catalog,
                    key,
                    identity: CodeIdentity::Named(Instance::base(FuncRef {
                        module: module.id,
                        func: fid,
                    })),
                    diagnostic_name: function.name,
                    declaration_ordinal: ordinal,
                });
                ordinal += 1;
                census.admitted += 1;
            }
        }
    }
    entries.sort_by_key(|entry| (entry.catalog, entry.declaration_ordinal));
    let mut catalogs = catalogs.into_values().collect::<Vec<_>>();
    catalogs.sort_by_key(|catalog| catalog.catalog);
    Ok(DefinitionPlan {
        catalogs,
        entries,
        census,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bone_semantic::semantic_passes::semantic_pass;

    fn workspace(sources: &[(&str, &str)]) -> Workspace {
        let mut modules = sources
            .iter()
            .map(|(file, src)| {
                skull_parser::parse_module_clean(src, Ustr::from(file)).expect("parse")
            })
            .collect::<Vec<_>>();
        semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn collector_admits_literal_keyed_factories_and_counts_population() {
        let ws = workspace(&[(
            "defs.rock",
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="alpha") fn make_alpha() -> int { return 1 }
                @widget(key="beta") fn make_beta() -> int { return 2 }
                fn main() {}
            "#,
        )]);

        let plan = collect(&ws).expect("definition plan");
        assert_eq!(plan.census.declarations, 1);
        assert_eq!(plan.census.applications, 2);
        assert_eq!(plan.census.admitted, 2);
        assert_eq!(plan.entries.len(), plan.census.admitted);
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| (entry.catalog.as_str(), entry.key.as_str()))
                .collect::<Vec<_>>(),
            vec![("widgets", "alpha"), ("widgets", "beta")]
        );
        assert_ne!(plan.entries[0].identity, plan.entries[1].identity);
        assert!(plan.entries[0].diagnostic_name == Ustr::from("make_alpha"));
    }

    #[test]
    fn duplicate_keys_reject_with_the_catalog_and_key() {
        let ws = workspace(&[(
            "dupe.rock",
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="same") fn first() -> int { return 1 }
                @widget(key="same") fn second() -> int { return 2 }
                fn main() {}
            "#,
        )]);

        let err = collect(&ws).expect_err("duplicate definition keys must reject");
        assert!(err.message.contains("widgets"), "{err:?}");
        assert!(err.message.contains("same"), "{err:?}");
    }

    fn collect_error(source: &str) -> CompilerError {
        let ws = workspace(&[("invalid.rock", source)]);
        collect(&ws).expect_err("invalid artifact definition must reject")
    }

    const TYPED_PREFIX: &str = r#"
        struct ArtifactDefinition<T> {
            catalog: string,
            key: string,
            name: string,
            ordinal: int,
            definition: T,
        }
        struct WidgetDef { value: int }
    "#;

    fn typed_source(body: &str) -> String {
        format!(
            r#"{TYPED_PREFIX}
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {{}}
                {body}
                fn main() {{}}
            "#
        )
    }

    #[test]
    fn typed_nonempty_catalog_has_an_accessor_and_a_nonzero_population() {
        let ws = workspace(&[(
            "typed.rock",
            &typed_source(
                r#"
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
                @widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { value: 1 } }
                "#,
            ),
        )]);
        let plan = collect(&ws).expect("typed definition plan");
        assert_eq!(plan.census.accessors, 1);
        assert_eq!(plan.catalogs.len(), 1);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries.len(), plan.census.admitted);
    }

    #[test]
    fn declared_empty_catalog_is_retained_as_a_typed_catalog() {
        let ws = workspace(&[(
            "empty.rock",
            &typed_source(
                r#"
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
                "#,
            ),
        )]);
        let plan = collect(&ws).expect("declared empty definition plan");
        assert_eq!(plan.catalogs.len(), 1);
        assert_eq!(plan.entries.len(), 0);
        assert_eq!(plan.census.admitted, 0);
    }

    #[test]
    fn wrong_factory_definition_type_rejects() {
        let err = collect(&workspace(&[(
            "wrong_factory.rock",
            &typed_source(
                r#"
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<WidgetDef>]
                @widget(key="alpha") fn make_alpha() -> int { return 1 }
                "#,
            ),
        )]))
        .expect_err("factory definition type must be checked");
        assert!(err.message.contains("definition type"), "{err:?}");
    }

    #[test]
    fn semantically_distinct_same_rendering_factory_type_rejects() {
        let err = collect(&workspace(&[(
            "same_rendering.rock",
            &typed_source(
                r#"
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn widget_definition_plan() -> [ArtifactDefinition<fn(int) -> int?>]
                @widget(key="collision") fn make_collision() -> (fn(int) -> int)? { return null }
                "#,
            ),
        )]))
        .expect_err("semantic type mismatch must reject despite equal display spellings");
        assert!(err.message.contains("identity"), "{err:?}");
        assert!(err.message.contains("definition type"), "{err:?}");
    }

    #[test]
    fn wrong_accessor_definition_type_rejects() {
        let err = collect(&workspace(&[(
            "wrong_accessor.rock",
            &typed_source(
                r#"
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn widget_definition_plan() -> [int]
                @widget(key="alpha") fn make_alpha() -> WidgetDef { return WidgetDef { value: 1 } }
                "#,
            ),
        )]))
        .expect_err("accessor definition type must be checked");
        assert!(err.message.contains("ArtifactDefinition"), "{err:?}");
    }

    #[test]
    fn unknown_catalog_accessor_rejects() {
        let err = collect(&workspace(&[(
            "unknown_accessor.rock",
            &format!(
                r#"{TYPED_PREFIX}
                    @artifact_catalog(catalog="missing")
                    @intrinsic extern fn missing_definition_plan() -> [ArtifactDefinition<WidgetDef>]
                    fn main() {{}}
                "#
            ),
        )]))
        .expect_err("unknown catalog accessor must reject");
        assert!(err.message.contains("unknown catalog"), "{err:?}");
    }

    #[test]
    fn duplicate_typed_accessors_reject_instead_of_picking_one() {
        let err = collect(&workspace(&[(
            "duplicate_accessor.rock",
            &typed_source(
                r#"
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn first_plan() -> [ArtifactDefinition<WidgetDef>]
                @artifact_catalog(catalog="widgets")
                @intrinsic extern fn second_plan() -> [ArtifactDefinition<WidgetDef>]
                "#,
            ),
        )]))
        .expect_err("duplicate typed accessors must reject");
        assert!(
            err.message.contains("more than one") || err.message.contains("duplicate"),
            "{err:?}"
        );
    }

    #[test]
    fn missing_and_nonliteral_keys_reject_before_plan_emission() {
        let missing = collect_error(
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget fn missing() -> int { return 1 }
                fn main() {}
            "#,
        );
        assert!(missing.message.contains("named literal"), "{missing:?}");

        let nonliteral = collect_error(
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                const KEY: string = "not-a-literal-at-the-application"
                @widget(key=KEY) fn nonliteral() -> int { return 1 }
                fn main() {}
            "#,
        );
        assert!(
            nonliteral.message.contains("string literal"),
            "{nonliteral:?}"
        );
    }

    #[test]
    fn role_and_factory_shapes_reject() {
        let extra_param = collect_error(
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string, version: int) {}
                @widget(key="x") fn extra() -> int { return 1 }
                fn main() {}
            "#,
        );
        assert!(
            extra_param.message.contains("exactly the required string"),
            "{extra_param:?}"
        );

        let unit = collect_error(
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="x") fn unit() {}
                fn main() {}
            "#,
        );
        assert!(
            unit.message.contains("return a concrete definition"),
            "{unit:?}"
        );

        let backend = collect_error(
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="x") inline(lua) fn backend() -> int = "return 1"
                fn main() {}
            "#,
        );
        assert!(backend.message.contains("backend-neutral"), "{backend:?}");
    }
}
