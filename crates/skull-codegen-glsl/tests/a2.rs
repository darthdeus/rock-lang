//! A2 vertex+fragment pipeline: whole-program shader
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
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn workspace(src: &str) -> (bone_semantic::inference::Workspace, bone_semantic::prelude::ModuleId) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

/// A1 fragment entry -- for the GPU-builtin goldens.
fn emit_entry(src: &str, root: &str) -> Result<String, String> {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_entry(&ws, mid, root, GlslProfile::Core330).map_err(|e| e.message)
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

/// A UI shader fixture: a per-vertex attribute struct, a Mat4
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

/// A2 gets ONE std140 block PER STAGE, at its own binding index -- not one
/// block shared by both.
///
/// The stages declare different `@uniform` types and cannot be made to share
/// one: `ui_vert` is a single fn consumed by three different fragment stages
/// (solid, SDF bar and dissolve), each with its own uniform
/// struct, so a merged per-program type would need `ui_vert` to declare a
/// different struct per program. Two blocks is also what WGSL/Vulkan/Metal do.
///
/// Verified to compile and link on real drivers in both profiles before this
/// was built (a two-block program through a native GLSL harness).
#[test]
fn pipeline_emits_one_block_per_stage() {
    let glsl = emit_pipeline(UI_SRC, "ui_vert", "ui_frag").expect("emit");
    // The FRAGMENT group is game-authored, so it is a block today.
    assert!(
        glsl.contains("layout(std140) uniform UiFragU {\n    vec2 uSize;\n    float uRatio;\n};\n"),
        "got:\n{glsl}"
    );
    // The VERTEX group is a block too: one per STAGE, each at its own binding.
    assert!(
        glsl.contains("layout(std140) uniform UiVertU {\n    mat4 uProjection;\n    mat4 uView;\n    mat4 uTransform;\n};\n"),
        "got:\n{glsl}"
    );
    // No member falls back to a loose uniform -- a block member has no location,
    // so a by-name setter aimed at one writes nothing.
    assert!(!glsl.contains("uniform mat4 uProjection;"), "got:\n{glsl}");
    // Unnamed instance -- members stay bare globals, so bodies are unchanged.
    assert!(!glsl.contains("u.uSize"), "got:\n{glsl}");
}

#[test]
fn pipeline_scaffold_vertex_block() {
    let glsl = emit_pipeline(UI_SRC, "ui_vert", "ui_frag").expect("emit");
    assert!(glsl.starts_with("#version 330 core\n"), "got:\n{glsl}");
    assert!(glsl.contains("#ifdef VERTEX"), "got:\n{glsl}");
    // Attribute struct fields -> `in` with declaration-order locations.
    assert!(glsl.contains("layout (location = 0) in vec2 aPos;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 1) in vec2 aTexCoord;"), "got:\n{glsl}");
    assert!(glsl.contains("layout (location = 2) in vec4 aColor;"), "got:\n{glsl}");
    // Vertex uniforms are a std140 block -- see `pipeline_emits_one_block_per_stage`.
    assert!(glsl.contains("layout(std140) uniform UiVertU {"), "got:\n{glsl}");
    assert!(glsl.contains("    mat4 uProjection;"), "got:\n{glsl}");
    assert!(glsl.contains("    mat4 uView;"), "got:\n{glsl}");
    assert!(glsl.contains("    mat4 uTransform;"), "got:\n{glsl}");
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
    // The fragment stage's own block, separate from the vertex stage's.
    assert!(
        glsl.contains("layout(std140) uniform UiFragU {\n    vec2 uSize;\n    float uRatio;\n};"),
        "got:\n{glsl}"
    );
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

/// A block `bool` IS legal -- the bool restriction is attrs/varyings only.
/// Verified to compile and link on real drivers in BOTH profiles, which is what
/// lets `SpriteFragU`'s four bools ride a block: GL reads a block bool as 32
/// bits where Rock's C layout models one byte, and that conversion is stated in
/// `WireKind::Bool32` rather than papered over with padding.
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
    assert!(
        glsl.contains("layout(std140) uniform FU {\n    bool uEnabled;\n};"),
        "got:\n{glsl}"
    );
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

// --- fragment depth output: a `FragDepth: float` field is gl_FragDepth -------
// Window-space depth in [0,1]. It is not a colour attachment: it takes no
// `layout(location=N) out` and the colour fields keep the locations they would
// have without it.

const FRAG_DEPTH_SRC: &str = r#"
    value QV   { aPos: Vec2, aDepth: float }
    value Vary { position: Vec4, TexCoord: Vec2, Depth: float }
    value DepthOut { FragColor: Vec4, FragDepth: float, FragId: Vec4 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos, Depth: a.aDepth }
    }
    @shader(fragment)
    fn fr(vary: Vary, uTexture: Sampler2D) -> DepthOut {
        let c = texture(uTexture, vary.TexCoord)
        return DepthOut { FragColor: c, FragDepth: vary.Depth * 0.5, FragId: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 } }
    }
"#;

