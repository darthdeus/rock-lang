use super::*;

#[test]
fn complete_struct_literal_fields() {
    let source = "struct FixVec2 { x: float, y: float }\n\nfn run() {\n    let v = FixVec2 { \n}";
    let project = Project::single("complete_struct_literal_fields", source);
    let reply = project.cli("completion", Some((3, 22)), &[]);
    completion_labels(&reply, &["x: float", "y: float"]);
}

#[test]
fn complete_member_includes_fields_and_ufcs_methods() {
    let source = r#"struct FixVec2 { x: float, y: float }

fn dist(a: FixVec2) -> float { a.x }

fn run() {
    let v = FixVec2 { x: 1.0, y: 2.0 }
    v.
}"#;
    let project = Project::single("complete_member_includes_fields_and_ufcs_methods", source);
    let reply = project.cli("completion", Some((6, 6)), &[]);
    completion_labels(&reply, &["x: float", "y: float", "dist"]);
}

#[test]
fn complete_member_ufcs_method_defined_later_in_file() {
    let source = r#"struct FixVec2 { x: float, y: float }

fn run() {
    let v = FixVec2 { x: 1.0, y: 2.0 }
    v.
}

fn dist(a: FixVec2) -> float { a.x }"#;
    let project = Project::single("complete_member_ufcs_method_defined_later_in_file", source);
    let reply = project.cli("completion", Some((4, 6)), &[]);
    completion_labels(&reply, &["x: float", "y: float", "dist"]);
}
