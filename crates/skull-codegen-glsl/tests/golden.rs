//! Golden tests: Rock snippet -> expected GLSL text, over the real stdlib.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// The stdlib with `Vec2` marked `nobox`. `nobox` is a whole-language guarantee,
/// and a tier-0 `vecN` is the type anyone would mark first, so the GLSL surface
/// has to carry one. Built here rather than in `stdlib/vec.rock` so the fixture
/// does not depend on whether the real stdlib has opted in yet.
static STDLIB_NOBOX_VEC2: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let src = f.contents.replace("\nvalue Vec2 {", "\nnobox value Vec2 {");
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(&src, id).expect("stdlib parse")
        })
        .collect()
});

/// Emit GLSL for `roots` from `src` (the `<test>` module).
fn emit(src: &str, roots: &[&str]) -> Result<String, String> {
    emit_with(&STDLIB, src, roots)
}

fn emit_with(
    stdlib: &[skull_parser::Ast],
    src: &str,
    roots: &[&str],
) -> Result<String, String> {
    let mut modules = stdlib.to_vec();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    // `semantic_pass` returns Ok with per-body errors recorded in `ws.errors`;
    // the CLI reports those and refuses to emit. Without this check the goldens
    // pin emission for sources the real pipeline rejects (backlog #67).
    let errs = skull_mir::verdict::body_errors(&ws);
    if !errs.is_empty() {
        return Err(errs.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; "));
    }
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    let root_refs = roots
        .iter()
        .map(|root| skull_codegen_glsl::resolve_root(&ws, mid, root))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.message)?;
    let session = skull_codegen_glsl::shader_library_session(&ws, root_refs)
        .map_err(|e| e.message)?;
    skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        .map_err(|e| e.message)
}

#[test]
fn function_library_preserves_no_version_contract() {
    let glsl = emit(
        "fn helper(value: float) -> float { return value + 1.0 }",
        &["helper"],
    )
    .expect("emit");
    assert!(
        !glsl.starts_with("#version"),
        "route-B function libraries must not select a GLSL profile:\n{glsl}"
    );
}

#[test]
fn function_library_requires_concrete_body_plans() {
    let src = r#"
        @pure fn identity<T>(value: T) -> T { return value }
        @pure fn root(value: float) -> float { return identity(value) }
    "#;
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>" )).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let root = skull_codegen_glsl::resolve_root(&ws, mid, "root").expect("root");
    let session = skull_codegen_glsl::shader_library_session(&ws, vec![root]).expect("session");
    let mut poisoned = session.view.clone();
    assert!(!poisoned.concrete_plans.is_empty(), "fixture must retain a concrete plan population");
    std::sync::Arc::make_mut(&mut poisoned.concrete_plans).clear();
    let error = skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &poisoned)
        .expect_err("emission must consume the concrete plan population");
    assert!(error.message.contains("concrete plan"), "unexpected error: {}", error.message);
}

#[test]
fn scalar_remap() {
    let src = r#"
        @pure fn remap(value: float, min1: float, max1: float, min2: float, max2: float) -> float {
            return min2 + (value - min1) * (max2 - min2) / (max1 - min1)
        }
    "#;
    let glsl = emit(src, &["remap"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float remap_float_float_float_float_float(float value, float min1, float max1, float min2, float max2) {\n  \
         return (min2 + (((value - min1) * (max2 - min2)) / (max1 - min1)));\n}"
    );
}

#[test]
fn callee_before_caller() {
    let src = r#"
        @pure fn remap(value: float, min1: float, max1: float, min2: float, max2: float) -> float {
            return min2 + (value - min1) * (max2 - min2) / (max1 - min1)
        }
        @pure fn map01(value: float, min1: float, max1: float) -> float {
            return remap(value, min1, max1, 0.0, 1.0)
        }
    "#;
    let glsl = emit(src, &["map01"]).expect("emit");
    // remap must appear before map01 (topological: callees first).
    let ri = glsl.find("float remap_float_float_float_float_float(").expect("remap emitted");
    let mi = glsl.find("float map01_float_float_float(").expect("map01 emitted");
    assert!(ri < mi, "remap should precede map01:\n{glsl}");
    assert!(glsl.contains("return remap_float_float_float_float_float(value, min1, max1, 0.0, 1.0);"), "got:\n{glsl}");
}

#[test]
fn vec_returning_and_intrinsics() {
    let src = r#"
        @pure fn random2(p: Vec2) -> Vec2 {
            let da = p.dot(v(127.32, 231.4))
            let db = p.dot(v(12.3, 146.3))
            return v(fract(sin(da)), fract(sin(db)))
        }
    "#;
    let glsl = emit(src, &["random2"]).expect("emit");
    assert!(glsl.starts_with("vec2 random2_Vec2(vec2 p) {"), "got:\n{glsl}");
    assert!(glsl.contains("dot(p, vec2(127.32, 231.4))"), "got:\n{glsl}");
    assert!(glsl.contains("return vec2(fract(sin(da)), fract(sin(db)));"), "got:\n{glsl}");
}

#[test]
fn vec3_vec4_constructors() {
    // A shader that computes a color from scratch needs vecN construction --
    // v(r,g,b) / v(r,g,b,a) must lower to GLSL vec3()/vec4(), not a struct.
    let src = r#"
        @pure fn tint(t: float) -> Vec4 {
            let rgb = v(sin(t), cos(t), 0.5)
            return v(rgb.x, rgb.y, rgb.z, 1.0)
        }
    "#;
    let glsl = emit(src, &["tint"]).expect("emit");
    assert!(glsl.starts_with("vec4 tint_float(float t) {"), "got:\n{glsl}");
    // v(...) maps to native vecN construction, not a helper fn.
    assert!(glsl.contains("vec3(sin(t), cos(t), 0.5)"), "got:\n{glsl}");
    assert!(glsl.contains("return vec4(rgb.x, rgb.y, rgb.z, 1.0);"), "got:\n{glsl}");
    assert!(!glsl.contains("v_float"), "v() should inline, not emit a helper fn:\n{glsl}");
}

#[test]
fn call_swizzle_texture_evaluates_once() {
    for (selection, result) in [("rgb", "v(rgb, 1.0)"), ("xy", "v(rgb.x, rgb.y, 0.0, 1.0)")] {
        let src = format!(
            "@shader(fragment) fn sampled(scene: Sampler2D, tc: Vec2) -> Vec4 {{
                let rgb = texture(scene, tc).{selection}
                return {result}
            }}"
        );
        for profile in [skull_codegen_glsl::GlslProfile::Core330, skull_codegen_glsl::GlslProfile::Es300] {
            let glsl = emit_entry_profile(&src, "sampled", profile).expect("emit");
            assert_eq!(glsl.matches("texture(scene, tc)").count(), 1, "{selection}:\n{glsl}");
        }
    }
}