#[test]
fn frag_depth_field_writes_gl_frag_depth_without_an_out_location() {
    for p in [GlslProfile::Core330, GlslProfile::Es300] {
        let glsl = emit_pipeline_profile(FRAG_DEPTH_SRC, "vt", "fr", p).expect("emit");
        assert!(glsl.contains("layout(location = 0) out vec4 FragColor;"), "got:\n{glsl}");
        assert!(glsl.contains("layout(location = 1) out vec4 FragId;"), "got:\n{glsl}");
        assert!(!glsl.contains("layout(location = 2)"), "FragDepth must not take a location:\n{glsl}");
        assert!(!glsl.contains("out float FragDepth"), "FragDepth is not an out:\n{glsl}");
        assert!(glsl.contains("gl_FragDepth = (Depth * 0.5);"), "got:\n{glsl}");
        assert_eq!(glsl.matches("FragDepth = ").count(), 1, "only gl_FragDepth is assigned:\n{glsl}");
    }
}

#[test]
fn reject_frag_depth_that_is_not_float() {
    let src = FRAG_DEPTH_SRC
        .replace("FragDepth: float", "FragDepth: Vec4")
        .replace("FragDepth: vary.Depth * 0.5", "FragDepth: Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }");
    reject_pipeline(&src, "vt", "fr", "`FragDepth` must be float");
}

#[test]
fn reject_frag_depth_without_a_colour_output() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        value DepthOnly { FragDepth: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @shader(fragment)
        fn fr(vary: Vary) -> DepthOnly { return DepthOnly { FragDepth: 0.5 } }
    "#;
    reject_pipeline(src, "vt", "fr", "colour");
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
//
// These cover the BODY half -- `at()` lowering to indexing, and the int loop
// bound. The array BLOCK the declarations now live in has its own coverage in
// `uniform_array_block_decl.rs`.

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
    assert!(glsl.contains("    vec2 entity_positions[32];"), "got:\n{glsl}");
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
    assert!(glsl.contains("    float weights[4];"), "got:\n{glsl}");
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

/// The premise the backend-neutral fullscreen submission rests on: an A1 entry contributes NO vertex code, so
/// nothing in an A1 program can observe how a backend tessellates the
/// fullscreen rectangle. Two different entries emit the same VERTEX section,
/// byte for byte, and that section reads only the two attributes -- no
/// gl_VertexID, no per-vertex branch, nothing order-dependent.
///
/// The contrast is `builtin_vertex_id_maps` above: an explicit `@shader(vertex)`
/// stage CAN read gl_VertexID, which is why the fullscreen guarantee is scoped
/// to the generated stage rather than claimed for every draw.
#[test]
fn a1_vertex_stage_is_compiler_owned_and_order_blind() {
    let vertex_of = |glsl: &str| -> String {
        let (_, v) = glsl.split_once("#ifdef VERTEX").expect("a VERTEX section");
        v.split("#endif").next().expect("a terminated VERTEX section").to_string()
    };
    let a = emit_entry(
        r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 { return Vec4 { x: tc.x, y: tc.y, z: 0.0, w: 1.0 } }
    "#,
        "f",
    )
    .expect("emit a");
    let b = emit_entry(
        r#"
        @shader(fragment)
        fn g(tc: Vec2) -> Vec4 { return Vec4 { x: 1.0 - tc.y, y: 0.0, z: tc.x, w: 0.5 } }
    "#,
        "g",
    )
    .expect("emit b");

    let (va, vb) = (vertex_of(&a), vertex_of(&b));
    assert_eq!(va, vb, "two A1 entries must share one compiler-owned vertex stage");
    assert!(va.contains("gl_Position = vec4(aPos, 0.0, 1.0)"), "got:\n{va}");
    assert!(va.contains("TexCoord = aTexCoord"), "got:\n{va}");
    assert!(!va.contains("gl_VertexID"), "the generated stage must not read vertex order:\n{va}");
    // And the entry's own body reached only the fragment half.
    assert!(!va.contains("f("), "the entry body must not appear in the vertex stage:\n{va}");
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

// --- bool-vector tier -----------------------------------

/// The emitted GLSL for the nonfinite-debug shape: bvec3 ctor + isnan/isinf +
/// any + greaterThan + the vec3(bvec) bool->float cast, all rendered inline as
/// native GLSL.
#[test]
fn bvec_nonfinite_entry_golden() {
    let src = r#"
        @shader(fragment)
        fn nonfinite_frag(src_tex: Sampler2D, tc: Vec2) -> Vec4 {
            let c = texture(src_tex, tc)
            let bad = bvec3(
                isinf(c.r) || isnan(c.r),
                isinf(c.g) || isnan(c.g),
                isinf(c.b) || isnan(c.b),
            )
            if any(bad) {
                return v(to_vec3(bad), 1.0)
            } else if any(greater_than(c.rgb.abs(), v(60000.0, 60000.0, 60000.0))) {
                return v(1.0, 1.0, 0.0, 1.0)
            } else {
                let luma = c.rgb.dot(v(0.2126, 0.7152, 0.0722))
                let d = luma * 0.05
                return v(d, d, d, 1.0)
            }
        }
    "#;
    let glsl = emit_entry(src, "nonfinite_frag").expect("emit");
    assert!(glsl.contains("bvec3("), "bvec3 ctor inline:\n{glsl}");
    assert!(glsl.contains("isinf(") && glsl.contains("isnan("), "isinf/isnan:\n{glsl}");
    assert!(glsl.contains("any("), "any reduction:\n{glsl}");
    assert!(glsl.contains("greaterThan("), "greaterThan:\n{glsl}");
    assert!(glsl.contains("vec3(bad)"), "to_vec3 bool->float cast:\n{glsl}");
    // The bvec3 ctor renders as the native builtin, never a mangled helper fn.
    assert!(!glsl.contains("bvec3__"), "no mangled bvec3 helper:\n{glsl}");
}

/// GLSL forbids bool-vector interpolants -- a Bvec3 varying field rejects at emit.
#[test]
fn reject_bvec_varying() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4, flags: Bvec3 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary {
                position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 },
                flags: bvec3(true, false, false),
            }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    reject_pipeline(src, "vt", "fr", "bvec3");
}

