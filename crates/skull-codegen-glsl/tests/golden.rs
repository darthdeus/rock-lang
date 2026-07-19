//! Golden tests: Rock snippet -> expected GLSL text, over the real stdlib.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

/// Emit GLSL for `roots` from `src` (the `<test>` module).
fn emit(src: &str, roots: &[&str]) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    skull_codegen_glsl::emit_shader_library(&ws, mid, &roots).map_err(|e| e.message)
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
        "float remap(float value, float min1, float max1, float min2, float max2) {\n  \
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
    let ri = glsl.find("float remap(").expect("remap emitted");
    let mi = glsl.find("float map01(").expect("map01 emitted");
    assert!(ri < mi, "remap should precede map01:\n{glsl}");
    assert!(glsl.contains("return remap(value, min1, max1, 0.0, 1.0);"), "got:\n{glsl}");
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
    assert!(glsl.starts_with("vec2 random2(vec2 p) {"), "got:\n{glsl}");
    assert!(glsl.contains("dot(p, vec2(127.32, 231.4))"), "got:\n{glsl}");
    assert!(glsl.contains("return vec2(fract(sin(da)), fract(sin(db)));"), "got:\n{glsl}");
}

#[test]
fn nested_counted_loops() {
    let src = r#"
        @pure fn random2(p: Vec2) -> Vec2 {
            return v(fract(p.x), fract(p.y))
        }
        @pure fn worley2(p: Vec2) -> float {
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

// --- relooper: structured control flow (docs/shaders.md) ---------

/// Milestone 2: the `pick` shape that `reject_if_branch_sentinel` pinned now
/// emits an if/else (both arms diverge; ipdom collapses to the virtual exit).
#[test]
fn if_branch_pick() {
    let src = r#"@pure fn pick(a: float, b: float) -> float { if a > b { return a } else { return b } }"#;
    let glsl = emit(src, &["pick"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float pick(float a, float b) {\n  \
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
    let src = r#"@pure fn root(v: Vec2) -> Vec2 { return v.normalize() }"#;
    let glsl = emit(src, &["root"]).expect("emit");
    assert!(
        glsl.contains(
            "vec2 normalize__Vec2_ret_Vec2(vec2 v) {\n  \
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
    let src = r#"@pure fn f(x: float) -> float { let r = x if x > 0.0 { r = x * 2.0 } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f(float x) {\n  \
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
        "float mask(float x) {\n  \
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
    let src = r#"@pure fn f(x: float) -> float { let r = if x > 0.0 { x } else { 0.0 - x } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f(float x) {\n  \
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
    let src = r#"@pure fn f(x: float) -> float { let r: float = if x > 0.0 { 1 } else { 2 } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f(float x) {\n  \
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
        "float f(float x, float y) {\n  \
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
        "float f(float x, float y) {\n  \
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
    let src = r#"@pure fn f(n: int) -> int {
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
        "int f(int n) {\n  \
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
    let src = r#"@pure fn f(x: float) -> float {
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
        "float f(float x) {\n  \
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
    let src = r#"@pure fn f(n: int) -> int {
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
        "int f(int n) {\n  \
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
    let src = r#"@pure fn f(n: int) -> int {
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
        "int f(int n) {\n  \
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
    let src = r#"@pure fn f(n: int) -> int {
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
        "int f(int n) {\n  \
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
    let src = r#"@pure fn f() -> int {
        let acc = 0
        for i in 0..4 { acc = acc + i }
        return acc
    }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "int f() {\n  \
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
    let src = r#"@pure fn f(n: int) -> int {
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
        "int f(int n) {\n  \
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

/// Nested general `while` loops with an inner `break` (the inner loop's exit
/// aliases the outer loop's back-edge region -- pins the `loop_binding` gate
/// that keeps a manual `while i < n { .. i = i + 1 }` off the range-for path).
#[test]
fn nested_while_loops() {
    let src = r#"@pure fn f(n: int) -> int {
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
        "int f(int n) {\n  \
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
        @pure fn root(x: float) -> float {
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
        "float root(float x) {\n  \
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
    assert_eq!(glsl.trim(), "float f(int i) {\n  return float(i);\n}");
}

/// An int arg (non-literal) at a float param gets a `float(...)` wrap.
#[test]
fn int_arg_at_float_param() {
    let src = r#"
        @pure fn takes_float(x: float) -> float { return x }
        @pure fn f(i: int) -> float { return takes_float(i) }
    "#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert!(glsl.contains("return takes_float(float(i));"), "got:\n{glsl}");
}

/// An int (non-literal) mixed into float arithmetic gets a `float(...)` wrap.
#[test]
fn int_in_mixed_arithmetic() {
    let src = r#"@pure fn f(x: float, i: int) -> float { return x + i }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(glsl.trim(), "float f(float x, int i) {\n  return (x + float(i));\n}");
}

/// Cell typing: a `let y: float = 0` binding declares `float y;` (from the
/// binding's declared type via Cell.ty) and stores `0.0` -- NOT `int y; y = 0`
/// (the first-store inference corner this replaces).
#[test]
fn let_float_annotation_cell_typing() {
    let src = r#"@pure fn f() -> float { let y: float = 0  return y }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f() {\n  \
         float y;\n  \
         y = 0.0;\n  \
         return y;\n}"
    );
}

// --- A1: fragment-entry compilation (docs/shaders.md) -------------------

fn emit_entry(src: &str, root: &str) -> Result<String, String> {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull_codegen_glsl::emit_shader_entry(&ws, mid, root).map_err(|e| e.message)
}

/// The A1 scaffold: generated #version + PIXEL block (in/out, uniforms from the
/// struct's fields + sampler params, helpers, entry fn with dropped uniform/
/// sampler params, main) + the default fullscreen VERTEX block.
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
    // Uniforms: struct fields in declaration order, then samplers.
    assert!(glsl.contains("uniform float strength;\nuniform vec2 center;\nuniform float radius;\nuniform sampler2D scene;"), "got:\n{glsl}");
    // Helper before entry; entry drops uniform/sampler params.
    let fi = glsl.find("float falloff(float d, float r)").expect("helper emitted");
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
    assert!(glsl.contains("vec4 xform(mat4 m, vec4 p) {"), "got:\n{glsl}");
    assert!(glsl.contains("return (m * p);"), "got:\n{glsl}");
    assert!(glsl.contains("mat4 compose(mat4 a, mat4 b) {"), "got:\n{glsl}");
    assert!(glsl.contains("return (a * b);"), "got:\n{glsl}");
}
