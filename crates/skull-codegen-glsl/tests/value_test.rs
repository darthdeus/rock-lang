//! `@test(returns = ...)` shaders: a parameterless scalar root wrapped in a
//! fragment stage that writes its value to one output.

use crate::support::{assert_has, workspace};
use skull_codegen_glsl::{GlslProfile, ValueTestRet, emit_value_test};

#[test]
fn scalar_returns_write_one_typed_output() {
    for (src, ret, output, store) in [
        (
            "fn t() -> int { return min(3, 5) }",
            ValueTestRet::Int,
            "layout(location = 0) out int _rock_value_out;",
            "_rock_value_out = t_0();",
        ),
        (
            "fn t() -> float { return clamp(9.0, 0.0, 5.0) }",
            ValueTestRet::Float,
            "layout(location = 0) out float _rock_value_out;",
            "_rock_value_out = t_0();",
        ),
        (
            "fn t() -> bool { return 3.0 == 3.0 }",
            ValueTestRet::Bool,
            "layout(location = 0) out int _rock_value_out;",
            "_rock_value_out = (t_0() ? 1 : 0);",
        ),
    ] {
        let (ws, mid) = workspace(src);
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ret, "{src}");
        assert!(
            s.fragment.starts_with("#version 330 core\n"),
            "{}",
            s.fragment
        );
        assert_has(&s.fragment, output);
        assert_has(&s.fragment, store);
        assert_has(&s.fragment, &format!("{} t_0() {{", ret.tag()));
        assert_has(&s.vertex, "gl_VertexID");
    }
}

#[test]
fn roots_outside_the_value_class_reject() {
    for (src, reason) in [
        ("fn t(x: int) -> int { return x }", "no parameters"),
        ("fn t() -> Vec2 { return v(1.0, 2.0) }", "not a scalar"),
        // A string is well above tier 0: the profile pass names the construct
        // rather than emitting a guess.
        (
            "fn t() -> int { let s = \"hi\"\n return 1 }",
            "string literal",
        ),
    ] {
        let (ws, mid) = workspace(src);
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err(src);
        assert!(e.message.contains(reason), "{src}: {}", e.message);
    }
}