/// A Bvec3 attribute field rejects (GLSL has no bool vertex inputs).
#[test]
fn reject_bvec_attribute() {
    let src = r#"
        value QV { aPos: Vec2, flags: Bvec3 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    reject_pipeline(src, "vt", "fr", "bvec3");
}

// --- R32I entity-id tier: int fragment outputs + ISampler2D ------------------
// The GLES entity-id path: ids live in R32I
// attachments, so fragment entries write `int` outputs and readers sample an
// `isampler2D` via texture_id(). ES300 note: isampler2D has NO default
// precision in GLSL ES 3.00, so the declaration itself carries `highp`
// (accepted-and-ignored on desktop GLSL).

const INT_MRT_SRC: &str = r#"
    value QV   { aPos: Vec2, aEntityId: float }
    value Vary { position: Vec4, TexCoord: Vec2, @flat EntityId: float }
    value IdOut { FragColor: Vec4, FragId: int }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos, EntityId: a.aEntityId }
    }
    @shader(fragment)
    fn fr(vary: Vary, uTexture: Sampler2D) -> IdOut {
        let c = texture(uTexture, vary.TexCoord)
        return IdOut { FragColor: c, FragId: vary.EntityId as int }
    }
"#;

/// An MRT out-struct field may be `int` -> `layout(location=N) out int`.
#[test]
fn mrt_int_output_field() {
    let glsl = emit_pipeline(INT_MRT_SRC, "vt", "fr").expect("emit");
    assert!(glsl.contains("layout(location = 0) out vec4 FragColor;"), "got:\n{glsl}");
    assert!(glsl.contains("layout(location = 1) out int FragId;"), "got:\n{glsl}");
    assert!(glsl.contains("FragId = "), "got:\n{glsl}");
}

/// Same outputs under ES300 (the profile whose driver this tier exists for).
#[test]
fn mrt_int_output_es300() {
    let glsl = emit_pipeline_profile(INT_MRT_SRC, "vt", "fr", GlslProfile::Es300).expect("emit");
    assert!(glsl.starts_with("#version 300 es\n"), "got:\n{glsl}");
    assert!(glsl.contains("layout(location = 1) out int FragId;"), "got:\n{glsl}");
}

/// A single bare `int` fragment return is a lone `out int` (the silhouette
/// mask pass writes only the id).
#[test]
fn single_int_fragment_output() {
    let src = r#"
        value QV   { aPos: Vec2, aEntityId: float }
        value Vary { position: Vec4, @flat entity_id: float }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, entity_id: a.aEntityId }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> int {
            if vary.entity_id <= 0.0 { discard() }
            return vary.entity_id as int
        }
    "#;
    let glsl = emit_pipeline(src, "vt", "fr").expect("emit");
    assert!(glsl.contains("out int FragColor;"), "got:\n{glsl}");
    assert!(glsl.contains("FragColor = fr("), "got:\n{glsl}");
}

const ISAMPLER_SRC: &str = r#"
    value QV   { aPos: Vec2 }
    value Vary { position: Vec4, TexCoord: Vec2 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }, TexCoord: a.aPos }
    }
    @shader(fragment)
    fn fr(vary: Vary, ids: ISampler2D) -> Vec4 {
        let id = texture_id(ids, vary.TexCoord)
        let ts = texture_size(ids)
        if id <= 0 { discard() }
        return Vec4 { x: 1.0, y: ts.x, z: 0.0, w: 1.0 }
    }
"#;

/// ISampler2D binds like Sampler2D; texture_id() samples `.r` as int;
/// texture_size() overloads. The declaration carries `highp` (no ES default).
#[test]
fn isampler_texture_id() {
    let glsl = emit_pipeline(ISAMPLER_SRC, "vt", "fr").expect("emit");
    assert!(glsl.contains("uniform highp isampler2D ids;"), "got:\n{glsl}");
    assert!(glsl.contains("texture(ids, TexCoord).r"), "got:\n{glsl}");
    assert!(glsl.contains("vec2(textureSize(ids, 0))"), "got:\n{glsl}");
}

/// ES300 profile compiles the same isampler declaration (precision comes from
/// the declaration-site `highp`, not the preamble).
#[test]
fn isampler_es300() {
    let glsl = emit_pipeline_profile(ISAMPLER_SRC, "vt", "fr", GlslProfile::Es300).expect("emit");
    assert!(glsl.contains("uniform highp isampler2D ids;"), "got:\n{glsl}");
}

/// ISampler2D is opaque like Sampler2D: illegal as a return type.
#[test]
fn reject_isampler_return() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary { return Vary { position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 } } }
        @pure fn pick(s: ISampler2D) -> ISampler2D { return s }
        @shader(fragment)
        fn fr(vary: Vary, ids: ISampler2D) -> Vec4 {
            let s = pick(ids)
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    reject_pipeline(src, "vt", "fr", "sampler");
}