#[test]
fn call_swizzle_nested_constructor_keeps_one_source_call() {
    let glsl = emit(
        "fn source(p: Vec2) -> Vec3 { return v(sin(p.x), cos(p.y), p.x * p.y) }
         fn selected(p: Vec2) -> Vec2 { return v(source(p), 1.0).wzyx.yx }",
        &["selected"],
    ).expect("emit");
    assert_eq!(glsl.matches("source_Vec2(p)").count(), 1, "{glsl}");
    let binding = glsl.lines().find(|line| line.contains("= vec4(source_Vec2(p), 1.0);")).expect(&glsl);
    let temp = binding.split_whitespace().nth(1).expect("typed vector binding");
    assert!(glsl.contains(&format!("return vec2({temp}.z, {temp}.w);")), "{glsl}");
}

#[test]
fn call_swizzle_constructor_with_folded_call_evaluates_once() {
    let glsl = emit(
        "fn source(t: float) -> float { return sin(t) }
         fn selected(flag: bool, t: float) -> Vec2 {
             return v(if flag { source(t) } else { 0.0 }, 1.0).xx
         }",
        &["selected"],
    ).expect("emit");
    assert_eq!(glsl.matches("source_float(t)").count(), 1, "{glsl}");
    assert!(glsl.contains("flag ? source_float(t) : 0.0"), "{glsl}");
}

#[test]
fn call_swizzle_shared_component_keeps_one_vector_source() {
    let glsl = emit(
        "fn source(p: Vec2) -> Vec4 { return v(sin(p.x), cos(p.y), p.x * p.y, 1.0) }
         fn selected(p: Vec2) -> Vec2 { return Vec2 { x: source(p).x, y: 1.0 }.xx }",
        &["selected"],
    ).expect("emit");
    assert_eq!(glsl.matches("source_Vec2(p)").count(), 1, "{glsl}");
    assert!(glsl.contains("return vec2(call_result.x, call_result.x);"), "{glsl}");
}

#[test]
fn call_swizzle_binding_stays_in_its_branch_and_loop() {
    let glsl = emit(
        "fn source(p: Vec2) -> Vec4 { return v(sin(p.x), cos(p.y), p.x * p.y, 1.0) }
         fn selected(flag: bool, p: Vec2) -> Vec2 {
             let result = if flag { source(p).yx } else { v(0.0, 1.0) }
             return result
         }
         fn repeated(n: int, p: Vec2) -> Vec2 {
             let result = v(0.0, 0.0)
             for i in 0..n { result = result + source(p).xy }
             return result
         }
         fn conditioned(p: Vec2, n: int) -> int {
             let left = n
             while left > 0 && source(p).xy.length() > 0.0 { left = left - 1 }
             return left
         }",
        &["selected", "repeated", "conditioned"],
    ).expect("emit");
    let branch = glsl.split("vec2 selected_bool_Vec2(").nth(1).expect(&glsl);
    let branch_call = branch.find("= source_Vec2(p);").expect(&glsl);
    assert!(branch.find("if (flag) {").expect(&glsl) < branch_call, "{glsl}");
    assert!(branch_call < branch.find("} else {").expect(&glsl), "{glsl}");
    let loop_fn = glsl.split("vec2 repeated_int_Vec2(").nth(1).expect(&glsl);
    let loop_call = loop_fn.find("= source_Vec2(p);").expect(&glsl);
    assert!(loop_fn.find("for (;").expect(&glsl) < loop_call, "{glsl}");
    assert!(loop_call < loop_fn.find("result = (result +").expect(&glsl), "{glsl}");
    let condition = glsl.split("int conditioned_Vec2_int(").nth(1).expect(&glsl);
    let condition_call = condition.find("= source_Vec2(p);").expect(&glsl);
    assert!(condition.find("while (true) {").expect(&glsl) < condition_call, "{glsl}");
    assert!(condition.find("if ((left > 0)) {").expect(&glsl) < condition_call, "{glsl}");
    assert!(condition_call < condition.find("length(").expect(&glsl), "{glsl}");
}

#[test]
fn call_swizzle_trivial_and_scalar_values_stay_inline() {
    let glsl = emit(
        "fn selected(t: float) -> Vec2 { return v(t + 1.0, 2.0).yx }
         fn scalar(p: Vec2) -> float { return sin(p.x) }
         fn constant() -> Vec2 { return v(1.0, 2.0).xy }
         fn folded(flag: bool) -> Vec2 { return v(if flag { 1.0 } else { 2.0 }, 3.0).xx }
         fn source(p: Vec2) -> Vec4 { return v(p.x, p.y, 0.0, 1.0) }
         fn single(p: Vec2) -> float { return source(p).x }",
        &["selected", "scalar", "constant", "folded", "single"],
    ).expect("emit");
    assert!(!glsl.contains("call_result"), "trivial expressions must stay inline:\n{glsl}");
    assert!(glsl.contains("return sin(p.x);"), "{glsl}");
    assert!(glsl.contains("return source_Vec2(p).x;"), "{glsl}");
    assert!(glsl.contains("return vec2(vec2((t + 1.0), 2.0).y, vec2((t + 1.0), 2.0).x);"), "{glsl}");
}

