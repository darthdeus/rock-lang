use super::*;

#[test]
fn hover_param_type_shows_struct_def() {
    let source = r#"struct Mob {
    hp: int,
}

fn describe(m: Mob) {
}"#;
    let project = Project::single("hover_param_type_shows_struct_def", source);
    let reply = project.cli("hover", Some((4, 15)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"struct Mob {
  hp: int,
}"#
        .trim()
    );
}