// --- Attribute locations are RECORDED, not invented while printing ----------
// Emission consumes the locations
// classification computed; it does not re-derive them with an enumerate()
// index, which assumes one location per field and cannot express a type that
// consumes several.

/// Locations continue ACROSS the @attr and @instance structs -- one numbering
/// for the whole vertex stage, not per-struct. This is an instanced grass
/// shape (quad at 0..1, GrassInstance at 2..5).
#[test]
fn instance_attr_locations_continue_after_the_per_vertex_ones() {
    let src = r#"
        value Quad { aPos: Vec2, aTexCoord: Vec2 }
        value Inst { pos: Vec2, rot: float, scale: float, height: float }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad, @instance i: Inst) -> Vary {
            return Vary { position: v(a.aPos + i.pos, 0.0, 1.0), TexCoord: a.aTexCoord, uv: a.aTexCoord }
        }
        @shader(fragment)
        fn fs(vary: Vary) -> Vec4 { return v(vary.uv, 0.0, 1.0) }
    "#;
    let glsl = emit_pipeline(src, "vs", "fs").expect("must emit");
    // The whole declaration, not location-exists + name-appears-somewhere: the
    // split form passes even when a location binds the wrong field, and the
    // name half matches the fn body.
    for (loc, ty, name) in [
        (0, "vec2", "aPos"),
        (1, "vec2", "aTexCoord"),
        (2, "vec2", "pos"),
        (3, "float", "rot"),
        (4, "float", "scale"),
        (5, "float", "height"),
    ] {
        assert!(
            glsl.contains(&format!("layout (location = {loc}) in {ty} {name};")),
            "want `layout (location = {loc}) in {ty} {name};`:\n{glsl}"
        );
    }
}

/// Attribute GLSL types are the artifact's `AttrKind` spelled for this backend,
/// so every kind maps to exactly one declaration. A wrong entry (F32x3 -> vec4)
/// is a silent geometry corruption: the shader reads a component the buffer
/// never supplied.
#[test]
fn every_attr_kind_emits_its_glsl_type() {
    let src = r#"
        value All { f: float, i: int, v2: Vec2, v3: Vec3, v4: Vec4 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: All) -> Vary {
            return Vary { position: a.v4 + v(a.v3, 0.0) * float(a.i) * a.f, uv: a.v2 }
        }
        @shader(fragment)
        fn fs(vary: Vary) -> Vec4 { return v(vary.uv, 0.0, 1.0) }
    "#;
    let glsl = emit_pipeline(src, "vs", "fs").expect("must emit");
    for (loc, ty, name) in [
        (0, "float", "f"),
        (1, "int", "i"),
        (2, "vec2", "v2"),
        (3, "vec3", "v3"),
        (4, "vec4", "v4"),
    ] {
        assert!(
            glsl.contains(&format!("layout (location = {loc}) in {ty} {name};")),
            "want `layout (location = {loc}) in {ty} {name};`:\n{glsl}"
        );
    }
}

/// A `mat4` attribute consumes FOUR locations in GL, so a one-location-per-field
/// numbering silently misplaces every attribute after it. Nothing in the tree
/// declares one today, and `struct_gl_fields` admits Mat4 in attribute position,
/// so the shape is reachable and currently wrong. Reject it rather than emit a
/// numbering the CPU side cannot match.
#[test]
fn mat4_attribute_rejects_rather_than_misnumbering_what_follows() {
    let src = r#"
        value Bad { m: Mat4, tail: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Bad) -> Vary {
            return Vary { position: a.m * v(a.tail, 0.0, 1.0), uv: a.tail }
        }
        @shader(fragment)
        fn fs(vary: Vary) -> Vec4 { return v(vary.uv, 0.0, 1.0) }
    "#;
    match emit_pipeline(src, "vs", "fs") {
        Ok(glsl) => panic!("a Mat4 attribute must reject, but emitted:\n{glsl}"),
        Err(msg) => assert!(
            msg.contains("m") && msg.to_lowercase().contains("attribute"),
            "the error must name the offending attribute: {msg}"
        ),
    }
}


// --- @textures: the unit is compiler-assigned (P2) ---------------------------
// Sampler params bound by hand-typed name AND hand-assigned unit are the
// silent-collision class -- two bind sites sharing a number bind one texture
// over the other with nothing to report. Grouping them makes the unit a
// property of field order, which the CPU side reads from the artifact.

#[test]
fn textures_group_declares_each_sampler() {
    let src = r#"
        struct GrassTex { uTexture: Sampler2D, mask_tex: Sampler2D, shadowmap_tex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: GrassTex) -> Vec4 {
            return texture(t.uTexture, vary.uv) * texture(t.mask_tex, vary.uv)
        }
    "#;
    let glsl = emit_pipeline(src, "vs", "fs").expect("must emit");
    for n in ["uTexture", "mask_tex", "shadowmap_tex"] {
        assert!(glsl.contains(&format!("uniform sampler2D {n};")), "want `{n}` declared:\n{glsl}");
    }
    // The group param is not itself a GLSL entity -- field reads are bare globals.
    assert!(!glsl.contains("t."), "the group param must not survive into GLSL:\n{glsl}");
}

