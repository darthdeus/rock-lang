use super::*;

const HOVER_ENUM_VARIANT_PATTERN_NAMES_THE_VARIANT: &str = r#"enum MobKind { Skeleton, Zombie }

fn name(k: MobKind) -> string {
    match k {
        MobKind::Skeleton => "s"
        MobKind::Zombie => "z"
    }
}"#;

#[test]
fn hover_enum_variant_pattern_names_the_variant() {
    let source = HOVER_ENUM_VARIANT_PATTERN_NAMES_THE_VARIANT;
    let project = Project::single("hover_enum_variant_pattern_names_the_variant", source);
    let reply = project.cli("hover", Some((4, 17)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
MobKind::Skeleton
```"#
            .trim()
    );
}

const HOVER_ENUM_QUALIFIER_IN_PATTERN_SHOWS_THE_BODY: &str =
    HOVER_ENUM_VARIANT_PATTERN_NAMES_THE_VARIANT;

#[test]
fn hover_enum_qualifier_in_pattern_shows_the_body() {
    let source = HOVER_ENUM_QUALIFIER_IN_PATTERN_SHOWS_THE_BODY;
    let project = Project::single("hover_enum_qualifier_in_pattern_shows_the_body", source);
    let reply = project.cli("hover", Some((4, 8)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
enum MobKind {
  Skeleton,
  Zombie,
}
```"#
            .trim()
    );
}

#[test]
fn hover_double_colon_in_pattern_shows_the_enum_body() {
    let source = HOVER_ENUM_QUALIFIER_IN_PATTERN_SHOWS_THE_BODY;
    let project = Project::single("hover_double_colon_in_pattern_shows_the_enum_body", source);
    let reply = project.cli("hover", Some((4, 15)), &[]);
    assert_eq!(
        string(&reply["hover"]["contents"]).trim(),
        r#"```rock
enum MobKind {
  Skeleton,
  Zombie,
}
```"#
            .trim()
    );
}
