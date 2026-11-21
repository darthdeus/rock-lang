//! GLSL local bindings must not shadow builtin calls.

use crate::support::emit_library;

fn assert_no_bare_binding(glsl: &str, name: &str) {
    for pat in [
        format!(" {name};"),
        format!(" {name},"),
        format!(" {name})"),
    ] {
        assert!(
            !glsl.contains(&pat),
            "a binding of `{name}` shadows the builtin:\n{glsl}"
        );
    }
}

/// A helper local named `cos` beside a `cos(...)` call: helpers emit with an
/// empty reserved set, so this is the unguarded allocator path.
#[test]
fn helper_local_named_cos_renamed_apart_from_builtin() {
    let src = r#"fn spin(angle: float) -> float {
        let cos = angle.cos()
        return cos + (angle + 1.0).cos()
    }"#;
    let glsl = emit_library(src, &["spin"]).expect("emit");
    assert!(
        glsl.contains("cos(angle)"),
        "the builtin call must survive:\n{glsl}"
    );
    assert_no_bare_binding(&glsl, "cos");
    assert!(
        glsl.contains("cos_1 = cos(angle)"),
        "the renamed local must hold the builtin result:\n{glsl}"
    );
}
