use skull_fmt::{FormatConfig, try_format};

#[test]
fn path_qualified_return_literal_reparses_and_is_idempotent() {
    let source = "fn profile(mob: Mob) {\n    if mob.active { return app_world::world_tick::WorldActorTurnProfile { move_speed: mob.move_speed() } }\n}\n";
    let formatted = try_format(source, &FormatConfig::default()).expect("format source");
    skull_parser::parse_module_clean(&formatted, "<formatted>".into())
        .unwrap_or_else(|error| panic!("formatted literal must parse: {error:?}\n{formatted}"));
    let parse_v1 = |text: &str| {
        skull_parser::parse_module_clean_with_dialect(
            text,
            "<path-literal>".into(),
            skull_parser::Dialect::V1,
        )
        .expect("qualified literal must parse under the path grammar")
        .module_sexp()
    };
    assert_eq!(parse_v1(source), parse_v1(&formatted), "{formatted}");
    assert_eq!(
        try_format(&formatted, &FormatConfig::default()).unwrap(),
        formatted
    );
}

#[test]
fn qualified_call_keeps_path_separators_tight() {
    let source = "fn probe() {\n    app_world::world_tick::update(actor)\n}\n";
    let formatted = try_format(source, &FormatConfig::default()).expect("format source");
    assert!(
        formatted.contains("app_world::world_tick::update(actor)"),
        "{formatted}"
    );
    skull_parser::parse_module_clean(&formatted, "<formatted>".into())
        .expect("formatted call must parse");
    assert_eq!(
        try_format(&formatted, &FormatConfig::default()).unwrap(),
        formatted
    );
}

#[test]
fn legacy_ascription_keeps_its_spacing_and_meaning() {
    let source = "fn ratio(value: int) {\n    let wide = value  ::  float\n    let tight = value::float\n    let sum = (value + 1) :: float\n}\n";
    let formatted = try_format(source, &FormatConfig::default()).expect("format source");
    assert!(formatted.contains("value :: float"), "{formatted}");
    assert!(formatted.contains("value::float"), "{formatted}");
    let parse = |text: &str| {
        skull_parser::parse_module_clean(text, "<legacy-ascription>".into())
            .expect("legacy ascriptions must parse")
            .module_sexp()
    };
    assert_eq!(parse(source), parse(&formatted));
    assert_eq!(
        try_format(&formatted, &FormatConfig::default()).unwrap(),
        formatted
    );
}
