use super::*;

#[test]
fn hover_field_ref_shows_the_referenced_field() {
    let source = r#"struct Mob { health: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    kill_co(mob, .attack_co)
}"#;
    let project = Project::single("hover_field_ref_shows_the_referenced_field", source);
    let reply = project.cli("hover", Some((3, 18)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "attack_co: int? (field of Mob)".trim()
    );
}
