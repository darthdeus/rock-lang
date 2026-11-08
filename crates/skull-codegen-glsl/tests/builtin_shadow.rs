//! A shader local named like a GLSL builtin callable must not shadow it: the
//! emitter hoists every local to function scope, so `let cos` beside a
//! `cos(...)` call used to emit `float cos; cos = cos(angle)`, which native
//! GLSL rejects. The hoisted local is renamed apart; the builtin call survives.
//! (Backlog: "GLSL locals can shadow builtin functions"; the live instance is
//! `Vec2.rotate`'s `cos`/`sin` locals in `stdlib/vec.rock`.)

use skull_codegen_glsl::{GlslProfile, ShaderEmission};
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn workspace(
    src: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    // The CLI reports per-body errors and refuses to emit; without this check
    // the test would pin emission for sources the real pipeline rejects.
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture has body errors: {errs:?}");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

/// Route-B function library: `roots` plus their reachable helper bodies.
fn emit_lib(src: &str, roots: &[&str]) -> String {
    let (ws, mid) = workspace(src);
    let roots: Vec<String> = roots.iter().map(|s| s.to_string()).collect();
    let root_refs = roots
        .iter()
        .map(|root| skull_codegen_glsl::resolve_root(&ws, mid, root))
        .collect::<Result<Vec<_>, _>>()
        .expect("resolve");
    let session = skull_codegen_glsl::shader_library_session(&ws, root_refs).expect("session");
    skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        .expect("emit")
}

fn emit_entry(src: &str, root: &str) -> ShaderEmission {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_entry_with_dependencies(&ws, mid, root, GlslProfile::Core330)
        .expect("emit")
}

/// No binding of the bare `name` anywhere in the GLSL: neither a hoisted
/// declaration (`float cos;`), nor a param (`float cos,` / `float cos)`).
/// A builtin call (`cos(angle)`) never matches these shapes, so every
/// remaining `name(` is the builtin.
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
    let glsl = emit_lib(src, &["spin"]);
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

/// The production instance: `Vec2.rotate` declares `cos`/`sin` locals and calls
/// `angle.cos()` / `angle.sin()`.
#[test]
fn stdlib_vec2_rotate_keeps_builtin_calls_valid() {
    let src = r#"fn root(a: Vec2, angle: float) -> Vec2 { return a.rotate(angle) }"#;
    let glsl = emit_lib(src, &["root"]);
    assert!(
        glsl.contains("rotate"),
        "the rotate helper body must be emitted:\n{glsl}"
    );
    assert!(
        glsl.contains("cos(angle)"),
        "the cos call must survive:\n{glsl}"
    );
    assert!(
        glsl.contains("sin(angle)"),
        "the sin call must survive:\n{glsl}"
    );
    assert_no_bare_binding(&glsl, "cos");
    assert_no_bare_binding(&glsl, "sin");
}

/// Entry mode: `entry_reserved_names` covers interface globals but not builtin
/// callables, so an entry local named `cos` shadows the builtin too.
#[test]
fn entry_local_named_cos_renamed_apart_from_builtin() {
    let src = r#"
        @shader(fragment)
        fn spin_frag(tc: Vec2) -> Vec4 {
            let cos = tc.x.cos()
            return Vec4 { x: cos + tc.y.cos(), y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let e = emit_entry(src, "spin_frag");
    let glsl = &e.source;
    assert!(
        glsl.contains("cos(tc.x)"),
        "the builtin call must survive:\n{glsl}"
    );
    assert_no_bare_binding(glsl, "cos");
    assert!(
        glsl.contains("cos_1 = cos(tc.x)"),
        "the renamed local must hold the builtin result:\n{glsl}"
    );
}
