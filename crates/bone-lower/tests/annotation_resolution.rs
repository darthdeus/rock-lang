use bone_core::hir::{Annotation, Module};
use bone_core::modules::{
    DefKind, DefMeta, FragmentScope, NsNode, NsPackage, NsVisibility, UsePathRef,
};
use bone_core::workspace::Workspace;
use bone_lower::{LowerCtx, lower_annotations_with_ctx};
use skull_parser::{
    CompilerError, Dialect, ItemKind, UseSuffix, UseTree, parse_module_clean_with_dialect,
};
use ustr::Ustr;

fn lower_field_annotation(left: &str, right: &str) -> Result<Vec<Annotation>, CompilerError> {
    lower_annotation(
        left,
        right,
        "use left::*; use right::*; struct Buffer { @size(4) values: int }",
    )
}

fn lower_annotation(
    left: &str,
    right: &str,
    source: &str,
) -> Result<Vec<Annotation>, CompilerError> {
    let file = Ustr::from("annotation.rock");
    let ast = parse_module_clean_with_dialect(source, file, Dialect::V1).unwrap();
    let mut ws = Workspace::new();
    let mut pkg = NsPackage {
        id: 1,
        self_name: Ustr::from("example"),
        ..Default::default()
    };
    let mut root = NsNode::default();
    let mut frag = FragmentScope {
        package: 1,
        package_name: pkg.self_name,
        ..Default::default()
    };
    for (module, source) in [("left", left), ("right", right)] {
        let name = Ustr::from(module);
        let parsed = parse_module_clean_with_dialect(source, name, Dialect::V1).unwrap();
        let item = &parsed.items[0];
        let kind = match &item.kind {
            ItemKind::Function(_) => DefKind::Callable,
            ItemKind::Decorator(_) => DefKind::Decorator,
            ItemKind::Global(_) => DefKind::Global,
            _ => panic!("expected a callable, decorator or global"),
        };
        let key = Ustr::from(&format!("example::{module}::size"));
        let spelling = Ustr::from("size");
        let mut node = NsNode {
            path_key: name,
            segments: vec![name],
            ..Default::default()
        };
        match kind {
            DefKind::Callable => {
                node.owned_callables.insert(spelling, key);
            }
            DefKind::Decorator => {
                node.owned_decorators.insert(spelling, key);
            }
            DefKind::Global => {
                node.owned_globals.insert(spelling, key);
            }
            _ => unreachable!(),
        }
        ws.defs.insert(
            key,
            DefMeta {
                package: 1,
                module: vec![name],
                node: name,
                kind,
                name: spelling,
                vis: NsVisibility::Pub,
                span: item.span,
                dialect: Dialect::V1,
            },
        );
        root.children.insert(name, name);
        pkg.nodes.insert(name, node);
    }
    for item in &ast.items {
        if let ItemKind::Use(import) = &item.kind {
            let UseTree::Path {
                from_root,
                segments,
                kind: UseSuffix::Glob,
            } = &import.tree
            else {
                panic!("expected a glob import");
            };
            frag.globs.push(UsePathRef {
                from_root: *from_root,
                segments: segments.iter().map(|s| s.name).collect(),
            });
        }
    }
    pkg.nodes.insert(Ustr::from(""), root);
    ws.ns.packages.insert(1, pkg);
    ws.fragment_scopes.insert(file, frag);
    let mut module = Module {
        file,
        ..Default::default()
    };
    let ctx = LowerCtx {
        workspace: &ws,
        module: &mut module,
        generics_in_scope: Default::default(),
        _next_var_id: 0,
        placeholder_param: None,
        placeholder_forbidden: false,
        scope_probes: Default::default(),
        resolutions: Default::default(),
        place_temps: Vec::new(),
    };
    let item = ast.items.last().unwrap();
    let annotations = match &item.kind {
        ItemKind::Struct(buffer) => &buffer.fields[0].annotations,
        ItemKind::Function(_) => &item.annotations,
        _ => panic!("expected a struct or function"),
    };
    lower_annotations_with_ctx(&ctx, &ast, annotations)
}

#[test]
fn builtin_size_ignores_glob_imported_functions() {
    let annotations = lower_field_annotation("pub fn size() {}", "pub fn size() {}").unwrap();
    assert!(annotations[0].is_builtin("size"));
}

#[test]
fn builtin_size_ignores_glob_imported_globals() {
    let annotations =
        lower_field_annotation("pub global size: int = 1", "pub global size: int = 2").unwrap();
    assert!(annotations[0].is_builtin("size"));
}

#[test]
fn glob_imported_decorators_remain_ambiguous() {
    let error = lower_field_annotation("pub deco size() {}", "pub deco size() {}").unwrap_err();
    let diagnostic = format!("{error:?}");
    assert!(diagnostic.contains("ambiguous"), "{diagnostic}");
    assert!(diagnostic.contains("example::left::size"), "{diagnostic}");
    assert!(diagnostic.contains("example::right::size"), "{diagnostic}");
}

#[test]
fn only_the_decorator_claims_a_mixed_glob_spelling() {
    let annotations = lower_field_annotation("pub deco size() {}", "pub fn size() {}").unwrap();
    assert_eq!(
        annotations[0].resolved_name,
        Some(Ustr::from("example::left::size"))
    );
}

#[test]
fn annotation_lookup_retains_broken_glob_errors() {
    let error = lower_annotation(
        "pub fn size() {}",
        "pub fn size() {}",
        "use missing::*; struct Buffer { @size(4) values: int }",
    )
    .unwrap_err();
    assert!(error.message.contains("missing"), "{error:?}");
}

#[test]
fn wire_onchange_still_rejects_missing_and_ambiguous_names() {
    for name in ["missing", "size"] {
        let source = format!("use left::*; use right::*; @wire(onchange=[{name}]) fn probe() {{}}");
        let error = lower_annotation("pub fn size() {}", "pub fn size() {}", &source).unwrap_err();
        let diagnostic = if name == "missing" {
            "did not resolve"
        } else {
            "not a single definition"
        };
        assert!(error.message.contains(diagnostic), "{error:?}");
    }
}
