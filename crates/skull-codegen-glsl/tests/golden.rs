use crate::support::{assert_has, emit_library as emit};

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

cases! { golden:
    repeated_vector_operator_stays_inline(
        "fn f(p: Vec2) -> Vec2 { return (p + v(1.0, 2.0)).xx }",
        "f",
        &["return vec2((p + vec2(1.0, 2.0)).x, (p + vec2(1.0, 2.0)).x);"],
        &["call_result"],
    );
}

fn call_once(src: &str, root: &str, call: &str, expression: &str) {
    let glsl = emit(src, &[root]).expect("emit");
    assert_eq!(glsl.matches(call).count(), 1, "{glsl}");
    assert_has(&glsl, expression);
}

cases! { call_once:
    folded_call_swizzle_evaluates_once(
        "fn source(t: float) -> float { return sin(t) }
         fn selected(flag: bool, t: float) -> Vec2 {
             return v(if flag { source(t) } else { 0.0 }, 1.0).xx
         }",
        "selected",
        "source_float(t)",
        "flag ? source_float(t) : 0.0",
    );
}

#[test]
fn retained_call_stays_in_its_if_arm() {
    let glsl = emit(
        "fn source(p: Vec2) -> Vec4 { return v(sin(p.x), cos(p.y), 0.0, 1.0) }
         fn choose(flag: bool, p: Vec2) -> Vec2 {
             let result = if flag { source(p).yx } else { v(0.0, 1.0) }
             return result
         }",
        &["choose"],
    )
    .expect("emit");
    let body = glsl.split("vec2 choose_bool_Vec2(").nth(1).expect(&glsl);
    let branch = body.find("if (flag) {").expect(&glsl);
    let call = body.find("= source_Vec2(p);").expect(&glsl);
    let otherwise = body.find("} else {").expect(&glsl);
    assert!(branch < call && call < otherwise, "{glsl}");
    assert_eq!(body.matches("source_Vec2(p)").count(), 1, "{glsl}");
}

cases! { golden:
    param_loop_bound_not_folded(
        r#"fn f(n: int) -> int {
            let acc = 0
            for i in 0..n { acc = acc + i }
            return acc
        }"#,
        "f",
        &["t3 = n;", "for (; i < t3; i = i + 1) {"],
        &[],
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

cases! { golden:
    match_expression_ladder_is_not_folded(
        r#"@pure fn f(n: int) -> int {
            let x = match n { 1 => 10, 2 => 20, _ => 30 }
            return x + 1
        }"#,
        "f",
        &["t1 = 10;"],
        &["(n == 2) ? 20 : 30"],
    );

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

cases! { golden:
    int_arg_at_float_param(
        r#"
            @pure fn takes_float(x: float) -> float { return x }
            @pure fn f(i: int) -> float { return takes_float(i) }
        "#,
        "f",
        &["return takes_float_float(float(i));"],
        &[],
    );

    glsl_emits_no_double_underscore_for_lua_escaped_names_or_idiv(
        r#"
            @pure fn compute(a: int, b: int) -> int {
                let local = a ./ b
                return local
            }
        "#,
        "compute",
        &["rock_idiv"],
        &["__"],
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
