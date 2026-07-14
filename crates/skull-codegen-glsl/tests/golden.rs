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
}
