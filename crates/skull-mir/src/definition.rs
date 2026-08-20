use bone_semantic::inference::Workspace;
use bone_semantic::hir::AnnoValue;
use bone_semantic::prelude::CompilerError;
use bone_semantic::prelude::ModuleId;
use ustr::Ustr;

use crate::code_identity::CodeIdentity;
use crate::instance::Instance;
use crate::mir::FuncRef;

pub fn is_artifact_definition(decorator: &bone_semantic::hir::DecoratorDef) -> bool {
    decorator
        .annotations
        .iter()
        .any(|ann| ann.name.name.as_str() == "artifact_definition")
}

pub fn is_compiler_owned_decorator(decorator: &bone_semantic::hir::DecoratorDef) -> bool {
    is_artifact_definition(decorator)
        || decorator
            .annotations
            .iter()
            .any(|ann| ann.name.name.as_str() == "lifecycle")
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
    pub accessor: FuncRef,
    pub definition_type: Ustr,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefinitionPlan {
    pub catalogs: Vec<DefinitionCatalog>,
    pub entries: Vec<DefinitionEntry>,
    pub census: DefinitionCensus,
}

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

fn metadata<'a>(
    decorator: &'a bone_semantic::hir::DecoratorDef,
) -> Result<Option<&'a bone_semantic::hir::Annotation>, CompilerError> {
    let mut found = decorator
        .annotations
        .iter()
        .filter(|ann| ann.name.name.as_str() == "artifact_definition");
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
    let catalog = string_arg(ann, "catalog").expect("validated artifact_definition catalog");
    let key_param =
        string_arg(ann, "key_param").expect("validated artifact_definition key_param");
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
    Ok(Role {
        catalog,
        key_param,
    })
}

pub fn collect(ws: &Workspace) -> Result<DefinitionPlan, CompilerError> {
    let mut roles = std::collections::HashMap::new();
    let mut census = DefinitionCensus::default();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (_, decorator) in module.decorators.iter() {
            let Some(ann) = metadata(decorator)? else { continue };
            census.declarations += 1;
            let role = role_for(ws, decorator, ann)?;
            if roles.insert(decorator.name, role).is_some() {
                return Err(CompilerError::new_with_label(
                    ann.span,
                    format!("duplicate artifact definition decorator `{}`", decorator.name),
                ));
            }
        }
    }

    let mut entries = Vec::new();
    let mut ordinal = 0usize;
    let mut seen = std::collections::HashMap::<(Ustr, Ustr), Ustr>::new();
    for module in ws.modules.iter().filter(|m| m.id != ModuleId::builtin()) {
        for (fid, function) in module.functions.iter() {
            for application in &function.annotations {
                let Some(role) = roles.get(&application.name.name) else { continue };
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
                if function.return_ty.is_none_or(|ty| ty == ws.builtins.unit()) {
                    return Err(CompilerError::new_with_label(
                        function.span,
                        format!(
                            "artifact definition target `{}` must return a concrete definition",
                            function.name
                        ),
                    ));
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
    Ok(DefinitionPlan {
        catalogs: Vec::new(),
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
                skull_parser::parse_module_clean(src, Ustr::from(*file)).expect("parse")
            })
            .collect::<Vec<_>>();
        semantic_pass(&mut modules).expect("semantic")
    }

    #[test]
    fn collector_admits_literal_keyed_factories_and_counts_population() {
        let ws = workspace(&[ (
            "defs.rock",
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="alpha") fn make_alpha() -> int { return 1 }
                @widget(key="beta") fn make_beta() -> int { return 2 }
                fn main() {}
            "#,
        ) ]);

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
        let ws = workspace(&[ (
            "dupe.rock",
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="same") fn first() -> int { return 1 }
                @widget(key="same") fn second() -> int { return 2 }
                fn main() {}
            "#,
        ) ]);

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
        assert!(err.message.contains("more than one") || err.message.contains("duplicate"), "{err:?}");
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
        assert!(nonliteral.message.contains("string literal"), "{nonliteral:?}");
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
        assert!(extra_param.message.contains("exactly the required string"), "{extra_param:?}");

        let unit = collect_error(
            r#"
                @artifact_definition(catalog="widgets", key_param="key")
                deco init widget(key: string) {}
                @widget(key="x") fn unit() {}
                fn main() {}
            "#,
        );
        assert!(unit.message.contains("return a concrete definition"), "{unit:?}");

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
