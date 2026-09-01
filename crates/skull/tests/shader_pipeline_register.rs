//! Driver contract for the `rock_shader` in-game shader-pipeline intrinsic
//! (docs/shaders.md). A call `rock_shader(vert, frag)`
//! (two direct references to top-level `@shader` fns) collapses at emit to the
//! pipeline key const `"<vert>+<frag>"`, and the driver emits exactly one
//! top-level `__rock_shader_register` carrying the GLSL and semantic wires per DISTINCT pair -- N
//! call sites of the same pair dedup to one registration. Re-emission is
//! byte-identical (incremental byte-identity gate).
//!
//! The GLSL emit shape itself is pinned by the skull-codegen-glsl goldens; here
//! we pin the driver wiring (collapse + dedup + top-level registration).
use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use crate::common::{emit_module, function_body, ws_with_stdlib};
use skull::mir_emit::{
    emit_full, emit_incremental, emit_overlay as emit_overlay_result, MirEmit, SkullEmitCache,
};

const TYPED_PROJECTION_REFUSAL_SRC: &str = r#"
struct EntityIdTex { id_buffer: ISampler2D }

@shader(fragment)
fn entity_id_debug_frag(@textures t: EntityIdTex, tc: Vec2) -> Vec4 {
    let id = texture_id(t.id_buffer, tc)
    if id <= 0 { discard() }
    let x = int_to_float(id)
    return v(x, x, x, 1.0)
}

value Quad { aPos: Vec2, aTexCoord: Vec2 }
value Vary { position: Vec4, TexCoord: Vec2 }
struct OutlineTex { target_mask: ISampler2D, visible_id: ISampler2D }

@shader(vertex)
fn outline_detect_vert(@attr a: Quad) -> Vary {
    return Vary {
        position: v(a.aPos, 0.0, 1.0),
        TexCoord: a.aTexCoord,
    }
}

@shader(fragment)
fn outline_detect_frag(vary: Vary, @textures t: OutlineTex) -> Vec4 {
    let target = texture_id(t.target_mask, vary.TexCoord)
    let visible = texture_id(t.visible_id, vary.TexCoord)
    if target == visible { return v(0.0, 1.0, 0.0, 1.0) }
    return v(1.0, 0.0, 0.0, 1.0)
}

fn use_all() -> string {
    let entity = rock_shader(entity_id_debug_frag)
    let outline = rock_shader(outline_detect_vert, outline_detect_frag)
    return entity + outline
}
"#;

const NONFINITE_PROJECTION_SRC: &str = r#"
struct SrcTex { src_tex: Sampler2D }

@shader(fragment)
fn nonfinite_frag(@textures t: SrcTex, tc: Vec2) -> Vec4 {
    let c = texture(t.src_tex, tc)
    let bad = bvec3(
        isinf(c.r) || isnan(c.r),
        isinf(c.g) || isnan(c.g),
        isinf(c.b) || isnan(c.b),
    )
    if any(bad) { return v(to_vec3(bad), 1.0) }
    if any(greater_than(c.rgb.abs(), v(60000.0, 60000.0, 60000.0))) {
        return v(1.0, 1.0, 0.0, 1.0)
    }
    return v(0.0, 0.0, 0.0, 1.0)
}

fn use_all() -> string { return rock_shader(nonfinite_frag) }
"#;

const OUTLINE_PROJECTION_SRC: &str = concat!(
    r#"
struct MainTex { uTexture: Sampler2D }
value Vertex { pos: Vec2, uv: Vec2, color: Vec4, pivot_y: float, entity_id: float }
    value EngineU { uProjection: Mat4, uView: Mat4 }
    "#,
    include_str!("../../../stone/engine/shaders/common/utils_math.rock"),
    include_str!("../../../stone/engine/shaders/rendering/outline_target_shader.rock"),
    include_str!("../../../stone/engine/shaders/rendering/outline_detect_shader.rock"),
    r#"
fn use_all() -> string {
    let target = rock_shader(outline_target_vert, outline_target_frag)
    let detect = rock_shader(outline_detect_vert, outline_detect_frag)
    return target + detect
}
"#,
);

fn emit_overlay(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> MirEmit {
    emit_overlay_result(ws, cache, reinferred).expect("emit_overlay")
}

/// One shared vertex + one fragment, referenced from TWO call sites (two fns).
const SRC: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn ui_sim_solid_frag(vary: UiVary) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}

