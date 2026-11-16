use bone_core::modules::{DefKind, NsAliases, PackageAliases};
use bone_core::prelude::*;
use bone_semantic::semantic_passes::{build_namespaces, semantic_pass_with_aliases};
use skull_parser::{Ast, Dialect};

const SOURCE: &str = include_str!("fixtures/cw7_macro_namespace_kind.rock");

fn provider(dialect: Dialect, file: &str) -> Ast {
    let mut ast = skull_parser::parse_module_with_dialect(SOURCE, ustr(file), dialect)
        .expect("parse macro namespace fixture");
    assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
    ast.package = Some(rock_core::source_code::PackageTag {
        id: 1,
        self_name: "provider".to_string(),
        identity_storage: Default::default(),
        root: String::new(),
    });
    ast
}

fn aliases() -> NsAliases {
    NsAliases {
        packages: vec![PackageAliases {
            id: 1,
            self_name: ustr("provider"),
            aliases: vec![],
        }],
    }
}

fn ordinary_declarations(ast: &Ast) -> usize {
    ast.items
        .iter()
        .filter(|item| {
            matches!(
                &item.kind,
                skull_parser::ItemKind::Function(f) if f.name.name == ustr("ordinary")
            )
        })
        .count()
}

#[test]
fn v1_expanded_macros_are_not_callable_families() {
    let ast = provider(Dialect::V1, "provider.rock");
    let ordinary_count = ordinary_declarations(&ast);
    assert!(
        ordinary_count > 1,
        "fixture must exercise an overload family"
    );
    let mut modules = vec![ast];
    let mut ws = Workspace::default();
    build_namespaces(&mut ws, &modules, &aliases()).expect("build namespace");
    let root = ws
        .ns
        .package(1)
        .and_then(|pkg| pkg.node(ustr("")))
        .expect("provider namespace root");
    assert!(
        !root.owned_callables.contains_key(&ustr("identity_value")),
        "expansion-owned macro leaked into callable namespace"
    );
    assert!(!root.callable_members.contains_key(&ustr("identity_value")));
    assert!(
        !ws.defs
            .values()
            .any(|meta| meta.name == ustr("identity_value"))
    );
    let ordinary = root.owned_callables[&ustr("ordinary")];
    assert_eq!(ws.defs[&ordinary].kind, DefKind::Callable);
    assert_eq!(
        root.callable_members[&ustr("ordinary")].len(),
        ordinary_count
    );
    let checked = semantic_pass_with_aliases(&mut modules, &aliases())
        .expect("macro expansion and ordinary overloads still lower");
    for module in &checked.modules {
        assert!(
            module.lowering_errors.is_empty(),
            "{:?}",
            module.lowering_errors
        );
        assert!(
            module.interface_errors.is_empty(),
            "{:?}",
            module.interface_errors
        );
    }
    for (_, inference) in checked.inferences_iter() {
        assert!(inference.errors.is_empty(), "{:?}", inference.errors);
    }
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
