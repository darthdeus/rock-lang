use super::*;

const HOVER_STRUCT_NAME_IN_LITERAL: &str = r#"struct Player {
    name: string,
    health: int,
}

fn main() {
    let p = Player { name: "foo", health: 100 }
}"#;

#[test]
fn hover_struct_name_in_literal() {
    let source = HOVER_STRUCT_NAME_IN_LITERAL;
    let project = Project::single("hover_struct_name_in_literal", source);
    let reply = project.cli("hover", Some((6, 12)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"struct Player {
  name: string,
  health: int,
}"#
        .trim()
    );
}

const HOVER_FIELD_IN_STRUCT_LITERAL: &str = HOVER_STRUCT_NAME_IN_LITERAL;

#[test]
fn hover_field_in_struct_literal() {
    let source = HOVER_FIELD_IN_STRUCT_LITERAL;
    let project = Project::single("hover_field_in_struct_literal", source);
    let reply = project.cli("hover", Some((6, 21)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "name: string (field of Player)".trim()
    );
}

#[test]
fn hover_second_field_in_struct_literal() {
    let source = HOVER_FIELD_IN_STRUCT_LITERAL;
    let project = Project::single("hover_second_field_in_struct_literal", source);
    let reply = project.cli("hover", Some((6, 34)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "health: int (field of Player)".trim()
    );
}
