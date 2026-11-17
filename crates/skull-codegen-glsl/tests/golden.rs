use crate::support::{assert_has, emit_checked_entry as emit_entry, emit_library as emit};

fn golden(src: &str, root: &str, contains: &[&str], lacks: &[&str]) {
    let glsl = emit(src, &[root]).expect("emit");
    for expected in contains {
        assert_has(&glsl, expected);
    }
    for unexpected in lacks {
        assert!(
            !glsl.contains(unexpected),
            "unexpected {unexpected}:\n{glsl}"
        );
    }
}

#[test]
fn a_user_fn_named_like_an_intrinsic_is_its_own_function() {
    let src = r#"
        @pure fn abs(b: bool) -> int { if b { return 5 } return 7 }
        @pure fn root() -> int { return abs(true) * 10 + abs(false) }
    "#;
    let glsl = emit(src, &["root"]).expect("emit");
    assert_has(&glsl, "int abs_bool(bool b) {");
    assert_has(&glsl, "return ((abs_bool(true) * 10) + abs_bool(false));");
}

#[test]
fn fragment_call_swizzles_evaluate_each_call_once() {
    let src = "fn shade(p: Vec2) -> Vec4 { return v(sin(p.x), cos(p.y), p.x * p.y, 1.0) }
        @shader(fragment) fn sampled(scene: Sampler2D, tc: Vec2) -> Vec4 {
            let uv = tc * 0.5
            let base = texture(scene, tc).rgb
            let offset = texture(scene, uv).xy
            let tint = shade(tc).rgb
            let warp = shade(uv).yx
            return v(base * tint, offset.x + offset.y + warp.x * warp.y)
        }";
    let glsl = emit_entry(src, "sampled").expect("emit");
    for call in [
        "texture(scene, tc)",
        "texture(scene, uv)",
        "shade_Vec2(tc)",
        "shade_Vec2(uv)",
    ] {
        assert_eq!(glsl.matches(call).count(), 1, "{call}:\n{glsl}");
    }
}

#[test]
fn param_loop_bound_not_folded() {
    let src = r#"fn f(n: int) -> int {
        let acc = 0
        for i in 0..n { acc = acc + i }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(
        glsl.contains("t3 = n;"),
        "param bound must stay a temp:\n{glsl}"
    );
    assert_has(&glsl, "for (; i < t3; i = i + 1) {");
}

cases! { golden:
    match_int_ladder(
        r#"@pure fn f(n: int) -> int {
            match n { 1 => { return 10 } 2 => { return 20 } _ => { return 0 } }
        }"#,
        "f",
        &[
            "if ((n == 1)) {\n    return 10;\n  } else {\n    if ((n == 2)) {\n      return 20;\n    } else {\n      return 0;\n    }\n  }",
        ], &[],
    );
}

#[test]
fn exhaustive_bool_match_expression_structures() {
    let src = r#"@pure fn f(b: bool) -> int {
        let x = match b { true => 10, false => 20 }
        return x + 1
    }"#;
    let glsl = emit(src, &["f"]).expect("an exhaustive bool match with a real join must structure");
    let t = glsl.find("true").expect("the true pattern must survive");
    let arm10 = glsl.find("= 10;").expect("the true arm must survive");
    let f = glsl.find("false").expect("the false pattern must survive");
    let arm20 = glsl.find("= 20;").expect("the false arm must survive");
    let ret = glsl.find("return").expect("the join must survive");
    assert!(
        t < arm10 && arm10 < f && f < arm20 && arm20 < ret,
        "each pattern must guard its own arm ahead of the join; got:\n{glsl}"
    );
    assert_eq!(
        glsl.matches("return").count(),
        1,
        "the join must be walked once; got:\n{glsl}"
    );
}

#[test]
fn match_expression_ladder_is_not_folded() {
    let src = r#"@pure fn f(n: int) -> int {
        let x = match n { 1 => 10, 2 => 20, _ => 30 }
        return x + 1
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(
        glsl.contains("t1 = 10;"),
        "n == 1 arm must survive; got:\n{glsl}"
    );
    assert!(
        !glsl.contains("(n == 2) ? 20 : 30"),
        "inner diamond must not fold past the outer arm's store; got:\n{glsl}"
    );
}

cases! { golden:
    nested_while_loops(
        r#"fn f(n: int) -> int {
            let acc = 0
            let i = 0
            while i < n {
                let j = 0
                while j < i {
                    if j == 3 { break }
                    acc = acc + 1
                    j = j + 1
                }
                i = i + 1
            }
            return acc
        }"#,
        "f",
        &[
            "int acc;\n  int i;\n  int j;\n  acc = 0;\n  i = 0;\n  while (true) {\n    if (!((i < n))) { break; }\n    j = 0;\n    while (true) {\n      if (!((j < i))) { break; }\n      if ((j == 3)) {\n        break;\n      }\n      acc = (acc + 1);\n      j = (j + 1);\n    }\n    i = (i + 1);\n  }\n  return acc;",
        ], &[],
    );

    int_literal_float_contexts(
        r#"
            fn root(x: float) -> float {
                let y = 0.5
                if x > 0 {
                    y = x + 1
                }
                if x == 3 {
                    y = sin(2)
                }
                return 0
            }
        "#,
        "root",
        &[
            "float root_float(float x)",
            "if ((x > 0.0)) {\n    y = (x + 1.0);\n  }",
            "if ((x == 3.0)) {\n    y = sin(2.0);\n  }",
            "return 0.0;",
        ], &[],
    );
}