/// An integer sampler in the group keeps its kind: `isampler2D`, not the float
/// spelling. Reading an r32i id target through a float sampler is undefined.
#[test]
fn textures_group_keeps_int_sampler_kind() {
    let src = r#"
        struct PickTex { color: Sampler2D, ids: ISampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: PickTex) -> Vec4 {
            let id = texture_id(t.ids, vary.uv)
            let c = texture(t.color, vary.uv)
            return Vec4 { x: c.x, y: c.y, z: float(id), w: 1.0 }
        }
    "#;
    let glsl = emit_pipeline(src, "vs", "fs").expect("must emit");
    assert!(glsl.contains("uniform sampler2D color;"), "got:\n{glsl}");
    assert!(glsl.contains("isampler2D ids;"), "got:\n{glsl}");
}

/// `@textures` is exclusive with the other roles: a group is its own binding
/// set, not attribute or uniform-block data.
#[test]
fn textures_conflicting_role_rejects() {
    let src = r#"
        struct T { tex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @uniform @textures t: T) -> Vec4 { return texture(t.tex, vary.uv) }
    "#;
    reject_pipeline(src, "vs", "fs", "conflicting role markers");
}

/// A non-sampler field in the group shifts every slot after it, so it rejects
/// at the artifact rather than emitting a silently misnumbered binding set.
#[test]
fn textures_group_with_a_non_sampler_field_rejects() {
    let src = r#"
        struct T { tex: Sampler2D, scale: float }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary {
            return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: T) -> Vec4 { return texture(t.tex, vary.uv) }
    "#;
    reject_pipeline(src, "vs", "fs", "scale");
}

/// A group and a bare sampler cannot coexist: the bare one keeps a CPU-assigned
/// unit, which can land on a compiler-assigned slot and bind over it.
#[test]
fn textures_group_plus_bare_sampler_rejects() {
    let src = r#"
        struct T { tex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary { return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv } }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: T, loose: Sampler2D) -> Vec4 {
            let a = texture(t.tex, vary.uv)
            let b = texture(loose, vary.uv)
            return Vec4 { x: a.x, y: b.y, z: 0.0, w: 1.0 }
        }
    "#;
    reject_pipeline(src, "vs", "fs", "every sampler must join it");
}

/// Texture units are program-scoped, so two stages cannot declare different
/// groups -- slot 0 would name two different textures.
#[test]
fn textures_groups_differing_across_stages_reject() {
    let src = r#"
        struct VT { vtex: Sampler2D }
        struct FT { ftex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad, @textures t: VT) -> Vary {
            let h = texture(t.vtex, a.aUv)
            return Vary { position: v(a.aPos, h.x, 1.0), uv: a.aUv }
        }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: FT) -> Vec4 { return texture(t.ftex, vary.uv) }
    "#;
    reject_pipeline(src, "vs", "fs", "one program has one group");
}

/// The emission hands the engine the slot order -- the whole compiler-to-engine
/// channel for texture binding.
#[test]
fn emission_reports_texture_slots_in_order() {
    let src = r#"
        struct GrassTex { uTexture: Sampler2D, mask_tex: Sampler2D, shadowmap_tex: Sampler2D }
        value Quad { aPos: Vec2, aUv: Vec2 }
        value Vary { position: Vec4, uv: Vec2 }
        @shader(vertex)
        fn vs(@attr a: Quad) -> Vary { return Vary { position: v(a.aPos, 0.0, 1.0), uv: a.aUv } }
        @shader(fragment)
        fn fs(vary: Vary, @textures t: GrassTex) -> Vec4 {
            let a = texture(t.uTexture, vary.uv)
            let b = texture(t.mask_tex, vary.uv)
            let c = texture(t.shadowmap_tex, vary.uv)
            return Vec4 { x: a.x, y: b.y, z: c.z, w: 1.0 }
        }
    "#;
    let (ws, mid) = workspace(src);
    let e = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws, mid, "vs", "fs", GlslProfile::Core330,
    )
    .expect("must emit");
    let names: Vec<String> = e.abi.texture_units.iter().map(|n| n.to_string()).collect();
    assert_eq!(names, vec!["uTexture", "mask_tex", "shadowmap_tex"]);
}

/// No group -> nothing for the engine to bind by slot; the bare-sampler path is
/// unchanged.
#[test]
fn emission_reports_no_slots_without_a_group() {
    let (ws, mid) = workspace(UI_SRC);
    let e = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws, mid, "ui_vert", "ui_frag", GlslProfile::Core330,
    )
    .expect("must emit");
    assert!(e.abi.texture_units.is_empty(), "got {:?}", e.abi.texture_units);
}

// --- @engine: the engine-owned block at binding 0 ----------------------------