#[test]
fn call_swizzle_temporary_does_not_shadow_a_local() {
    let glsl = emit(
        "fn source(p: Vec2) -> Vec4 { return v(p.x, p.y, 0.0, 1.0) }
         fn selected(call_result: Vec2) -> Vec2 { return source(call_result).yx }",
        &["selected"],
    ).expect("emit");
    assert!(glsl.contains("vec4 call_result_1 = source_Vec2(call_result);"), "{glsl}");
    assert!(glsl.contains("return vec2(call_result_1.y, call_result_1.x);"), "{glsl}");
}

#[test]
fn call_swizzle_bool_conversion_preserves_component_types() {
    let glsl = emit(
        "fn selected(p: Vec3) -> Vec2 { return to_vec3(greater_than(p, v(0.0, 1.0, 2.0))).yx }",
        &["selected"],
    ).expect("emit");
    assert_eq!(glsl.matches("greaterThan(p, vec3(0.0, 1.0, 2.0))").count(), 1, "{glsl}");
    assert!(glsl.contains("vec3 call_result = vec3(greaterThan(p, vec3(0.0, 1.0, 2.0)));"), "{glsl}");
    assert!(glsl.contains("return vec2(call_result.y, call_result.x);"), "{glsl}");
}

#[test]
fn call_swizzle_helper_preserves_components() {
    for (selection, ty, components) in [
        ("rgb", "vec3", &["x", "y", "z"][..]),
        ("yx", "vec2", &["y", "x"][..]),
        ("xx", "vec2", &["x", "x"][..]),
    ] {
        let src = format!(
            "fn source(p: Vec2) -> Vec4 {{ return v(sin(p.x), cos(p.y), p.x * p.y, 1.0) }}
             fn selected(p: Vec2) -> {} {{ return source(p).{selection} }}",
            if ty == "vec3" { "Vec3" } else { "Vec2" }
        );
        let glsl = emit(&src, &["selected"]).expect("emit");
        assert_eq!(glsl.matches("source_Vec2(p)").count(), 1, "{selection}:\n{glsl}");
        let binding = glsl.lines().find(|line| line.contains("= source_Vec2(p);")).expect(&glsl);
        let temp = binding.split_whitespace().nth(1).expect("typed vector binding");
        assert!(binding.trim_start().starts_with("vec4 "), "{glsl}");
        let args = components.iter().map(|c| format!("{temp}.{c}")).collect::<Vec<_>>().join(", ");
        assert!(glsl.contains(&format!("return {ty}({args});")), "{selection}:\n{glsl}");
    }
}

#[test]
fn nested_counted_loops() {
    let src = r#"
        fn random2(p: Vec2) -> Vec2 {
            return v(fract(p.x), fract(p.y))
        }
        fn worley2(p: Vec2) -> float {
            let dist = 1.0
            let i_p = v(floor(p.x), floor(p.y))
            let f_p = v(fract(p.x), fract(p.y))
            for y in -1..2 {
                for x in -1..2 {
                    let n = v(int_to_float(x), int_to_float(y))
                    let diff = n + random2(i_p + n) - f_p
                    dist = min(dist, diff.length())
                }
            }
            return dist
        }
    "#;
    let glsl = emit(src, &["worley2"]).expect("emit");
    assert!(glsl.contains("for (; y < "), "got:\n{glsl}");
    assert!(glsl.contains("for (; x < "), "got:\n{glsl}");
    assert!(glsl.contains("dist = min(dist, length(diff));"), "got:\n{glsl}");
    assert!(glsl.contains("return dist;"), "got:\n{glsl}");
    // Vec operators render inline (native infix) and are NOT emitted as GLSL fn
    // definitions (they are reachability leaves, like intrinsics).
    assert!(!glsl.contains("__op_add"), "vec operators must not emit fn defs:\n{glsl}");
    assert!(!glsl.contains("__op_sub"), "vec operators must not emit fn defs:\n{glsl}");
}

/// A counted loop over a RUNTIME invariant bound folds the bound expression into
/// the `for` header instead of a hoisted temp (`t = r + 1; for (; y < t; ..)`).
/// The temp form miscompiles on a GLES/GL driver for the outline_detect kernel
/// (an outline classification shader) even though it is valid GLSL; the inlined bound
/// is correct, and `r + 1` is loop-invariant (`r` is single-store), so re-reading
/// it in the condition is sound. Mirrors the outline_detect neighbor-scan shape.
#[test]
fn counted_loop_invariant_bound_folds() {
    let src = r#"
        fn scan(n: int) -> int {
            let r = n + 2
            let acc = 0
            for y in (0 - r)..(r + 1) {
                acc = acc + y
            }
            return acc
        }
    "#;
    let glsl = emit(src, &["scan"]).expect("emit");
    // The bound is inlined into the header, not read from a hoisted temp.
    assert!(glsl.contains("for (; y < (r + 1);"), "invariant bound must inline:\n{glsl}");
    // No hoisted bound-temp assignment survives (it would read `= (r + 1);` on its
    // own line, distinct from the loop init `y = (0 - r);` and step `y = y + 1`).
    assert!(!glsl.contains("= (r + 1);"), "bound temp must be dropped:\n{glsl}");
}

// --- relooper: structured control flow (docs/shaders.md) ---------

