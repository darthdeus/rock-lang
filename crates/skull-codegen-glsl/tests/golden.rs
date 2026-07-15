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
            let da = vec2_dot(p, v(127.32, 231.4))
            let db = vec2_dot(p, v(12.3, 146.3))
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
                    dist = min(dist, vec2_length(diff))
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

// --- relooper: structured control flow (docs/glsl-relooper-plan.md) ---------

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

/// The motivating fn: `vec2_normalize`'s zero-guard body emits (NOT the GLSL
/// `normalize` builtin, which is undefined at 0). The `&&` becomes a bool temp
/// diamond; the outer `if` splits the guard from the normalize. (The `v.x == 0`
/// int-literal-in-float-comparison is a separate rendering gap, not the relooper's.)
#[test]
fn vec2_normalize_body() {
    let src = r#"@pure fn root(v: Vec2) -> Vec2 { return vec2_normalize(v) }"#;
    let glsl = emit(src, &["vec2_normalize"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "vec2 vec2_normalize(vec2 v) {\n  \
         bool t2;\n  \
         float len;\n  \
         if ((v.x == 0)) {\n    \
           t2 = (v.y == 0);\n  \
         } else {\n    \
           t2 = false;\n  \
         }\n  \
         if (t2) {\n    \
           return vec2(0.0);\n  \
         } else {\n    \
           len = sqrt(((v.x * v.x) + (v.y * v.y)));\n    \
           return vec2((v.x / len), (v.y / len));\n  \
         }\n}"
    );
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

/// Expression-position `if` into a cell: both arms store the result cell.
#[test]
fn expression_if_into_cell() {
    let src = r#"@pure fn f(x: float) -> float { let r = if x > 0.0 { x } else { 0.0 - x } return r }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f(float x) {\n  \
         float t1;\n  \
         float r;\n  \
         if ((x > 0.0)) {\n    \
           t1 = x;\n  \
         } else {\n    \
           t1 = (0.0 - x);\n  \
         }\n  \
         r = t1;\n  \
         return r;\n}"
    );
}

/// `&&` in a nested if: short-circuit becomes a bool temp diamond.
#[test]
fn and_in_nested_if() {
    let src = r#"@pure fn f(x: float, y: float) -> float { if x > 0.0 && y > 0.0 { return 1.0 } return 0.0 }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f(float x, float y) {\n  \
         bool t2;\n  \
         if ((x > 0.0)) {\n    \
           t2 = (y > 0.0);\n  \
         } else {\n    \
           t2 = false;\n  \
         }\n  \
         if (t2) {\n    \
           return 1.0;\n  \
         }\n  \
         return 0.0;\n}"
    );
}

/// `||` in a nested if.
#[test]
fn or_in_nested_if() {
    let src = r#"@pure fn f(x: float, y: float) -> float { if x > 0.0 || y > 0.0 { return 1.0 } return 0.0 }"#;
    let glsl = emit(src, &["f"]).expect("emit");
    assert_eq!(
        glsl.trim(),
        "float f(float x, float y) {\n  \
         bool t2;\n  \
         if ((x > 0.0)) {\n    \
           t2 = true;\n  \
         } else {\n    \
           t2 = (y > 0.0);\n  \
         }\n  \
         if (t2) {\n    \
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
         bool t3;\n  \
         acc = 0.0;\n  \
         i = 0;\n  \
         while (true) {\n    \
           if ((i < 10)) {\n      \
             t3 = (acc < 100.0);\n    \
           } else {\n      \
             t3 = false;\n    \
           }\n    \
           if (!(t3)) { break; }\n    \
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
