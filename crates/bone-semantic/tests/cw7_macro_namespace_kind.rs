use bone_core::modules::{DefKind, NsAliases, PackageAliases};
use bone_core::prelude::*;
use bone_semantic::semantic_passes::{build_namespaces, legacy_owned_names, semantic_pass_with_aliases};
use skull_parser::{Ast, Dialect};

const SOURCE: &str = include_str!("fixtures/cw7_macro_namespace_kind.rock");

fn provider(dialect: Dialect, file: &str) -> Ast {
    let mut ast = skull_parser::parse_module_with_dialect(SOURCE, ustr(file), dialect)
        .expect("parse macro namespace fixture");
    assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
    ast.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "provider".to_string(),
    });
    ast
}

fn aliases() -> NsAliases {
    NsAliases {
        packages: vec![
            PackageAliases {
                id: 1,
                self_name: ustr("provider"),
                aliases: vec![],
            },
            PackageAliases {
                id: 2,
                self_name: ustr("consumer"),
                aliases: vec![(ustr("provider"), 1)],
            },
        ],
    }
}

#[test]
fn v1_expanded_macros_are_not_callable_families() {
    check_namespace(Dialect::V1);
}

#[test]
fn legacy_provider_macros_are_not_callable_families() {
    check_namespace(Dialect::Legacy);
}

fn ordinary_declarations(ast: &Ast) -> usize {
    ast.items.iter().filter(|item| matches!(
        &item.kind,
        skull_parser::ItemKind::Function(f) if f.name.name == ustr("ordinary")
    )).count()
}

fn check_namespace(dialect: Dialect) {
    let ast = provider(dialect, "provider.rock");
    let ordinary_count = ordinary_declarations(&ast);
    assert!(ordinary_count > 1, "fixture must exercise an overload family");
    let mut modules = vec![ast];
    if dialect == Dialect::Legacy {
        // A v1 consumer's dependency alias admits the legacy provider view.
        let mut consumer = skull_parser::parse_module_with_dialect(
            "fn entry() {}",
            ustr("consumer.rock"),
            Dialect::V1,
        ).expect("parse v1 consumer");
        consumer.package = Some(rock_core::source_code::PackageTag {
            id: 2,
            self_name: "consumer".to_string(),
        });
        modules.push(consumer);
    }
    let mut ws = Workspace::default();
    build_namespaces(&mut ws, &modules, &aliases()).expect("build namespace");
    let root = ws.ns.package(1).and_then(|pkg| pkg.node(ustr("")))
        .expect("provider namespace root");
    assert!(!root.owned_callables.contains_key(&ustr("identity_value")),
        "{dialect:?}: expansion-owned macro leaked into callable namespace");
    assert!(!root.callable_members.contains_key(&ustr("identity_value")));
    assert!(!ws.defs.values().any(|meta| meta.name == ustr("identity_value")));
    let ordinary = root.owned_callables[&ustr("ordinary")];
    if dialect == Dialect::V1 {
        assert_eq!(ws.defs[&ordinary].kind, DefKind::Callable);
        assert_eq!(root.callable_members[&ustr("ordinary")].len(), ordinary_count);
    } else {
        assert_eq!(ordinary, ustr("ordinary"));
    }
    let checked = semantic_pass_with_aliases(&mut modules, &aliases())
        .expect("macro expansion and ordinary overloads still lower");
    for module in &checked.modules {
        assert!(module.lowering_errors.is_empty(), "{:?}", module.lowering_errors);
        assert!(module.interface_errors.is_empty(), "{:?}", module.interface_errors);
    }
    for (_, inference) in checked.inferences_iter() {
        assert!(inference.errors.is_empty(), "{:?}", inference.errors);
    }
}

#[test]
fn raw_legacy_macro_ownership_excludes_expanded_definitions() {
    let ast = provider(Dialect::Legacy, "legacy.rock");
    let names = legacy_owned_names(&ast);
    assert!(!names.iter().any(|(name, _)| *name == ustr("identity_value")));
    assert_eq!(names.iter().filter(|(name, kind)|
        *name == ustr("ordinary") && *kind == DefKind::Callable).count(),
        ordinary_declarations(&ast));
}

#[test]
fn stdlib_macro_ownership_excludes_expanded_definitions() {
    let ast = provider(Dialect::Legacy, "<stdlib>/macro_provider.rock");
    let mut ws = Workspace::default();
    build_namespaces(&mut ws, &[ast], &NsAliases::default()).expect("stdlib namespace");
    let root = ws.ns.std_root().expect("stdlib root");
    assert!(!root.owned_callables.contains_key(&ustr("identity_value")));
    assert!(root.owned_callables.contains_key(&ustr("ordinary")));
}