/// Milestone 2: the `pick` shape that `reject_if_branch_sentinel` pinned now
/// emits an if/else (both arms diverge; ipdom collapses to the virtual exit).
#[test]
fn if_branch_pick() {
    let src = r#"@pure fn pick(a: float, b: float) -> float { if a > b { return a } else { return b } }"#;
    let glsl = emit(src, &["pick"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float pick_float_float(float a, float b) {\n  \
         if ((a > b)) {\n    \
           return a;\n  \
         } else {\n    \
           return b;\n  \
         }\n}"
    );
}

/// The motivating fn: `Vec2.normalize`'s zero-guard body emits (NOT the GLSL
/// `normalize` builtin, which is undefined at 0). The `&&` becomes a bool temp
/// diamond; the outer `if` splits the guard from the normalize. `normalize` is
/// deliberately absent from the intrinsic table, so the method's mangled fn def
/// emits.
#[test]
fn vec2_normalize_body() {
    let src = r#"fn root(v: Vec2) -> Vec2 { return v.normalize() }"#;
    let glsl = emit(src, &["root"]).expect("emit");
    assert!(
        glsl.contains(
            "vec2 normalize_Vec2(vec2 v) {\n  \
             float len;\n  \
             if (((v.x == 0.0) && (v.y == 0.0))) {\n    \
               return vec2(0.0);\n  \
             } else {\n    \
               len = sqrt(((v.x * v.x) + (v.y * v.y)));\n    \
               return vec2((v.x / len), (v.y / len));\n  \
             }\n}"
        ),
        "got:\n{glsl}"
    );
    // The native GLSL builtin must NOT be used (undefined at 0).
    assert!(!glsl.contains("= normalize(v)"), "must not use native normalize:\n{glsl}");
}

/// Statement `if` with no else: the empty else block is suppressed.
#[test]
fn statement_if_no_else() {
    let src = r#"fn f(x: float) -> float { let r = x if x > 0.0 { r = x * 2.0 } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_float(float x) {\n  \
         float r;\n  \
         r = x;\n  \
         if ((x > 0.0)) {\n    \
           r = (x * 2.0);\n  \
         }\n  \
         return r;\n}"
    );
}

/// if / else-if ladder (the grass `mask_red` shape): nested full diamonds.
#[test]
fn if_else_if_ladder() {
    let src = r#"@pure fn mask(x: float) -> float {
        if x < 0.0 { return 0.0 } else if x < 1.0 { return x } else { return 1.0 }
    }"#;
    let glsl = emit(src, &["mask"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float mask_float(float x) {\n  \
         if ((x < 0.0)) {\n    \
           return 0.0;\n  \
         } else {\n    \
           if ((x < 1.0)) {\n      \
             return x;\n    \
           } else {\n      \
             return 1.0;\n    \
           }\n  \
         }\n}"
    );
}

/// Expression-position `if` into a cell folds to a ternary (cosmetic tier,
/// `docs/shaders.md` item 2): both arms single-store the single-use
/// result cell -> `(cond ? a : b)` at the use.
#[test]
fn expression_if_into_cell() {
    let src = r#"fn f(x: float) -> float { let r = if x > 0.0 { x } else { 0.0 - x } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_float(float x) {\n  \
         float r;\n  \
         r = ((x > 0.0) ? x : (0.0 - x));\n  \
         return r;\n}"
    );
}

/// A ternary over an int-typed if-expr keeps the outer `float(...)` coercion
/// (both arms unify to int; the `let r: float` subsumption wraps the whole
/// ternary, exactly as the diamond's `r = float(t1)` store did).
#[test]
fn ternary_int_result_outer_coercion() {
    let src = r#"fn f(x: float) -> float { let r: float = if x > 0.0 { 1 } else { 2 } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_float(float x) {\n  \
         float r;\n  \
         r = float(((x > 0.0) ? 1 : 2));\n  \
         return r;\n}"
    );
}

/// `&&` in a nested if: the short-circuit diamond folds back to infix `&&`
/// (cosmetic tier -- `docs/shaders.md`).
#[test]
fn and_in_nested_if() {
    let src = r#"@pure fn f(x: float, y: float) -> float { if x > 0.0 && y > 0.0 { return 1.0 } return 0.0 }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_float_float(float x, float y) {\n  \
         if (((x > 0.0) && (y > 0.0))) {\n    \
           return 1.0;\n  \
         }\n  \
         return 0.0;\n}"
    );
}

/// `||` in a nested if: folds back to infix `||`.
#[test]
fn or_in_nested_if() {
    let src = r#"@pure fn f(x: float, y: float) -> float { if x > 0.0 || y > 0.0 { return 1.0 } return 0.0 }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_float_float(float x, float y) {\n  \
         if (((x > 0.0) || (y > 0.0))) {\n    \
           return 1.0;\n  \
         }\n  \
         return 0.0;\n}"
    );
}

/// Early `return` inside an `if` inside a counted `for`: the empty-else rule
/// keeps the return unnested; the counted-for shape survives the early exit.
#[test]
fn early_return_in_counted_for() {
    let src = r#"fn f(n: int) -> int {
        let acc = 0
        for i in 0..n {
            if i > 5 { return acc }
            acc = acc + i
        }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_int(int n) {\n  \
         int acc;\n  \
         int i;\n  \
         int t3;\n  \
         acc = 0;\n  \
         i = 0;\n  \
         t3 = n;\n  \
         for (; i < t3; i = i + 1) {\n    \
           if ((i > 5)) {\n      \
             return acc;\n    \
           }\n    \
           acc = (acc + i);\n  \
         }\n  \
         return acc;\n}"
    );
}

/// `while` with a compound `&&` condition: the header is a multi-block region
/// (a diamond computing the bool), emitted via `while (true) { <cond>; if (!c) break; <body> }`.
#[test]
fn while_compound_condition() {
    let src = r#"fn f(x: float) -> float {
        let acc = 0.0
        let i = 0
        while i < 10 && acc < 100.0 {
            acc = acc + x
            i = i + 1
        }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_float(float x) {\n  \
         float acc;\n  \
         int i;\n  \
         acc = 0.0;\n  \
         i = 0;\n  \
         while (true) {\n    \
           if (!(((i < 10) && (acc < 100.0)))) { break; }\n    \
           acc = (acc + x);\n    \
           i = (i + 1);\n  \
         }\n  \
         return acc;\n}"
    );
}

