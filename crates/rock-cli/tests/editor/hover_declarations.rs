use super::*;

#[test]
fn hover_struct_field_declaration_name() {
    let source = r#"struct Mob {
    pos: FixVec2,
    dir: int,
}

struct FixVec2 { x: float, y: float }"#;
    let project = Project::single("hover_struct_field_declaration_name", source);
    let reply = project.cli("hover", Some((1, 4)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        "pos: FixVec2 (field of Mob)".trim()
    );
}

const HOVER_ENUM_DECLARATION_NAME_SHOWS_FULL_BODY: &str = r#"enum SpellState {
    Idle,
    Casting(SpellKind, float, float),
    Loaded(SpellKind),
}

enum SpellKind { Fire, Ice }"#;

#[test]
fn hover_enum_declaration_name_shows_full_body() {
    let source = HOVER_ENUM_DECLARATION_NAME_SHOWS_FULL_BODY;
    let project = Project::single("hover_enum_declaration_name_shows_full_body", source);
    let reply = project.cli("hover", Some((0, 10)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
enum SpellState {
  Idle,
  Casting(SpellKind, float, float),
  Loaded(SpellKind),
}
```"#
            .trim()
    );
}

#[test]
fn hover_enum_variant_declaration_name_shows_the_variant() {
    let source = HOVER_ENUM_DECLARATION_NAME_SHOWS_FULL_BODY;
    let project = Project::single(
        "hover_enum_variant_declaration_name_shows_the_variant",
        source,
    );
    let reply = project.cli("hover", Some((2, 4)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
SpellState::Casting(SpellKind, float, float)
```"#
            .trim()
    );
}
