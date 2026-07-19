//! A2 vertex+fragment pipeline (docs/shaders.md): whole-program shader
//! compilation. RED-first scaffold -- these pin the A2a target behavior and go
//! green as the emitter lands.
//!
//! Two harnesses:
//!   - `emit_entry`  : A1 fragment entry, reused to exercise the new GPU builtins
//!                     (frag_coord/fwidth/discard) in isolation.
//!   - `emit_pipeline`: the A2 vertex+fragment pair -> one #ifdef file.

use skull_codegen_glsl::GlslProfile;
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

fn workspace(src: &str) -> (bone_semantic::inference::Workspace, bone_semantic::prelude::ModuleId) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

/// A1 fragment entry -- for the GPU-builtin goldens.
fn emit_entry(src: &str, root: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_entry(&ws, mid, root).map_err(|e| e.message)
}

/// The A2 vertex+fragment pipeline.
fn emit_pipeline(src: &str, vert: &str, frag: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline(&ws, mid, vert, frag, GlslProfile::Core330)
        .map_err(|e| e.message)
}

fn emit_pipeline_profile(src: &str, vert: &str, frag: &str, p: GlslProfile) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline(&ws, mid, vert, frag, p).map_err(|e| e.message)
}

// --- GPU builtins (decision 5): frag_coord / fwidth / discard ----------------
// Fragment-stage GPU-only intrinsics. Exercised through the A1 fragment entry
// (they add no params, so the A1 contract still holds). RED until the
// GLSL_INTRINSICS table backs them (today: "no emittable body and no GLSL
// intrinsic mapping").

#[test]
fn builtin_fwidth_maps() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            let w = fwidth(tc.x)
            return Vec4 { x: w, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_entry(src, "f").expect("emit");
    assert!(glsl.contains("fwidth(tc.x)"), "got:\n{glsl}");
}

#[test]
fn builtin_frag_coord_maps() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            let c = frag_coord()
            return Vec4 { x: c.x, y: c.y, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_entry(src, "f").expect("emit");
    assert!(glsl.contains("gl_FragCoord"), "got:\n{glsl}");
    // frag_coord() takes no args -- the template is a bare builtin, not a call.
    assert!(!glsl.contains("frag_coord("), "must not emit a fn call:\n{glsl}");
}

#[test]
fn builtin_discard_is_a_statement() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            if tc.x < 0.5 { discard() }
            return Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_entry(src, "f").expect("emit");
    // discard is a control-flow statement, not an expression: `discard;`, never
    // `discard()` or an assignment to a temp.
    assert!(glsl.contains("discard;"), "got:\n{glsl}");
    assert!(!glsl.contains("discard()"), "discard must not render as a call:\n{glsl}");
}