/// break + continue in a counted `for` (continue -> the for-header increment).
#[test]
fn break_continue_in_counted_for() {
    let src = r#"fn f(n: int) -> int {
        let acc = 0
        for i in 0..n {
            if i > 5 { break }
            if i == 2 { continue }
            acc = acc + i
        }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_int(int n) {\n  \
         int acc;\n  \
         int i;\n  \
         int t3;\n  \
         acc = 0;\n  \
         i = 0;\n  \
         t3 = n;\n  \
         for (; i < t3; i = i + 1) {\n    \
           if ((i > 5)) {\n      \
             break;\n    \
           }\n    \
           if ((i == 2)) {\n      \
             continue;\n    \
           }\n    \
           acc = (acc + i);\n  \
         }\n  \
         return acc;\n}"
    );
}

/// break + continue in a general `while`.
#[test]
fn break_continue_in_while() {
    let src = r#"fn f(n: int) -> int {
        let acc = 0
        let i = 0
        while i < n {
            i = i + 1
            if i > 5 { break }
            if i == 2 { continue }
            acc = acc + i
        }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_int(int n) {\n  \
         int acc;\n  \
         int i;\n  \
         acc = 0;\n  \
         i = 0;\n  \
         while (true) {\n    \
           if (!((i < n))) { break; }\n    \
           i = (i + 1);\n    \
           if ((i > 5)) {\n      \
             break;\n    \
           }\n    \
           if ((i == 2)) {\n      \
             continue;\n    \
           }\n    \
           acc = (acc + i);\n  \
         }\n  \
         return acc;\n}"
    );
}

/// Nested loop + if (the grass wind-loop shape).
#[test]
fn nested_loop_with_if() {
    let src = r#"fn f(n: int) -> int {
        let acc = 0
        for y in 0..n {
            for x in 0..n {
                if x == y { acc = acc + 1 }
            }
        }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_int(int n) {\n  \
         int acc;\n  \
         int y;\n  \
         int t3;\n  \
         int x;\n  \
         int t5;\n  \
         acc = 0;\n  \
         y = 0;\n  \
         t3 = n;\n  \
         for (; y < t3; y = y + 1) {\n    \
           x = 0;\n    \
           t5 = n;\n    \
           for (; x < t5; x = x + 1) {\n      \
             if ((x == y)) {\n        \
               acc = (acc + 1);\n      \
             }\n    \
           }\n  \
         }\n  \
         return acc;\n}"
    );
}

/// Const loop bound folds into the for-header (cosmetic tier,
/// `docs/shaders.md` item 3): `t = 4; for (; i < t; ...)` -> `for (;
/// i < 4; ...)`, dropping the single-use bound temp + its declaration. A const
/// is loop-invariant, so re-reading it each iteration is identical.
#[test]
fn const_loop_bound_fold() {
    let src = r#"fn f() -> int {
        let acc = 0
        for i in 0..4 { acc = acc + i }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_0() {\n  \
         int acc;\n  \
         int i;\n  \
         acc = 0;\n  \
         i = 0;\n  \
         for (; i < 4; i = i + 1) {\n    \
           acc = (acc + i);\n  \
         }\n  \
         return acc;\n}"
    );
}

/// A PARAM loop bound is NOT folded (it could be reassigned in the loop body,
/// which would change the iteration count if re-read each iteration -- only
/// const bounds are provably loop-invariant).
#[test]
fn param_loop_bound_not_folded() {
    let src = r#"fn f(n: int) -> int {
        let acc = 0
        for i in 0..n { acc = acc + i }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(glsl.contains("t3 = n;"), "param bound must stay a temp:\n{glsl}");
    assert!(glsl.contains("for (; i < t3; i = i + 1) {"), "got:\n{glsl}");
}

/// Milestone finding: `match` lowers to `Branch` chains (NOT `Term::Switch`),
/// so a tier-0 int `match` is accepted and emitted as an if / else-if ladder.
#[test]
fn match_int_ladder() {
    let src = r#"@pure fn f(n: int) -> int {
        match n { 1 => { return 10 } 2 => { return 20 } _ => { return 0 } }
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_int(int n) {\n  \
         if ((n == 1)) {\n    \
           return 10;\n  \
         } else {\n    \
           if ((n == 2)) {\n      \
             return 20;\n    \
           } else {\n      \
             return 0;\n    \
           }\n  \
         }\n}"
    );
}

/// An exhaustive boolean `match` as an EXPRESSION: both arms store one result
/// cell and a real join continues (`return x + 1`). The arm-test chain's dead
/// end is a REACHABLE `Term::Unreachable`, which must not count as an exit
/// path for post-dominance -- else the join's ipdom collapses to the virtual
/// exit, both arms absorb the continuation, and the join double-walks into
/// `unstructured control flow`. (JS fixed this in T6; GLSL kept the old
/// `Return | Unreachable` rule and rejects this shape.)
/// Oracle: source semantics -- the arm literals (10/20), their patterns
/// (true/false), and the join arithmetic (+ 1) -- plus the source's own
/// control shape: two arms, one join, one return. `match_int_ladder` above is
/// the control: its wildcard tail is unreachable, so it structured even before.
#[test]
fn exhaustive_bool_match_expression_structures() {
    let src = r#"@pure fn f(b: bool) -> int {
        let x = match b { true => 10, false => 20 }
        return x + 1
    }"#;
    let glsl = emit(src, &["f"]).expect("an exhaustive bool match with a real join must structure");
    // Each pattern guards its own arm's literal, and the join follows both.
    let t = glsl.find("true").expect("the true pattern must survive");
    let arm10 = glsl.find("= 10;").expect("the true arm must survive");
    let f = glsl.find("false").expect("the false pattern must survive");
    let arm20 = glsl.find("= 20;").expect("the false arm must survive");
    let ret = glsl.find("return").expect("the join must survive");
    assert!(
        t < arm10 && arm10 < f && f < arm20 && arm20 < ret,
        "each pattern must guard its own arm ahead of the join; got:\n{glsl}"
    );
    assert_eq!(glsl.matches("return").count(), 1, "the join must be walked once; got:\n{glsl}");
}