/// `@engine` is a distinct ROLE, not a second `@uniform`: the engine produces
/// it, its lifetime is the pass rather than the draw, and it binds at index 0 so
/// BOTH stages of a program name the same block.
///
/// Shaped like the sprite batch after the port: engine-owned camera/pass state
/// in `EngineU`, and the shader's own vertex data (`uYBounds` is engine-owned so
/// it moves in; grass keeps `entity_count` in its own group) beside it.
const ENGINE_SRC: &str = r#"
    value QuadVert { aPos: Vec2, aTexCoord: Vec2, aColor: Vec4 }
    value EngineU  { uProjection: Mat4, uView: Mat4, uYBounds: Vec2, uTime: float }
    value GrassVertU { entity_count: int }
    value UiVary   { position: Vec4, TexCoord: Vec2, Color: Vec4 }
    value UiFragU  { uSize: Vec2, uRatio: float }

    @shader(vertex)
    fn e_vert(@attr a: QuadVert, @engine e: EngineU, @uniform u: GrassVertU) -> UiVary {
        let clip = e.uProjection * e.uView * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 }
        let k = clamp((a.aPos.y - e.uYBounds.x) / max(e.uYBounds.y - e.uYBounds.x, 0.0001), 0.0, 1.0)
        return UiVary {
            position: Vec4 { x: clip.x, y: clip.y, z: k + float(u.entity_count), w: clip.w },
            TexCoord: a.aTexCoord,
            Color: a.aColor,
        }
    }

    @shader(fragment)
    fn e_frag(vary: UiVary, @engine e: EngineU, @uniform u: UiFragU) -> Vec4 {
        return Vec4 { x: vary.Color.x, y: e.uTime, z: u.uRatio, w: vary.Color.w }
    }
"#;

#[test]
fn engine_block_is_declared_in_both_stages() {
    let glsl = emit_pipeline(ENGINE_SRC, "e_vert", "e_frag").expect("emit");
    let decl = "layout(std140) uniform EngineU {\n    mat4 uProjection;\n    mat4 uView;\n    vec2 uYBounds;\n    float uTime;\n};\n";
    assert_eq!(glsl.matches(decl).count(), 2, "one per stage:\n{glsl}");
    // Unnamed instance, so members stay bare globals in both bodies.
    assert!(glsl.contains("uProjection * uView"), "got:\n{glsl}");
    assert!(!glsl.contains("e.uProjection"), "got:\n{glsl}");
}

/// The shader's OWN group survives beside the engine block -- the case that
/// rules out collapsing everything into one shared camera type (instanced grass
/// mixes engine-owned camera/time with a game-owned `entity_count`).
///
/// Asserted on BOTH stages: the engine block and a shader-owned block coexist at
/// distinct bindings, which is the property here.
#[test]
fn engine_block_coexists_with_a_shader_owned_block() {
    let glsl = emit_pipeline(ENGINE_SRC, "e_vert", "e_frag").expect("emit");
    assert!(glsl.contains("layout(std140) uniform UiFragU {\n    vec2 uSize;\n    float uRatio;\n};\n"), "got:\n{glsl}");
    assert!(glsl.contains("layout(std140) uniform EngineU {"), "got:\n{glsl}");
    // The vertex stage's own group is a block at its own binding.
    assert!(glsl.contains("layout(std140) uniform GrassVertU {\n    int entity_count;\n};\n"), "got:\n{glsl}");
    // ...but the engine block's members never fall back to loose uniforms.
    assert!(!glsl.contains("uniform mat4 uProjection;"), "got:\n{glsl}");
}

/// The binding index comes from the param ROLE, not from position: `@engine` is
/// slot 0, the vertex group 1, the fragment group 2. Reserved that way so slot 0
/// means "the engine's block" even for a program that declares no `@engine` --
/// which is what `register_uniform_blocks` relies on to turn a foreign block
/// away there.
#[test]
fn engine_block_binds_at_index_zero() {
    let (ws, mid) = workspace(ENGINE_SRC);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(&ws, mid, "e_vert", "e_frag", GlslProfile::Core330)
        .expect("emit");
    let bound: Vec<(u32, &str)> =
        em.abi.uniform_blocks.iter().map(|(b, p)| (*b, p.plan.struct_name.as_str())).collect();
    assert_eq!(bound, vec![(0, "EngineU"), (1, "GrassVertU"), (2, "UiFragU")], "role -> binding");
}

/// One block per PROGRAM, so the stages cannot name different engine groups --
/// unlike `@uniform`, where differing per-stage types are the correct shape.
#[test]
fn engine_groups_must_agree_across_stages() {
    let src = ENGINE_SRC.replace(
        "fn e_frag(vary: UiVary, @engine e: EngineU",
        "fn e_frag(vary: UiVary, @engine e: UiFragU",
    );
    let e = emit_pipeline(&src, "e_vert", "e_frag").expect_err("must reject");
    assert!(e.contains("different @engine groups"), "got: {e}");
}