/// texture_size(s) -> `vec2(textureSize(s, 0))`: the sampler's mip-0 dimensions
/// as a float Vec2 (used for gl_FragCoord-based canvas sampling in sprite_batch).
/// Not stage-restricted (textureSize is legal in every GLSL stage).
#[test]
fn builtin_texture_size_maps() {
    let src = r#"
        @shader(fragment)
        fn f(tex: Sampler2D, tc: Vec2) -> Vec4 {
            let sz = texture_size(tex)
            return Vec4 { x: sz.x, y: sz.y, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_entry(src, "f").expect("emit");
    assert!(glsl.contains("vec2(textureSize(tex, 0))"), "got:\n{glsl}");
    assert!(!glsl.contains("texture_size("), "must not emit a fn call:\n{glsl}");
}

// --- the vertex+fragment pipeline scaffold (A2a) -----------------------------

/// The mag-UI-shaped port (ui_sdf_bar): a per-vertex attribute struct, a Mat4
/// uniform block, and a varyings struct whose `position` field drives
/// gl_Position and is NOT emitted as a varying.
const UI_SRC: &str = r#"
    value QuadVert { aPos: Vec2, aTexCoord: Vec2, aColor: Vec4 }
    value UiVertU  { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }
    value UiVary   { position: Vec4, TexCoord: Vec2, Color: Vec4 }
    value UiFragU  { uSize: Vec2, uRatio: float }

    @shader(vertex)
    fn ui_vert(@attr a: QuadVert, @uniform u: UiVertU) -> UiVary {
        let clip = u.uProjection * u.uView * u.uTransform * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
        return UiVary { position: clip, TexCoord: a.aTexCoord, Color: a.aColor }
    }

    @shader(fragment)
    fn ui_frag(vary: UiVary, @uniform u: UiFragU) -> Vec4 {
        let fe = fwidth(vary.TexCoord.x)
        return Vec4 { x: vary.Color.x, y: fe, z: u.uRatio, w: vary.Color.w }
    }
"#;

#[test]
fn pipeline_scaffold_vertex_block() {
    let glsl = emit_pipeline(UI_SRC, "ui_vert", "ui_frag").expect("emit");
    assert!(glsl.starts_with("#version 330 core\n"), "got:\n{glsl}");
    assert!(glsl.contains("#ifdef VERTEX"), "got:\n{glsl}");
    // Attribute struct fields -> `in` with declaration-order locations.
    assert!(glsl.contains("layout (location = 0) in vec2 aPos;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 1) in vec2 aTexCoord;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 2) in vec4 aColor;"), "got:\n{glsl}");
    // Uniform block fields -> `uniform`, declaration order.
    assert!(glsl.contains("uniform mat4 uProjection;"), "got:\n{glsl}");
    assert!(glsl.contains("uniform mat4 uView;"), "got:\n{glsl}");
    assert!(glsl.contains("uniform mat4 uTransform;"), "got:\n{glsl}");
    // Varyings out -- position is suppressed (it is gl_Position, not a varying).
    assert!(glsl.contains("out vec2 TexCoord;"), "got:\n{glsl}");
    assert!(glsl.contains("out vec4 Color;"), "got:\n{glsl}");
    assert!(!glsl.contains("out vec4 position;"), "position must not be a varying:\n{glsl}");
    // main writes gl_Position from the position field + the varying outs.
    assert!(glsl.contains("gl_Position ="), "got:\n{glsl}");
    assert!(glsl.contains("TexCoord = "), "got:\n{glsl}");
}

#[test]
fn pipeline_scaffold_pixel_block() {
    let glsl = emit_pipeline(UI_SRC, "ui_vert", "ui_frag").expect("emit");
    assert!(glsl.contains("#ifdef PIXEL"), "got:\n{glsl}");
    // Varyings arrive as `in`, matching the vertex `out` (position excluded).
    assert!(glsl.contains("in vec2 TexCoord;"), "got:\n{glsl}");
    assert!(glsl.contains("in vec4 Color;"), "got:\n{glsl}");
    assert!(glsl.contains("out vec4 FragColor;"), "got:\n{glsl}");
    // Fragment uniform block.
    assert!(glsl.contains("uniform vec2 uSize;"), "got:\n{glsl}");
    assert!(glsl.contains("uniform float uRatio;"), "got:\n{glsl}");
    assert!(glsl.contains("void main()"), "got:\n{glsl}");
    assert!(glsl.contains("FragColor = ui_frag("), "got:\n{glsl}");
}

/// Both profiles emit; only the version line (and precision preamble) differ.
#[test]
fn pipeline_es300_profile() {
    let glsl = emit_pipeline_profile(UI_SRC, "ui_vert", "ui_frag", GlslProfile::Es300).expect("emit");
    assert!(glsl.starts_with("#version 300 es\n"), "got:\n{glsl}");
    assert!(glsl.contains("precision highp float;"), "got:\n{glsl}");
}

// --- pipeline rejects (spanned emit CompilerError) ---------------------------

fn reject_pipeline(src: &str, vert: &str, frag: &str, needle: &str) {
    match emit_pipeline(src, vert, frag) {
        Ok(glsl) => panic!("expected rejection ({needle}) but emitted:\n{glsl}"),
        Err(msg) => assert!(msg.contains(needle), "rejection should mention `{needle}`, got: {msg}"),
    }
}

/// The varyings struct MUST carry a `position: Vec4` field (-> gl_Position).
#[test]
fn reject_varyings_missing_position() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { TexCoord: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: vary.TexCoord.x, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "position");
}

/// `position` is gl_Position, not a varying -- reading it in the fragment stage
/// is a spanned reject.
#[test]
fn reject_fragment_reads_position() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, TexCoord: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    reject_pipeline(src, "vt", "fr", "position");
}