/// A `match` EXPRESSION ladder: the inner diamond stores the same single-use
/// result cell as the outer arm, so folding it to a ternary would suppress the
/// outer arm's store and silently drop the `n == 1` case (emitting an empty
/// `if/else` followed by an unconditional `x = (n==2) ? 20 : 30`, so `f(1)`
/// returned 31 instead of 11). The fold requires the diamond's two arms to be
/// the cell's ONLY writers -- a load-count of 1 does not imply that.
#[test]
fn match_expression_ladder_is_not_folded() {
    let src = r#"@pure fn f(n: int) -> int {
        let x = match n { 1 => 10, 2 => 20, _ => 30 }
        return x + 1
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(glsl.contains("t1 = 10;"), "n == 1 arm must survive; got:\n{glsl}");
    assert!(
        !glsl.contains("(n == 2) ? 20 : 30"),
        "inner diamond must not fold past the outer arm's store; got:\n{glsl}"
    );
}

/// The same diamond shape WITH a single writer pair still folds -- the store
/// guard must not disable the legitimate cosmetic fold.
#[test]
fn two_store_diamond_still_folds() {
    let src = r#"@pure fn f(n: int) -> int {
        let y = match n { 10 => 100, _ => 200 }
        return y
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(glsl.contains('?'), "single diamond should still fold to a ternary; got:\n{glsl}");
}

/// Nested general `while` loops with an inner `break` (the inner loop's exit
/// aliases the outer loop's back-edge region -- pins the `loop_binding` gate
/// that keeps a manual `while i < n { .. i = i + 1 }` off the range-for path).
#[test]
fn nested_while_loops() {
    let src = r#"fn f(n: int) -> int {
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
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f_int(int n) {\n  \
         int acc;\n  \
         int i;\n  \
         int j;\n  \
         acc = 0;\n  \
         i = 0;\n  \
         while (true) {\n    \
           if (!((i < n))) { break; }\n    \
           j = 0;\n    \
           while (true) {\n      \
             if (!((j < i))) { break; }\n      \
             if ((j == 3)) {\n        \
               break;\n      \
             }\n      \
             acc = (acc + 1);\n      \
             j = (j + 1);\n    \
           }\n    \
           i = (i + 1);\n  \
         }\n  \
         return acc;\n}"
    );
}

/// Int literals in float contexts render as float literals: GLSL ES has no
/// implicit int->float conversion, and desktop drivers vary on the corner.
/// Int contexts (the match ladder's `n == 1`, counted-for bounds) keep ints.
#[test]
fn int_literal_float_contexts() {
    let src = r#"
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
    "#;
    let glsl = emit(src, &["root"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float root_float(float x) {\n  \
         float y;\n  \
         y = 0.5;\n  \
         if ((x > 0.0)) {\n    \
           y = (x + 1.0);\n  \
         }\n  \
         if ((x == 3.0)) {\n    \
           y = sin(2.0);\n  \
         }\n  \
         return 0.0;\n}"
    );
}

// --- non-literal int->float subsumption (typed-MIR coercion) ----------------
// bone allows general int->float subsumption (`fn f(i: int) -> float { return i }`
// and `takes_float(i)` both compile). On GLSL that materializes a `float(...)`
// wrap; the coercion is inserted at lowering (Inst::IntToFloat), not re-derived
// in the emitter. Docs: docs/shaders.md.

/// An int PARAM (non-literal) returned from a float fn gets a `float(...)` wrap.
#[test]
fn int_param_returned_from_float_fn() {
    let src = r#"@pure fn f(i: int) -> float { return i }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(glsl.trim(), "float f_int(int i) {\n  return float(i);\n}");
}

/// An int arg (non-literal) at a float param gets a `float(...)` wrap.
#[test]
fn int_arg_at_float_param() {
    let src = r#"
        @pure fn takes_float(x: float) -> float { return x }
        @pure fn f(i: int) -> float { return takes_float(i) }
    "#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(glsl.contains("return takes_float_float(float(i));"), "got:\n{glsl}");
}

/// An int (non-literal) mixed into float arithmetic gets a `float(...)` wrap.
#[test]
fn int_in_mixed_arithmetic() {
    let src = r#"@pure fn f(x: float, i: int) -> float { return x + i }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(glsl.trim(), "float f_float_int(float x, int i) {\n  return (x + float(i));\n}");
}

/// Cell typing: a `let y: float = 0` binding declares `float y;` (from the
/// binding's declared type via Cell.ty) and stores `0.0` -- NOT `int y; y = 0`
/// (the first-store inference corner this replaces).
#[test]
fn let_float_annotation_cell_typing() {
    let src = r#"fn f() -> float { let y: float = 0  return y }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f_0() {\n  \
         float y;\n  \
         y = 0.0;\n  \
         return y;\n}"
    );
}

// --- A1: fragment-entry compilation (docs/shaders.md) -------------------

fn emit_entry(src: &str, root: &str) -> Result<String, String> {
    emit_entry_profile(src, root, skull_codegen_glsl::GlslProfile::Core330)
}

