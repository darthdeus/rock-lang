use bone_core::type_sources::declaration_sources;
use ustr::Ustr;

fn workspace(files: &[(&str, &str)]) -> bone_semantic::inference::Workspace {
    let mut modules = files
        .iter()
        .map(|(file, source)| {
            let mut ast = skull_parser::parse_module(source, Ustr::from(file)).unwrap();
            ast.package = Some(skull_parser::PackageTag {
                id: 1,
                self_name: "@/<sources>".into(),
                identity_storage: Default::default(),
                root: String::new(),
            });
            ast
        })
        .collect::<Vec<_>>();
    bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap()
}

#[test]
fn follows_recursive_generic_fields_and_enum_payloads_without_unrelated_owners() {
    let ws = workspace(&[
        (
            "root.rock",
            "struct Root { node: Node<int>?, choice: Choice }",
        ),
        (
            "node.rock",
            "struct Node<T> { next: Node<T>?, item: T, leaf: Leaf }",
        ),
        ("choice.rock", "enum Choice { Empty, Full(Leaf) }"),
        ("leaf.rock", "value Leaf { count: int = 4 }"),
        ("unrelated.rock", "struct Other { value: int }"),
    ]);
    let rows = declaration_sources(&ws, &[Ustr::from("@/<sources>::Root")]).unwrap();
    let owners = rows
        .iter()
        .map(|row| {
            (
                bone_core::modules::display_name(row.name.as_str()),
                row.file.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        vec![
            ("Choice", "choice.rock"),
            ("Leaf", "leaf.rock"),
            ("Node", "node.rock"),
            ("Root", "root.rock"),
        ]
    );
}

#[test]
fn rejects_unknown_and_uninstantiated_generic_roots() {
    let ws = workspace(&[("box.rock", "struct Box<T> { item: T }")]);
    assert!(declaration_sources(&ws, &[Ustr::from("Missing")]).is_err());
    assert!(declaration_sources(&ws, &[Ustr::from("@/<sources>::Box")]).is_err());
}

#[test]
fn discovers_a_new_referenced_file_and_newtype_target() {
    let ws = workspace(&[
        ("root.rock", "struct Root { added: Wrapped? }"),
        ("wrapper.rock", "newtype Wrapped(Added)"),
        ("added.rock", "value Added { count: int = 2 }"),
    ]);
    let rows = declaration_sources(&ws, &[Ustr::from("@/<sources>::Root")]).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.file.as_str()).collect::<Vec<_>>(),
        vec!["added.rock", "root.rock", "wrapper.rock"]
    );
}