/// The fragment's first param type MUST equal the vertex's return type.
#[test]
fn reject_varyings_type_mismatch() {
    let src = r#"
        value QV { aPos: Vec2 }
        value VaryA { position: Vec4, TexCoord: Vec2 }
        value VaryB { position: Vec4, Other: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> VaryA { return VaryA { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: VaryB) -> Vec4 { return Vec4 { x: vary.Other.x, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "varyings");
}

/// A value-struct vertex param with no `@attr`/`@uniform`/`@instance` role is
/// ambiguous -- reject (the "never mixed" guarantee).
#[test]
fn reject_unmarked_vertex_struct_param() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    // (fragment reads position too, but the unmarked-param reject fires first.)
    reject_pipeline(src, "vt", "fr", "@attr");
}

/// An int-typed varying without `@flat` is a spanned reject (GLSL mandates flat
/// integer interpolation). This check lives in the emitter, not the registry.
#[test]
fn reject_int_varying_without_flat() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, kind: int }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, kind: 3 } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "flat");
}

/// Decision 7: BOTH stages' names share ONE collision namespace (the linked GL
/// program has one uniform namespace) -- a vertex uniform and a fragment
/// uniform with the same name reject at emit, not at link.
#[test]
fn reject_cross_stage_name_collision() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value VU { uShared: float }
        value FU { uShared: float }
        @shader(vertex)
        fn vt(@attr a: QV, @uniform u: VU) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: u.uShared, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: FU) -> Vec4 { return Vec4 { x: u.uShared, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "collide");
}

/// A global-base struct param (attrs/uniforms/varyings) reads field-by-field --
/// its fields are GLSL globals; the struct itself has no GLSL identity. Copying
/// it into a local must reject, not emit a dangling reference to the dropped
/// param name.
#[test]
fn reject_global_struct_param_alias() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, TexCoord: Vec2 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            let v = vary
            return Vec4 { x: v.TexCoord.x, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    reject_pipeline(src, "vt", "fr", "field-by-field");
}

/// `in bool` / `out bool` are illegal GLSL: bool attributes and bool varyings
/// reject at classification (uniform bool stays legal).
#[test]
fn reject_bool_varying() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, ok: bool }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, ok: true } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "bool");
}

#[test]
fn reject_bool_attribute() {
    let src = r#"
        value QV { aPos: Vec2, flag: bool }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "bool");
}

/// `uniform bool` IS legal GLSL -- the bool restriction is attrs/varyings only.
#[test]
fn bool_uniform_is_allowed() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value FU { uEnabled: bool }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: FU) -> Vec4 {
            if u.uEnabled { return Vec4 { x: 1.0, y: 1.0, z: 1.0, w: 1.0 } }
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert!(glsl.contains("uniform bool uEnabled;"), "got:\n{glsl}");
}

// --- A2b: MRT fragment output (value-struct-of-Vec4 return) ------------------
// A fragment entry may return a value struct whose fields are all Vec4 -> one
// `layout(location=N) out vec4 <field>` per field, in declaration order. The
// single-Vec4 return (FragColor only) stays unchanged.

const MRT_SRC: &str = r#"
    value QV   { aPos: Vec2, aEntityId: float }
    value Vary { position: Vec4, TexCoord: Vec2, @flat EntityId: float }
    value SpriteOut { FragColor: Vec4, FragId: Vec4 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos, EntityId: a.aEntityId }
    }
    @shader(fragment)
    fn fr(vary: Vary, uTexture: Sampler2D) -> SpriteOut {
        let c = texture(uTexture, vary.TexCoord)
        return SpriteOut { FragColor: c, FragId: Vec4 { x: vary.EntityId, y: 0.0, z: 0.0, w: 1.0 } }
    }
"#;