fn make_a() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }
fn make_b() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }
"#;

const KEY: &str = "ui_vert+ui_sim_solid_frag";

#[test]
fn call_collapses_to_key_const() {
    let lua = emit_module(SRC);
    let body = function_body(&lua, "make_a");
    assert!(body.contains(&format!("\"{KEY}\"")), "key const missing in make_a:\n{body}");
    assert!(!body.contains("rock_shader("), "unexpanded rock_shader call:\n{body}");
    assert!(!body.contains("compile-time shader-pipeline intrinsic"),
        "phantom body leaked into make_a:\n{body}");
}

#[test]
fn one_registration_per_distinct_pair() {
    let lua = emit_module(SRC);
    let needle = format!("__rock_shader_register(\"{KEY}\"");
    let count = lua.matches(&needle).count();
    assert_eq!(count, 1, "two call sites of one pair must emit exactly one registration, got {count}:\n{lua}");
}

#[test]
fn registration_carries_generated_glsl() {
    let lua = emit_module(SRC);
    let at = lua.find("__rock_shader_register(").expect("a registration");
    let reg = &lua[at..];
    for needle in ["#version 330 core", "#ifdef VERTEX", "#ifdef PIXEL", "layout (location = 0) in vec2 aPos;"] {
        assert!(reg.contains(needle), "registration missing `{needle}`:\n{reg}");
    }
}

#[test]
fn explicit_vertex_registration_marks_that_it_is_not_fullscreen() {
    let lua = emit_module(SRC);
    let at = lua.find("__rock_shader_register(").expect("a registration");
    let reg = &lua[at..];
    assert!(
        reg.contains("false, [[") && reg.contains("fullscreen 0"),
        "an explicit vertex artifact must register as non-fullscreen:\n{reg}"
    );
}

#[test]
fn reemission_is_byte_identical() {
    assert_eq!(emit_module(SRC), emit_module(SRC), "re-emission must be byte-identical");
}

// -- Hot-reload parity: a reload re-`dofile`s the chunk (full) or the overlay
// (incremental body edit), and either must carry the EDITED shader source so the
// engine's hash-idempotent register recompiles the GL program in place. --

/// One shared vertex + a fragment whose magenta X channel is parameterized, so a
/// "body edit" is a real content change to the generated GLSL.
fn src_with_frag(x: &str) -> String {
    format!(
        r#"
value QuadVert {{ aPos: Vec2, aTexCoord: Vec2 }}
value UiVertU  {{ uProjection: Mat4, uView: Mat4, uTransform: Mat4 }}
value UiVary   {{ position: Vec4, TexCoord: Vec2 }}

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {{
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 {{ x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }}
    return UiVary {{ position: clip, TexCoord: a.aTexCoord }}
}}

@shader(fragment)
fn ui_sim_solid_frag(vary: UiVary) -> Vec4 {{ return Vec4 {{ x: {x}, y: 0.0, z: 1.0, w: 1.0 }} }}

fn use_it() -> string {{ return rock_shader(ui_vert, ui_sim_solid_frag) }}
"#
    )
}

fn test_mid(ws: &Workspace) -> ModuleId {
    *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("<test> module id")
}

fn frag_body_id(ws: &Workspace) -> (ModuleId, BodyOwner) {
    let mid = test_mid(ws);
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == "ui_sim_solid_frag" {
            return (mid, BodyOwner::Function(fid));
        }
    }
    panic!("no ui_sim_solid_frag fn");
}

fn body_id_named(ws: &Workspace, name: &str) -> (ModuleId, BodyOwner) {
    let mid = test_mid(ws);
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == name {
            return (mid, BodyOwner::Function(fid));
        }
    }
    panic!("no {name} fn");
}

#[test]
fn changed_fragment_body_changes_registration() {
    let a = emit_module(&src_with_frag("1.0"));
    let b = emit_module(&src_with_frag("0.5"));
    assert!(a.contains("vec4(1.0, 0.0, 1.0, 1.0)"), "v-a magenta:\n{a}");
    assert!(b.contains("vec4(0.5, 0.0, 1.0, 1.0)"), "v-b edited color:\n{b}");
    assert_ne!(a, b, "an edited shader body must change the emitted registration");
}