fn emit_entry_profile(
    src: &str,
    root: &str,
    profile: skull_codegen_glsl::GlslProfile,
) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    // `semantic_pass` returns Ok with per-body errors recorded in `ws.errors`;
    // the CLI reports those and refuses to emit. Without this check the goldens
    // pin emission for sources the real pipeline rejects (backlog #67).
    let errs = skull_mir::verdict::body_errors(&ws);
    if !errs.is_empty() {
        return Err(errs.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; "));
    }
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull_codegen_glsl::emit_shader_entry(&ws, mid, root, profile).map_err(|e| e.message)
}

/// The A1 scaffold: generated #version + PIXEL block (in/out, the `@uniform`
/// group's std140 block + sampler declarations, helpers, entry fn with dropped
/// uniform/sampler params, main) + the default fullscreen VERTEX block.
#[test]
fn fragment_entry_scaffold() {
    let src = r#"
        value GlowU { strength: float, center: Vec2, radius: float }

        @pure fn falloff(d: float, r: float) -> float {
            return 1.0 - smoothstep(0.0, r, d)
        }

        @shader(fragment)
        fn glow(@uniform u: GlowU, scene: Sampler2D, tc: Vec2) -> Vec4 {
            let m = falloff(tc.distance(u.center), u.radius)
            let t = texture(scene, tc)
            let k = 1.0 + u.strength * m
            return Vec4 { x: t.x * k, y: t.y * k, z: t.z * k, w: t.w }
        }
    "#;
    let glsl = emit_entry(src, "glow").expect("emit");
    assert!(glsl.starts_with("#version 330 core\n"), "got:\n{glsl}");
    assert!(glsl.contains("#ifdef PIXEL"), "got:\n{glsl}");
    assert!(glsl.contains("in vec2 TexCoord;"), "got:\n{glsl}");
    assert!(glsl.contains("out vec4 FragColor;"), "got:\n{glsl}");
    // Uniforms: one std140 block in field order, then the loose samplers.
    assert!(glsl.contains("layout(std140) uniform GlowU {\n    float strength;\n    vec2 center;\n    float radius;\n};\nuniform sampler2D scene;"), "got:\n{glsl}");
    // Helper before entry; entry drops uniform/sampler params.
    let fi = glsl.find("float falloff_float_float(float d, float r)").expect("helper emitted");
    let gi = glsl.find("vec4 glow(vec2 tc)").expect("entry emitted");
    assert!(fi < gi, "helper must precede entry:\n{glsl}");
    // Uniform-struct reads render as bare uniform names.
    assert!(glsl.contains("distance(tc, center)"), "got:\n{glsl}");
    assert!(glsl.contains("texture(scene, tc)"), "got:\n{glsl}");
    assert!(!glsl.contains("u."), "uniform struct must not survive as a param:\n{glsl}");
    assert!(glsl.contains("void main() {\n  FragColor = glow(TexCoord);\n}"), "got:\n{glsl}");
    // Default fullscreen vertex stage.
    assert!(glsl.contains("#ifdef VERTEX"), "got:\n{glsl}");
    assert!(glsl.contains("gl_Position = vec4(aPos, 0.0, 1.0);"), "got:\n{glsl}");
    assert!(glsl.contains("TexCoord = aTexCoord;"), "got:\n{glsl}");
}

/// `@uniform` is ONE std140 block, not a run of loose uniforms. The block is
/// declared with no instance name, so its members stay bare GLSL globals and
/// every body reading them is unchanged -- that is what confines this change to
/// declaration lines (verified on real drivers in both profiles,
/// docs/shader-binding-layout-plan.md). Samplers are opaque and cannot live in a
/// block, so they stay loose beside it.
#[test]
fn fragment_entry_uniforms_are_one_std140_block() {
    let src = r#"
        value GlowU { strength: float, center: Vec2, radius: float }

        @shader(fragment)
        fn glow(@uniform u: GlowU, scene: Sampler2D, tc: Vec2) -> Vec4 {
            let d = tc.distance(u.center)
            let k = 1.0 + u.strength * (1.0 - smoothstep(0.0, u.radius, d))
            let t = texture(scene, tc)
            return Vec4 { x: t.x * k, y: t.y * k, z: t.z * k, w: t.w }
        }
    "#;
    let glsl = emit_entry(src, "glow").expect("emit");
    // Member order is field order -- the same contract @attr uses for locations
    // and @textures for slots.
    assert!(
        glsl.contains(
            "layout(std140) uniform GlowU {\n    float strength;\n    vec2 center;\n    float radius;\n};\n"
        ),
        "got:\n{glsl}"
    );
    assert!(glsl.contains("uniform sampler2D scene;"), "got:\n{glsl}");
    assert!(!glsl.contains("uniform float strength;"), "no loose member survives:\n{glsl}");
    // The block is UNNAMED-instance, so reads are bare globals exactly as before.
    assert!(glsl.contains("distance(tc, center)"), "got:\n{glsl}");
    assert!(!glsl.contains("u."), "uniform struct must not survive as a param:\n{glsl}");
}

/// ES300 fullscreen entry: the profile header replaces the hardcoded 330
/// version line, and BOTH stages carry the precision preamble (ES requires an
/// explicit float precision in the fragment stage; the vertex declarations are
/// harmless no-ops there).
#[test]
fn fragment_entry_es300_profile() {
    let src = r#"
        @shader(fragment)
        fn blit(scene: Sampler2D, tc: Vec2) -> Vec4 {
            return texture(scene, tc)
        }
    "#;
    let glsl = emit_entry_profile(src, "blit", skull_codegen_glsl::GlslProfile::Es300).expect("emit");
    assert!(glsl.starts_with("#version 300 es\n"), "got:\n{glsl}");
    let px = glsl.find("#ifdef PIXEL").expect("pixel block");
    let vx = glsl.find("#ifdef VERTEX").expect("vertex block");
    let pixel_block = &glsl[px..vx];
    let vertex_block = &glsl[vx..];
    assert!(pixel_block.contains("precision highp float;"), "got:\n{glsl}");
    assert!(vertex_block.contains("precision highp float;"), "got:\n{glsl}");
    assert!(!glsl.contains("330"), "no desktop version residue:\n{glsl}");
}