#[test]
fn mrt_fragment_outputs_by_field_order() {
    let glsl = emit_pipeline(MRT_SRC, "vt", "fr").expect("emit");
    // Field order == layout location, mirroring the @attr location contract.
    assert!(glsl.contains("layout(location = 0) out vec4 FragColor;"), "got:\n{glsl}");
    assert!(glsl.contains("layout(location = 1) out vec4 FragId;"), "got:\n{glsl}");
    // MRT frag is `void main()` with destructured assignments -- no `vec4 fr()`
    // returning + a `FragColor = fr();` wrapper.
    assert!(glsl.contains("void main()"), "got:\n{glsl}");
    assert!(glsl.contains("FragColor = "), "got:\n{glsl}");
    assert!(glsl.contains("FragId = "), "got:\n{glsl}");
    assert!(!glsl.contains("FragColor = fr("), "MRT must not emit a single-output wrapper:\n{glsl}");
    // The bare (non-layout) single-output declaration must be absent.
    assert!(!glsl.contains("\nout vec4 FragColor;"), "MRT must not emit the bare FragColor out:\n{glsl}");
}

/// Both profiles emit the same MRT outputs (only the version/precision differ).
#[test]
fn mrt_fragment_es300() {
    let glsl = emit_pipeline_profile(MRT_SRC, "vt", "fr", GlslProfile::Es300).expect("emit");
    assert!(glsl.starts_with("#version 300 es\n"), "got:\n{glsl}");
    assert!(glsl.contains("layout(location = 0) out vec4 FragColor;"), "got:\n{glsl}");
    assert!(glsl.contains("layout(location = 1) out vec4 FragId;"), "got:\n{glsl}");
}

/// Single-Vec4 fragment return is unchanged by the MRT work.
#[test]
fn single_output_fragment_still_wraps() {
    let glsl = emit_pipeline(UI_SRC, "ui_vert", "ui_frag").expect("emit");
    assert!(glsl.contains("out vec4 FragColor;"), "got:\n{glsl}");
    assert!(glsl.contains("FragColor = ui_frag("), "got:\n{glsl}");
    assert!(!glsl.contains("layout(location = 0) out vec4"), "single output has no layout:\n{glsl}");
}

/// Every MRT output field must be Vec4 (each is a `out vec4`); a non-Vec4 field
/// rejects.
#[test]
fn reject_mrt_non_vec4_field() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value BadOut { FragColor: Vec4, depth: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> BadOut { return BadOut { FragColor: Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }, depth: 0.5 } }
    "#;
    reject_pipeline(src, "vt", "fr", "Vec4");
}

/// MRT output names join the one cross-stage namespace -- an output name that
/// collides with a uniform rejects.
#[test]
fn reject_mrt_output_name_collision() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value FU { FragId: float }
        value Out2 { FragColor: Vec4, FragId: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary, @uniform u: FU) -> Out2 {
            return Out2 { FragColor: Vec4 { x: u.FragId, y: 0.0, z: 0.0, w: 1.0 }, FragId: Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
        }
    "#;
    reject_pipeline(src, "vt", "fr", "collide");
}

// --- builtin stage gating: frag_coord/fwidth/discard are fragment-only -------

// --- A2c: the instanced tier -- @instance attrs continue the locations -------