/// `@textures` is exclusive with EVERY other role, `@engine` included. Its
/// checker enumerated the roles that existed when it was written, so adding a
/// fourth silently widened what slips through -- a group is a binding set of
/// its own, never engine-block data.
#[test]
fn textures_and_engine_conflict() {
    let src = r#"
        value QuadVert { aPos: Vec2, aTexCoord: Vec2 }
        value EngineU  { uProjection: Mat4, uView: Mat4 }
        struct Tex     { atlas: Sampler2D }
        value Vary     { position: Vec4, TexCoord: Vec2 }

        @shader(vertex)
        fn t_vert(@attr a: QuadVert, @textures @engine t: Tex) -> Vary {
            return Vary {
                position: Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 },
                TexCoord: a.aTexCoord,
            }
        }

        @shader(fragment)
        fn t_frag(vary: Vary) -> Vec4 {
            return Vec4 { x: vary.TexCoord.x, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let e = emit_pipeline(src, "t_vert", "t_frag").expect_err("must reject");
    assert!(e.contains("@textures"), "got: {e}");
}

// --- block members share the global GLSL namespace ---------------------------
//
// An unnamed-instance block's members are bare globals, so an `@engine` member
// collides with an attribute, sampler, array, varying or MRT output exactly as
// two loose uniforms would. A stage's own group already reaches the census
// through `uniforms` whether or not it became a block; `@engine` is a separate
// ROLE landing in `engine_uniforms`, so it had to be added explicitly.
// Uncaught, each of these is a raw driver error carrying no Rock span.
//
// One rejection per namespace, because the census chains them separately and a
// single case cannot show the others are covered.

/// Builds a pipeline whose ENGINE block declares `name`, against a fixture that
/// already uses that name in another namespace.
fn engine_member_named(name: &str) -> String {
    format!(r#"
    value QuadVert  {{ aPos: Vec2, TexCoordAttr: Vec2 }}
    value EngineU   {{ uProjection: Mat4, {name}: float }}
    value Vary      {{ position: Vec4, TexCoord: Vec2 }}

    @shader(vertex)
    fn c_vert(@attr a: QuadVert, @engine e: EngineU) -> Vary {{
        return Vary {{
            position: e.uProjection * Vec4 {{ x: a.aPos.x, y: a.aPos.y, z: e.{name}, w: 1.0 }},
            TexCoord: a.TexCoordAttr,
        }}
    }}

    @shader(fragment)
    fn c_frag(vary: Vary, tex: Sampler2D, @size(4) tints: UniformArray<Vec4>) -> Vec4 {{
        return texture(tex, vary.TexCoord) + at(tints, 0)
    }}
"#)
}

#[test]
fn engine_member_colliding_with_an_attribute_rejects() {
    let e = emit_pipeline(&engine_member_named("aPos"), "c_vert", "c_frag").unwrap_err();
    assert!(e.contains("`aPos` would collide"), "got: {e}");
}

#[test]
fn engine_member_colliding_with_a_varying_rejects() {
    let e = emit_pipeline(&engine_member_named("TexCoord"), "c_vert", "c_frag").unwrap_err();
    assert!(e.contains("`TexCoord` would collide"), "got: {e}");
}

#[test]
fn engine_member_colliding_with_a_sampler_rejects() {
    let e = emit_pipeline(&engine_member_named("tex"), "c_vert", "c_frag").unwrap_err();
    assert!(e.contains("`tex` would collide"), "got: {e}");
}

#[test]
fn engine_member_colliding_with_a_uniform_array_rejects() {
    let e = emit_pipeline(&engine_member_named("tints"), "c_vert", "c_frag").unwrap_err();
    assert!(e.contains("`tints` would collide"), "got: {e}");
}

/// An MRT output, whose names are ARBITRARY struct fields rather than the
/// reserved `FragColor`. Using `FragColor` here would prove nothing: the
/// single-output form pre-seeds that name as reserved, so such a test passes
/// with `mrt_outputs` removed from the census entirely.
#[test]
fn engine_member_colliding_with_an_mrt_output_rejects() {
    let src = r#"
    value QuadVert { aPos: Vec2 }
    value EngineU  { uProjection: Mat4, FragId: float }
    value Vary     { position: Vec4, @flat EntityId: float }
    value SpriteOut { FragColor: Vec4, FragId: Vec4 }

    @shader(vertex)
    fn c_vert(@attr a: QuadVert, @engine e: EngineU) -> Vary {
        return Vary {
            position: e.uProjection * Vec4 { x: a.aPos.x, y: a.aPos.y, z: 0.0, w: 1.0 },
            EntityId: e.FragId,
        }
    }

    @shader(fragment)
    fn c_frag(vary: Vary) -> SpriteOut {
        return SpriteOut {
            FragColor: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 },
            FragId: Vec4 { x: vary.EntityId, y: 0.0, z: 0.0, w: 1.0 },
        }
    }
"#;
    let e = emit_pipeline(src, "c_vert", "c_frag").unwrap_err();
    assert!(e.contains("`FragId` would collide"), "got: {e}");
}

/// Both sides of admission: the same fixture with a name that collides with
/// nothing must EMIT, or the rejections above would pass for the wrong reason
/// (a fixture that never compiles).
#[test]
fn engine_member_with_a_free_name_emits() {
    let glsl = emit_pipeline(&engine_member_named("uEngineTime"), "c_vert", "c_frag").expect("emit");
    assert!(glsl.contains("layout(std140) uniform EngineU {"), "got:\n{glsl}");
    assert!(glsl.contains("float uEngineTime;"), "got:\n{glsl}");
}

/// A per-draw uniform collision: `BoltFragU.uTime` against
/// an engine clock of the same name. This is the case the raw driver error
/// (`declaration of "uTime" conflicts with previous declaration`) came from, and
/// it is why the engine's clock is `uEngineTime`.
#[test]
fn engine_member_colliding_with_a_stage_block_member_rejects() {
    let src = r#"
    value QuadVert { aPos: Vec2 }
    value EngineU  { uProjection: Mat4, uTime: float }
    value BoltFragU { uTime: float }
    value Vary     { position: Vec4 }

    @shader(vertex)
    fn c_vert(@attr a: QuadVert, @engine e: EngineU) -> Vary {
        return Vary { position: e.uProjection * Vec4 { x: a.aPos.x, y: a.aPos.y, z: e.uTime, w: 1.0 } }
    }

    @shader(fragment)
    fn c_frag(vary: Vary, @uniform u: BoltFragU) -> Vec4 {
        return Vec4 { x: u.uTime, y: 0.0, z: 0.0, w: 1.0 }
    }
"#;
    let e = emit_pipeline(src, "c_vert", "c_frag").unwrap_err();
    assert!(e.contains("`uTime` would collide"), "got: {e}");
}

/// The reserved slot's whole point: a program with NO `@engine` group leaves
/// binding 0 empty rather than sliding its own block into it. Before roles
/// assigned the index, this fixture's fragment block landed at 0 and was
/// indistinguishable from an engine block at registration.
#[test]
fn a_program_without_an_engine_group_leaves_slot_zero_empty() {
    let (ws, mid) = workspace(UI_SRC);
    let em = skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws, mid, "ui_vert", "ui_frag", GlslProfile::Core330,
    )
    .expect("emit");
    let bound: Vec<(u32, &str)> =
        em.abi.uniform_blocks.iter().map(|(b, p)| (*b, p.plan.struct_name.as_str())).collect();
    assert_eq!(bound, vec![(1, "UiVertU"), (2, "UiFragU")], "role -> binding");
    assert!(!em.abi.uniform_blocks.iter().any(|(b, _)| *b == 0), "slot 0 is the engine's");
}