/// Entry with samplers only (no uniform struct) is legal.
#[test]
fn fragment_entry_sampler_only() {
    let src = r#"
        @shader(fragment)
        fn blit(scene: Sampler2D, tc: Vec2) -> Vec4 {
            return texture(scene, tc)
        }
    "#;
    let glsl = emit_entry(src, "blit").expect("emit");
    assert!(glsl.contains("uniform sampler2D scene;"), "got:\n{glsl}");
    assert!(glsl.contains("vec4 blit(vec2 tc)"), "got:\n{glsl}");
    assert!(glsl.contains("return texture(scene, tc);"), "got:\n{glsl}");
}

/// Mat4 * Vec4 and Mat4 * Mat4 render as native GLSL `*` (the vec/mat operator
/// keying -- any 2-arg `__op_mul` is a native infix). `Mat4` maps to `mat4`.
#[test]
fn mat4_native_multiply() {
    let src = r#"
        @pure fn xform(m: Mat4, p: Vec4) -> Vec4 { return m * p }
        @pure fn compose(a: Mat4, b: Mat4) -> Mat4 { return a * b }
    "#;
    let glsl = emit(src, &["xform", "compose"]).expect("emit");
    assert!(glsl.contains("vec4 xform_Mat4_Vec4(mat4 m, vec4 p) {"), "got:\n{glsl}");
    assert!(glsl.contains("return (m * p);"), "got:\n{glsl}");
    assert!(glsl.contains("mat4 compose_Mat4_Mat4(mat4 a, mat4 b) {"), "got:\n{glsl}");
    assert!(glsl.contains("return (a * b);"), "got:\n{glsl}");
}

/// A `nobox` value crossing a call on the GLSL path must NOT be rejected as a
/// boxed boundary. GLSL has no collections and no boxes: a `value` struct is a
/// first-class `vecN`, which is exactly what `ScalarCtx::values_first_class`
/// says. The nobox sink classifier used `!fn_uses_scalar_abi` as its proxy for
/// "this callee takes a real boxed value", and that flag INVERTS under
/// values-first-class -- so every shader call rejected.
#[test]
fn nobox_value_crosses_a_glsl_call_without_materializing() {
    let src = r#"
        @pure fn len2(v: Vec2) -> float { return v.x * v.x + v.y * v.y }
        @pure fn shade(p: Vec2) -> float { return len2(p) }
    "#;
    let glsl = emit_with(&STDLIB_NOBOX_VEC2, src, &["shade"]).expect("nobox Vec2 must emit GLSL");
    assert!(
        glsl.contains("vec2 v") || glsl.contains("(vec2 "),
        "nobox Vec2 must lower to a first-class vec2 parameter:\n{glsl}"
    );
    assert!(
        !glsl.contains("materialize"),
        "GLSL emission must not route a nobox value through materialization:\n{glsl}"
    );
}

/// The `@gpu_only` intrinsic boundary is the ONE place a nobox value is
/// authorized to box (`authorize_gpu_boundary_boxes`), and it authorizes by
/// re-marking the packed arg `MaterializeValueStruct`. So on the GLSL side that
/// variant is the shape a mapped intrinsic's argument arrives in, and every
/// ctor arm has to render it exactly like `MakeValueStruct` -- a plain `vecN`.
///
/// Distinct from `nobox_value_crosses_a_glsl_call_without_materializing`, which
/// pins the opposite property one layer out: an ORDINARY call is first-class and
/// mints no Materialize at all. Here the boundary is `@gpu_only`, the MIR does
/// carry a Materialize, and the requirement is that GLSL renders it as a ctor
/// rather than dropping it. Neither test may be "fixed" against the other.
///
/// The argument must be a literal ctor AT the call. `authorize_gpu_boundary_boxes`
/// re-marks only a `MakeValueStruct` operand, so passing a `Vec2` param whole --
/// or building it through the stdlib `v(..)` helper -- mints nothing to authorize
/// and leaves all four arms unexercised.
#[test]
fn a_materialized_nobox_value_at_a_gpu_intrinsic_lowers_to_a_vec_ctor() {
    let src = r#"
        fn shade(s: Sampler2D, u: float, v_: float) -> Vec4 {
            return texture(s, Vec2 { x: u, y: v_ })
        }
    "#;
    let glsl = emit_with(&STDLIB_NOBOX_VEC2, src, &["shade"])
        .expect("a nobox Vec2 at a @gpu_only boundary must emit GLSL");
    assert!(
        glsl.contains("texture(s, vec2(u, v_))"),
        "the authorized box must render as a plain vec2 ctor argument:\n{glsl}"
    );
}

/// A `let` of a `nobox` value inside a shader body stays a single `vecN` local.
/// GLSL has no boxed representation to avoid, so the leaf-cell form buys nothing
/// and costs correctness twice: `<name>__<i>` is a RESERVED identifier (GLSL spec
/// 3.7), and each leaf re-evaluates the whole initializer.
///
/// The two nobox tests above pin call BOUNDARIES; this one pins a body-internal
/// storage decision, which `fn_uses_scalar_abi` never sees.
#[test]
fn a_nobox_value_local_stays_a_single_glsl_vec() {
    let src = r#"
        @pure fn shade(tc: Vec2) -> Vec4 {
            let shift = v(tc.x * 0.5, tc.y * 0.5)
            return Vec4 { x: shift.x, y: shift.y, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_with(&STDLIB_NOBOX_VEC2, src, &["shade"]).expect("nobox Vec2 must emit GLSL");
    assert!(
        glsl.contains("vec2 shift;"),
        "the local must be one vec2, not scalarized leaves:\n{glsl}"
    );
    assert!(
        !glsl.contains("shift__"),
        "a double underscore is a reserved GLSL identifier:\n{glsl}"
    );
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
