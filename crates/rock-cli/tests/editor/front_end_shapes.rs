use super::*;

#[test]
fn complete_macro_with_qualified_type_shapes() {
    let source = r#"macro typed_signature(
    a: ref ::demo::Item,
    b: demo::Box<int>,
    c: (ref int)?,
    d: [::demo::Item]
) -> ::demo::Box<int> { 1 }
fn main() {
    typed_
}"#;
    let project = Project::single("complete_macro_with_qualified_type_shapes", source);
    let reply = project.cli("completion", Some((7, 10)), &[]);
    completion_labels(&reply, &["typed_signature"]);
}

#[test]
fn references_follow_nested_macro_path_patterns() {
    let source = r#"macro choose(Name: ident, value: expr) {
    match value {
        Outer::Wrap(Name::Leaf) => 1,
        Name::Record { child: Name::Leaf } => 2,
        _ => 0
    }
}
fn main() {}"#;
    let project = Project::single("references_follow_nested_macro_path_patterns", source);
    let reply = project.cli("references", Some((0, 13)), &[]);
    reference_starts(&reply["references"], &[(0, 13), (2, 20), (3, 8), (3, 30)]);
}