#[test]
fn builtin_dfdx_maps() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            let w = dfdx(tc.x)
            return Vec4 { x: w, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_entry(src, "f").expect("emit");
    assert!(glsl.contains("dFdx(tc.x)"), "got:\n{glsl}");
}

#[test]
fn builtin_dfdy_maps() {
    let src = r#"
        @shader(fragment)
        fn f(tc: Vec2) -> Vec4 {
            let w = dfdy(tc.x)
            return Vec4 { x: w, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let glsl = emit_entry(src, "f").expect("emit");
    assert!(glsl.contains("dFdy(tc.x)"), "got:\n{glsl}");
}

#[test]
fn reject_dfdx_in_vertex() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            let w = dfdx(a.aPos.x)
            return Vary { position: Vec4 { x: w, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    reject_pipeline(src, "vt", "fr", "fragment");
}

#[test]
fn reject_dfdy_in_vertex() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            let w = dfdy(a.aPos.x)
            return Vary { position: Vec4 { x: w, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 { return vary.position }
    "#;
    reject_pipeline(src, "vt", "fr", "fragment");
}

#[test]
fn mrt_early_return_terminates_the_entry() {
    let source = MRT_SRC.replace(
        "let c = texture(uTexture, vary.TexCoord)",
        "if vary.EntityId > 0.0 { return SpriteOut { FragColor: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 }, FragId: Vec4 { x: 1.0, y: 0.0, z: 0.0, w: 1.0 } } }\nlet c = texture(uTexture, vary.TexCoord)",
    );
    let glsl = emit_pipeline(&source, "vt", "fr").expect("emit");
    let branch = glsl.split("if (").last().expect("conditional entry");
    let end = branch.find('}').expect("branch end");
    assert!(branch[..end].contains("return;"), "MRT branch must not fall through:\n{glsl}");
}

// --- per-stage sampler budget ---------------------------------------------
// GL 4.1 core (macOS) and GLES 3.0 guarantee 16 texture image units per
// stage. Linux drivers allow more, so a shader over the budget linked there
// and failed only on a Mac.

fn sampler_group_src(n: usize) -> String {
    let fields: Vec<String> = (0..n).map(|i| format!("s{i}: Sampler2D")).collect();
    let reads: Vec<String> = (0..n).map(|i| format!("acc += texture(t.s{i}, uv).x")).collect();
    format!(
        r#"
        struct ManyTex {{ {} }}
        value Quad {{ aPos: Vec2, aUv: Vec2 }}
        value Vary {{ position: Vec4, uv: Vec2 }}
        @shader(vertex)
        fn many_vs(@attr a: Quad) -> Vary {{ return Vary {{ position: v(a.aPos, 0.0, 1.0), uv: a.aUv }} }}
        @shader(fragment)
        fn many_fs(vary: Vary, @textures t: ManyTex) -> Vec4 {{
            let uv = vary.uv
            let acc = 0.0
            {}
            return Vec4 {{ x: acc, y: 0.0, z: 0.0, w: 1.0 }}
        }}
        @shader(fragment)
        fn many_entry(@textures t: ManyTex, uv: Vec2) -> Vec4 {{
            let acc = 0.0
            {}
            return Vec4 {{ x: acc, y: 0.0, z: 0.0, w: 1.0 }}
        }}
    "#,
        fields.join(", "),
        reads.join("\n            "),
        reads.join("\n            ")
    )
}

#[test]
fn fragment_stage_at_sixteen_samplers_emits() {
    let src = sampler_group_src(16);
    emit_pipeline(&src, "many_vs", "many_fs").expect("16 samplers fit every GLSL target");
    emit_entry(&src, "many_entry").expect("16 samplers fit every GLSL target");
}

#[test]
fn fragment_stage_over_sixteen_samplers_rejects_naming_the_shader() {
    let src = sampler_group_src(17);
    let e = emit_pipeline(&src, "many_vs", "many_fs").expect_err("17 samplers must reject");
    assert!(e.contains("many_fs") && e.contains("17 samplers") && e.contains("16"), "got: {e}");
    let e = emit_entry(&src, "many_entry").expect_err("17 samplers must reject");
    assert!(e.contains("many_entry") && e.contains("17 samplers") && e.contains("16"), "got: {e}");
}