#[test]
fn int_param_returned_from_float_fn() {
    let src = r#"@pure fn f(i: int) -> float { return i }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(glsl.trim(), "float f_int(int i) {\n  return float(i);\n}");
}

#[test]
fn int_arg_at_float_param() {
    let src = r#"
        @pure fn takes_float(x: float) -> float { return x }
        @pure fn f(i: int) -> float { return takes_float(i) }
    "#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_has(&glsl, "return takes_float_float(float(i));");
}

cases! { golden:
    int_in_mixed_arithmetic(
        r#"@pure fn f(x: float, i: int) -> float { return x + i }"#,
        "f",
        &["return (x + float(i));"], &[],
    );

    let_float_annotation_cell_typing(
        r#"fn f() -> float { let y: float = 0  return y }"#,
        "f",
        &["float y;\n  y = 0.0;\n  return y;"], &[],
    );
}

#[test]
fn log2_float_on_desktop_profile() {
    let src = r#"
        @shader(fragment)
        fn logfrag(scene: Sampler2D, tc: Vec2) -> Vec4 {
            let t = texture(scene, tc)
            return v(log2(t.r + 1.0), log2(t.g + 1.0), log2(t.b + 1.0), t.a)
        }
    "#;
    let glsl = emit_entry(src, "logfrag").expect("emit");
    assert!(glsl.contains("log2("), "must use the log2 builtin:\n{glsl}");
    assert!(!glsl.contains("null"), "must not mention null:\n{glsl}");
}

#[test]
fn helper_float_default_is_carried_on_desktop_profile() {
    let src = r#"
        @pure fn shade(c: Vec4, gain: float = 1.0) -> Vec4 {
            return v(c.r * gain, c.g * gain, c.b * gain, c.a)
        }
        @shader(fragment)
        fn shadefrag(scene: Sampler2D, tc: Vec2) -> Vec4 {
            return shade(texture(scene, tc))
        }
        @shader(fragment)
        fn shadefrag_exp(scene: Sampler2D, tc: Vec2) -> Vec4 {
            return shade(texture(scene, tc), 2.0)
        }
    "#;
    let omitted = emit_entry(src, "shadefrag").expect("emit");
    assert!(
        omitted.contains("shade_Vec4_float(texture(scene, tc), 1.0)"),
        "omitted call must carry the default:\n{omitted}"
    );
    let explicit = emit_entry(src, "shadefrag_exp").expect("emit");
    assert!(
        explicit.contains("shade_Vec4_float(texture(scene, tc), 2.0)"),
        "explicit call must keep its argument:\n{explicit}"
    );
    for glsl in [&omitted, &explicit] {
        assert!(!glsl.contains("null"), "must not mention null:\n{glsl}");
    }
}

#[test]
fn glsl_emits_no_double_underscore_for_lua_escaped_names_or_idiv() {
    let src = r#"
        @pure fn compute(a: int, b: int) -> int {
            let local = a ./ b
            return local
        }
    "#;
    let glsl = emit(src, &["compute"]).expect("must emit GLSL");
    assert!(
        !glsl.contains("__"),
        "GLSL identifiers must not contain reserved double underscores:\n{glsl}"
    );
    assert!(
        glsl.contains("rock_idiv"),
        "GLSL floored division helper must be emitted:\n{glsl}"
    );
}

#[test]
fn conditionally_reassigned_sample() {
    let src = r#"
        @shader(fragment)
        fn frag(scene: Sampler2D, tc: Vec2) -> Vec4 {
            let color = texture(scene, tc)
            if color.w < 0.5 {
                color = Vec4 { x: color.x * 0.5, y: color.y, z: 0.0, w: 1.0 }
            }
            return color
        }
    "#;
    let glsl = emit_entry(src, "frag").expect("emit");
    assert_has(&glsl, "vec4 color;");
    assert_has(
        &glsl,
        "color = texture(scene, tc);\n  if ((color.w < 0.5)) {\n    color = vec4((color.x * 0.5), color.y, 0.0, 1.0);\n  }\n  return color;",
    );
}

cases! { golden:
    value_local_and_param_field_writes_replace_the_whole_value(
        r#"
            fn nudge(p: Vec2, d: float) -> float {
                let q = p
                q.y += d
                p.x *= 2.0
                let old = q.x++
                return old + q.x + q.y + p.x + p.y
            }
        "#,
        "nudge",
        &[
            "float nudge_Vec2_float(vec2 p, float d) {\n  vec2 q;\n  float t3;\n  float old;",
            "q = p;\n  q = vec2(q.x, (q.y + d));\n  p = vec2((p.x * 2.0), p.y);\n  t3 = q.x;\n  q = vec2((q.x + 1.0), q.y);\n  old = t3;",
            "return ((((old + q.x) + q.y) + p.x) + p.y);",
        ], &[],
    );
}
