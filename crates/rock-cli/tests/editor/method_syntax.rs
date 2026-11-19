use super::*;

#[test]
fn hover_dform_receiver_param() {
    let source = r#"struct Mob { hp: int }

fn Mob.take_damage(m, amount: int) -> int {
    return m.hp - amount
}"#;
    let project = Project::single("hover_dform_receiver_param", source);
    let reply = project.cli("hover", Some((3, 11)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "Type: struct Mob".trim()
    );
}

#[test]
fn goto_definition_dform_call() {
    let source = r#"struct Mob { hp: int }

fn Mob.take_damage(m, amount: int) -> int {
    return m.hp - amount
}

fn main() {
    let mob = Mob { hp: 10 }
    print(mob.take_damage(3))
}"#;
    let project = Project::single("goto_definition_dform_call", source);
    let reply = project.cli("definition", Some((8, 15)), &[]);
    assert_eq!(reply["definitions"][0]["range"]["start"]["line"], 2);
    assert_eq!(reply["definitions"][0]["range"]["start"]["character"], 7);
}