/// The grass shape in miniature: per-vertex quad attrs at locations 0..1, the
/// instance struct continuing at 2..5 (the gpu_mesh sequential-location
/// contract), a @flat varying, and vertex_id-gated tip work.
#[test]
fn instanced_locations_continue_across_structs() {
    let src = r#"
        value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
        value GrassInst { InstancePosition: Vec2, InstanceRotation: float, InstanceScale: float, InstanceHeight: float }
        value Vary { position: Vec4, TexCoord: Vec2, @flat y_pos: float }
        @shader(vertex)
        fn vt(@attr a: QuadVert, @instance inst: GrassInst) -> Vary {
            let bend = 0.0
            let vid = vertex_id() % 4
            if vid == 0 || vid == 1 {
                bend = 0.1
            }
            let px = inst.InstancePosition.x + a.aPos.x * inst.InstanceScale + bend
            let py = inst.InstancePosition.y + a.aPos.y * inst.InstanceHeight
            return Vary {
                position: Vec4 { x: px, y: py, z: 0.0, w: 1.0 },
                TexCoord: a.aTexCoord,
                y_pos: inst.InstancePosition.y,
            }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            return Vec4 { x: vary.TexCoord.x, y: vary.y_pos, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert!(glsl.contains("layout (location = 0) in vec2 aPos;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 1) in vec2 aTexCoord;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 2) in vec2 InstancePosition;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 3) in float InstanceRotation;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 4) in float InstanceScale;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 5) in float InstanceHeight;"), "got:\n{glsl}");
    assert!(glsl.contains("flat out float y_pos;"), "got:\n{glsl}");
    assert!(glsl.contains("flat in float y_pos;"), "got:\n{glsl}");
    assert!(glsl.contains("gl_VertexID"), "got:\n{glsl}");
}

// --- UniformArray<T> + at() + @size(N) (decision 3; pulled into A2c by grass) -

/// The grass shape: a vertex-stage uniform array read in a dynamic-bound loop.
#[test]
fn uniform_array_vertex_decl_and_at() {
    let src = r#"
        value QV { aPos: Vec2 }
        value VU { entity_count: int }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @uniform u: VU, @size(32) entity_positions: UniformArray<Vec2>) -> Vary {
            let off = v(0.0, 0.0)
            for i in 0..min(u.entity_count, 32) {
                off = off + at(entity_positions, i)
            }
            return Vary { position: Vec4 { x: a.aPos.x + off.x, y: off.y, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert!(glsl.contains("uniform vec2 entity_positions[32];"), "got:\n{glsl}");
    assert!(glsl.contains("entity_positions["), "at() must render as indexing:\n{glsl}");
    assert!(!glsl.contains("at("), "at() must not render as a call:\n{glsl}");
    // The loop-bound temp for min(int, int) must be int: ES300 has no implicit
    // int->float on assignment, so the float fallback fails to compile there.
    let line = glsl
        .lines()
        .find(|l| l.contains("= min(entity_count, 32);"))
        .expect("min bound assignment");
    let name = line.trim().split(" = ").next().unwrap();
    assert!(glsl.contains(&format!("int {name};")), "bound temp `{name}` must be int:\n{glsl}");
}

#[test]
fn uniform_array_fragment_stage() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary, @size(4) weights: UniformArray<float>) -> Vec4 {
            let w0 = at(weights, 0)
            return Vec4 { x: w0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert!(glsl.contains("uniform float weights[4];"), "got:\n{glsl}");
    assert!(glsl.contains("weights[0]"), "got:\n{glsl}");
}

#[test]
fn reject_uniform_array_without_size() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, entity_positions: UniformArray<Vec2>) -> Vary {
            return Vary { position: Vec4 { x: at(entity_positions, 0).x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "@size");
}

/// Element types are the corpus set {float, int, Vec2, Vec4} -- Mat4 rejects.
#[test]
fn reject_uniform_array_bad_elem() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @size(4) mats: UniformArray<Mat4>) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    reject_pipeline(src, "vt", "fr", "element");
}

// --- vertex_id: vertex-only builtin (A2c) ------------------------------------

#[test]
fn builtin_vertex_id_maps() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            let vid = vertex_id() % 4
            let bend = 0.0
            if vid == 0 || vid == 1 {
                bend = 1.0
            }
            return Vary { position: Vec4 { x: a.aPos.x + bend, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 } }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert!(glsl.contains("gl_VertexID"), "got:\n{glsl}");
    assert!(!glsl.contains("vertex_id("), "must not emit a fn call:\n{glsl}");
}

#[test]
fn reject_vertex_id_in_fragment_stage() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            let vid = vertex_id()
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    reject_pipeline(src, "vt", "fr", "vertex-stage builtin");
}

/// The A1 fragment-only entry is also a fragment stage -- vertex_id rejects.
#[test]
fn reject_vertex_id_in_a1_entry() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            let vid = vertex_id()
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    match emit_entry(src, "f") {
        Ok(glsl) => panic!("expected rejection but emitted:\n{glsl}"),
        Err(msg) => assert!(msg.contains("vertex-stage builtin"), "rejection should mention `vertex-stage builtin`, got: {msg}"),
    }
}

#[test]
fn reject_fwidth_in_vertex() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            let w = fwidth(a.aPos.x)
            return Vary { position: Vec4 { x: w, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    reject_pipeline(src, "vt", "fr", "fragment");
}