#[test]
fn incremental_shader_edit_stays_byte_identical() {
    let ws1 = ws_with_stdlib(&src_with_frag("1.0"));
    let mid1 = test_mid(&ws1);
    let (_full1, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");

    let ws2 = ws_with_stdlib(&src_with_frag("0.5"));
    let mid2 = test_mid(&ws2);
    let reinferred = vec![frag_body_id(&ws2)];
    let inc = emit_incremental(&ws2, &mut cache, &reinferred).expect("emit_incremental");
    let fresh = emit_full(&ws2, mid2).expect("emit_full v2").0;
    assert_eq!(inc.lua, fresh.lua, "incremental shader-edit assembly != fresh full emit");
}

#[test]
fn overlay_carries_edited_shader_registration() {
    let ws1 = ws_with_stdlib(&src_with_frag("1.0"));
    let mid1 = test_mid(&ws1);
    let (_full1, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");

    let ws2 = ws_with_stdlib(&src_with_frag("0.5"));
    let reinferred = vec![frag_body_id(&ws2)];
    let overlay = emit_overlay(&ws2, &mut cache, &reinferred);
    assert!(
        overlay.lua.contains("__rock_shader_register(\"ui_vert+ui_sim_solid_frag\""),
        "the incremental overlay must re-run the registration on reload:\n{}",
        overlay.lua
    );
    assert!(
        overlay.lua.contains("vec4(0.5, 0.0, 1.0, 1.0)"),
        "the overlay registration must carry the EDITED fragment source:\n{}",
        overlay.lua
    );
}

#[test]
fn unrelated_body_overlay_omits_shader_registrations() {
    let src = format!(
        "{}\nfn unrelated_shader_user() -> int {{ return 1 }}\n",
        src_with_frag("1.0")
    );
    let ws = ws_with_stdlib(&src);
    let mid = test_mid(&ws);
    let (_full, mut cache) = emit_full(&ws, mid).expect("emit_full");
    let overlay = emit_overlay(&ws, &mut cache, &[body_id_named(&ws, "unrelated_shader_user")]);
    assert!(
        !overlay.lua.contains("__rock_shader_register("),
        "an unrelated body edit must not rebuild shader registrations:\n{}",
        overlay.lua
    );
}

fn src_with_shader_helper(x: &str) -> String {
    src_with_frag(&format!("shader_color_component({x})")).replace(
        "@shader(fragment)",
        "fn shader_color_component(x: float) -> float { return x }\n\n@shader(fragment)",
    )
}

#[test]
fn shader_helper_overlay_carries_affected_registration() {
    let ws1 = ws_with_stdlib(&src_with_shader_helper("1.0"));
    let mid1 = test_mid(&ws1);
    let (_full, mut cache) = emit_full(&ws1, mid1).expect("emit_full v1");

    let ws2 = ws_with_stdlib(
        &src_with_shader_helper("1.0")
            .replace("return x }", "return x * 0.5 }"),
    );
    let overlay = emit_overlay(
        &ws2,
        &mut cache,
        &[body_id_named(&ws2, "shader_color_component")],
    );
    assert!(
        overlay.lua.contains("__rock_shader_register(\"ui_vert+ui_sim_solid_frag\""),
        "a shader helper edit must rebuild its registration:\n{}",
        overlay.lua
    );
    assert!(
        overlay.lua.contains("0.5"),
        "the registration must carry the edited helper source:\n{}",
        overlay.lua
    );
}

fn src_with_optional_call(include_call: bool) -> String {
    let body = if include_call {
        "return rock_shader(ui_vert, ui_sim_solid_frag)"
    } else {
        "return \"none\""
    };
    let base = src_with_frag("1.0").replace(
        "fn use_it() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }\n",
        "",
    );
    format!("{base}\nfn shader_choice() -> string {{ {body} }}\n")
}

#[test]
fn new_shader_callsite_overlay_carries_registration() {
    let ws1 = ws_with_stdlib(&src_with_optional_call(false));
    let mid1 = test_mid(&ws1);
    let (_full, mut cache) = emit_full(&ws1, mid1).expect("emit_full without callsite");

    let ws2 = ws_with_stdlib(&src_with_optional_call(true));
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id_named(&ws2, "shader_choice")]);
    assert!(
        overlay.lua.contains("__rock_shader_register(\"ui_vert+ui_sim_solid_frag\""),
        "adding a shader callsite must register its pipeline:\n{}",
        overlay.lua
    );
}

