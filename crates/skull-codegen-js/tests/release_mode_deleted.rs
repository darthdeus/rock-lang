#[test]
fn js_emitter_has_no_release_mode_compatibility_surface() {
    let emitter = include_str!("../src/emit.rs");
    assert!(
        !emitter.contains("is_release_mode") && !emitter.contains("__ROCK_RELEASE"),
        "the deleted release-mode hook resurfaced in the JS emitter"
    );
}