// -- A3c: single-arg fullscreen form `rock_shader(frag)`. Key = the bare frag
// name (never contains '+', so it cannot collide with a pair key); the driver
// routes vert=None to `emit_shader_entry`, which emits the same complete file
// the A1 single-entry emitter produces (PIXEL body + fullscreen VERTEX
// passthrough). No new GLSL emission -- resolution + routing only. --

const FS_SRC: &str = r#"
@shader(fragment)
fn fs_frag(tc: Vec2) -> Vec4 {
    return Vec4 { x: tc.x, y: 0.0, z: 1.0, w: 1.0 }
}

fn make_fs() -> string { return rock_shader(fs_frag) }
"#;

/// A mixed workspace: one single-arg (fullscreen) and one two-arg (pair) call.
/// Keys "fs_frag" and "ui_vert+ui_sim_solid_frag"; sorted-by-key emission puts
/// the single first.
const MIXED_SRC: &str = r#"
value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
value UiVary   { position: Vec4, TexCoord: Vec2 }

@shader(vertex)
fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
    let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
    return UiVary { position: clip, TexCoord: a.aTexCoord }
}

@shader(fragment)
fn ui_sim_solid_frag(vary: UiVary) -> Vec4 { return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 } }

@shader(fragment)
fn fs_frag(tc: Vec2) -> Vec4 { return Vec4 { x: tc.x, y: 0.0, z: 1.0, w: 1.0 } }

fn make_pair() -> string { return rock_shader(ui_vert, ui_sim_solid_frag) }
fn make_fs() -> string { return rock_shader(fs_frag) }
"#;

#[test]
fn single_arg_call_collapses_to_bare_frag_key() {
    let lua = emit_module(FS_SRC);
    let body = function_body(&lua, "make_fs");
    assert!(body.contains("\"fs_frag\""), "bare frag key const missing in make_fs:\n{body}");
    assert!(!body.contains("rock_shader("), "unexpanded rock_shader call:\n{body}");
    assert!(!body.contains("compile-time shader-pipeline intrinsic"),
        "phantom body leaked into make_fs:\n{body}");
}

#[test]
fn single_arg_registration_carries_both_ifdef_blocks() {
    let lua = emit_module(FS_SRC);
    let needle = "__rock_shader_register(\"fs_frag\"";
    let count = lua.matches(needle).count();
    assert_eq!(count, 1, "one single-arg registration, got {count}:\n{lua}");
    let at = lua.find(needle).expect("a registration");
    let reg = &lua[at..];
    for n in ["#version 330 core", "#ifdef PIXEL", "#ifdef VERTEX"] {
        assert!(reg.contains(n), "single-arg registration missing `{n}`:\n{reg}");
    }
    assert!(
        reg.contains("true, [[") && reg.contains("fullscreen 1"),
        "a fragment-only artifact must register as compiler-owned fullscreen:\n{reg}"
    );
}

#[test]
fn mixed_single_and_pair_register_both_sorted() {
    let lua = emit_module(MIXED_SRC);
    let fs = lua
        .find("__rock_shader_register(\"fs_frag\"")
        .expect("single-arg registration");
    let pair = lua
        .find("__rock_shader_register(\"ui_vert+ui_sim_solid_frag\"")
        .expect("pair registration");
    assert!(fs < pair, "registrations must be sorted by key (fs_frag before ui_vert+...):\n{lua}");
}

#[test]
fn mixed_reemission_byte_identical() {
    assert_eq!(emit_module(MIXED_SRC), emit_module(MIXED_SRC), "re-emission must be byte-identical");
}

#[test]
fn single_arg_overlay_carries_registration() {
    let ws = ws_with_stdlib(MIXED_SRC);
    let mid = test_mid(&ws);
    let (_full, mut cache) = emit_full(&ws, mid).expect("emit_full");
    let reinferred = vec![body_id_named(&ws, "fs_frag")];
    let overlay = emit_overlay(&ws, &mut cache, &reinferred);
    assert!(
        overlay.lua.contains("__rock_shader_register(\"fs_frag\""),
        "the incremental overlay must re-run the single-arg registration:\n{}",
        overlay.lua
    );
}
