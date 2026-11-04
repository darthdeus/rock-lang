//! **A stage's `@size(N)` params become ONE std140 array block** at a reserved
//! binding, instead of loose `uniform T name[N];` declarations.
//!
//! Loose is the out-of-spec arrangement, which is the whole argument for the
//! port: loose uniforms live in the DEFAULT block, whose ES 3.0 guarantee is 224
//! vec4, while a named block gets 16 KB. `outline_detect`'s three arrays are 768
//! vec4 loose -- 3.4x over -- and 75% of a budget they fit inside a block.
//!
//! The declaration reads `ArrayMember.elem` out of the PLAN. It deliberately
//! does not spell the type from the emitter's own `GlslTy` classification of the
//! same param: two classifiers for one question is how the layout and the
//! declaration drift into disagreeing about what an array holds, and "one layout
//! authority" would be false the moment the second one exists.

use skull_codegen_glsl::{GlslProfile, ShaderEmission};
use skull_mir::gpu_layout::UniformArrayBlockPlan;
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

fn workspace(
    src: &str,
) -> (
    bone_semantic::inference::Workspace,
    bone_semantic::prelude::ModuleId,
) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    (ws, mid)
}

fn emit(src: &str, vert: &str, frag: &str) -> ShaderEmission {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        vert,
        frag,
        GlslProfile::Core330,
    )
    .expect("emit")
}

fn emit_err(src: &str, vert: &str, frag: &str) -> String {
    let (ws, mid) = workspace(src);
    skull_codegen_glsl::emit_shader_pipeline_with_dependencies(
        &ws,
        mid,
        vert,
        frag,
        GlslProfile::Core330,
    )
    .map(|_| ())
    .expect_err("expected a rejection")
    .message
}

/// The one block at `binding`, or a panic naming what was actually there.
fn block_at(e: &ShaderEmission, binding: u32) -> &UniformArrayBlockPlan {
    let found: Vec<u32> = e.abi.uniform_array_blocks.iter().map(|(b, _)| *b).collect();
    e.abi
        .uniform_array_blocks
        .iter()
        .find(|(b, _)| *b == binding)
        .map(|(_, p)| p)
        .unwrap_or_else(|| {
            panic!("no array block at binding {binding}; bindings present: {found:?}")
        })
}

/// `(name, capacity, offset, stride)` per member, declaration order.
fn layout(p: &UniformArrayBlockPlan) -> Vec<(&str, u32, u32, u32)> {
    p.members
        .iter()
        .map(|m| (m.name.as_str(), m.capacity, m.offset, m.stride))
        .collect()
}

/// An instanced grass fixture: two vertex-stage arrays of different element kinds.
/// A `Vec2` and a `float` member, deliberately -- a `Vec4`-only fixture cannot
/// detect a stride bug, because `Vec4`'s base alignment is already 16 and the
/// round-to-16 is then a no-op (backlog 21).
const VERTEX_ARRAYS: &str = r#"
    value QV { aPos: Vec2 }
    value VU { entity_count: int }
    value Vary { position: Vec4 }
    @shader(vertex)
    fn vt(
        @attr a: QV,
        @uniform u: VU,
        @size(32) entity_positions: UniformArray<Vec2>,
        @size(32) entity_influences: UniformArray<float>,
    ) -> Vary {
        let p = at(entity_positions, 0)
        let w = at(entity_influences, 0)
        return Vary { position: Vec4 { x: a.aPos.x + p.x + w, y: 0.0, z: 0.0, w: 1.0 } }
    }
    @shader(fragment)
    fn fr(vary: Vary) -> Vec4 {
        return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
    }
"#;

/// The sprite-lights shape: the arrays sit in the FRAGMENT stage.
const FRAGMENT_ARRAYS: &str = r#"
    value QV { aPos: Vec2 }
    value Vary { position: Vec4 }
    @shader(vertex)
    fn vt(@attr a: QV) -> Vary {
        return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
    }
    @shader(fragment)
    fn fr(
        vary: Vary,
        @size(16) uLightTypes: UniformArray<int>,
        @size(16) uLightColors: UniformArray<Vec4>,
    ) -> Vec4 {
        let t = at(uLightTypes, 0)
        let c = at(uLightColors, 0)
        if t > 0 {
            return c
        }
        return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
    }
"#;

// --- the declaration ------------------------------------------------------

#[test]
fn a_stages_arrays_are_declared_as_one_std140_block() {
    let e = emit(VERTEX_ARRAYS, "vt", "fr");
    let src = &e.source;
    assert!(
        src.contains("layout(std140) uniform vt_Arrays {"),
        "expected a std140 array block for the vertex stage:\n{src}"
    );
    assert!(
        src.contains("    vec2 entity_positions[32];"),
        "got:\n{src}"
    );
    assert!(
        src.contains("    float entity_influences[32];"),
        "got:\n{src}"
    );
}

#[test]
fn the_loose_declaration_is_gone() {
    // The point of the port. A loose array and a block member of the same name
    // would both compile, so asserting the block exists does not imply this.
    let src = emit(VERTEX_ARRAYS, "vt", "fr").source;
    assert!(
        !src.contains("uniform vec2 entity_positions[32];"),
        "the loose form must not survive beside the block:\n{src}"
    );
    assert!(
        !src.contains("uniform float entity_influences[32];"),
        "the loose form must not survive beside the block:\n{src}"
    );
}

#[test]
fn bodies_still_read_the_arrays_as_bare_globals() {
    // The block is declared with NO instance name, so its members stay bare
    // globals and every body reading them is byte-identical to the loose form.
    // That is what confines this change to declaration lines.
    let src = emit(VERTEX_ARRAYS, "vt", "fr").source;
    assert!(
        src.contains("entity_positions["),
        "at() must render as indexing:\n{src}"
    );
    assert!(
        !src.contains("at("),
        "at() must not render as a call:\n{src}"
    );
}

// --- the reserved bindings ------------------------------------------------

#[test]
fn the_vertex_array_block_binds_at_its_own_reserved_slot() {
    let e = emit(VERTEX_ARRAYS, "vt", "fr");
    assert_eq!(skull_codegen_glsl::UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS, 3);
    let p = block_at(&e, 3);
    assert_eq!(p.block_name.as_str(), "vt_Arrays");
}

#[test]
fn the_fragment_array_block_binds_at_its_own_reserved_slot() {
    let e = emit(FRAGMENT_ARRAYS, "vt", "fr");
    assert_eq!(skull_codegen_glsl::UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS, 4);
    let p = block_at(&e, 4);
    assert_eq!(p.block_name.as_str(), "fr_Arrays");
}

#[test]
fn an_array_block_never_lands_on_a_scalar_blocks_binding() {
    // The reason slot 0 was reserved for `@engine` in the first place: a binding
    // that means different things depending on what a program happens to declare
    // is not enforceable. Same rule one lane over.
    let e = emit(VERTEX_ARRAYS, "vt", "fr");
    let scalar: Vec<u32> = e.abi.uniform_blocks.iter().map(|(b, _)| *b).collect();
    let arrays: Vec<u32> = e.abi.uniform_array_blocks.iter().map(|(b, _)| *b).collect();
    for b in &arrays {
        assert!(
            !scalar.contains(b),
            "array binding {b} collides with a scalar block: {scalar:?}"
        );
    }
}

#[test]
fn both_stages_may_carry_arrays_and_get_distinct_blocks() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @size(8) vpos: UniformArray<Vec2>) -> Vary {
            let p = at(vpos, 0)
            return Vary { position: Vec4 { x: a.aPos.x + p.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary, @size(8) fcol: UniformArray<Vec4>) -> Vec4 {
            return at(fcol, 0)
        }
    "#;
    let e = emit(src, "vt", "fr");
    assert_eq!(block_at(&e, 3).block_name.as_str(), "vt_Arrays");
    assert_eq!(block_at(&e, 4).block_name.as_str(), "fr_Arrays");
    assert_eq!(e.abi.uniform_array_blocks.len(), 2);
}

#[test]
fn a_stage_with_no_arrays_declares_no_array_block() {
    // An empty block binds nothing, so the absence must be an absence rather
    // than a zero-member block -- the same rule `uniform_array_block` enforces.
    let e = emit(FRAGMENT_ARRAYS, "vt", "fr");
    assert!(
        e.abi.uniform_array_blocks.iter().all(|(b, _)| *b == 4),
        "the vertex stage has no arrays and must contribute no block: {:?}",
        e.abi
            .uniform_array_blocks
            .iter()
            .map(|(b, _)| *b)
            .collect::<Vec<_>>()
    );
}

// --- the layout the host will pack ---------------------------------------

#[test]
fn members_carry_the_std140_layout_the_host_packs_against() {
    let e = emit(VERTEX_ARRAYS, "vt", "fr");
    let p = block_at(&e, 3);
    // Every array stride is 16 whatever the element -- a `float[32]` costs 512
    // bytes to carry 128. Accepted (backlog 18); visible here rather than buried.
    assert_eq!(
        layout(p),
        vec![
            ("entity_positions", 32, 0, 16),
            ("entity_influences", 32, 512, 16)
        ]
    );
    assert_eq!(p.size, 1024);
}

#[test]
fn the_array_blocks_fingerprint_travels_with_it() {
    let e = emit(VERTEX_ARRAYS, "vt", "fr");
    let p = block_at(&e, 3);
    // Folded to 53 bits: it is emitted as a plain numeric literal and LuaJIT and
    // JS both represent integers exactly only that far.
    assert!(
        p.fingerprint() < (1u64 << 53),
        "fingerprint must fit 53 bits"
    );
    assert_ne!(p.fingerprint(), 0);
}

// --- rejections, spanned at the offending param ---------------------------

#[test]
fn an_oversized_array_block_is_rejected_at_the_size_annotation() {
    // 16 KB is the ES 3.0 guaranteed MAX_UNIFORM_BLOCK_SIZE. A desktop driver
    // offers far more, so nothing downstream would catch this -- `shader-validate`
    // links against the host driver's own limit.
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @size(1025) big: UniformArray<Vec4>) -> Vary {
            let p = at(big, 0)
            return Vary { position: Vec4 { x: a.aPos.x + p.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let msg = emit_err(src, "vt", "fr");
    assert!(msg.contains("16384"), "the budget must be named: {msg}");
    assert!(
        msg.contains("big"),
        "the offending member must be named: {msg}"
    );
}

#[test]
fn a_zero_capacity_array_is_rejected() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @size(0) none: UniformArray<Vec4>) -> Vary {
            let p = at(none, 0)
            return Vary { position: Vec4 { x: a.aPos.x + p.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let msg = emit_err(src, "vt", "fr");
    assert!(msg.contains("capacity 0"), "got: {msg}");
}

#[test]
fn an_array_block_name_cannot_collide_with_a_uniform_group() {
    // The block name is MINTED from the entry, so a user struct that happens to
    // spell it must reject rather than produce two blocks with one name.
    let src = r#"
        value QV { aPos: Vec2 }
        value vt_Arrays { entity_count: int }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @uniform u: vt_Arrays, @size(8) vpos: UniformArray<Vec2>) -> Vary {
            let p = at(vpos, 0)
            return Vary { position: Vec4 { x: a.aPos.x + p.x + (u.entity_count as float), y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let msg = emit_err(src, "vt", "fr");
    assert!(msg.contains("vt_Arrays"), "got: {msg}");
}

// --- the one-plan rule ----------------------------------------------------

#[test]
fn the_published_check_and_emission_agree_about_array_blocks() {
    // `check_pipeline_profile` must not admit what emission refuses. It is
    // emission's own preparation, so an array-block rejection has to reach it --
    // a check that re-ran a summary of these steps would miss the layout entirely.
    for src in [VERTEX_ARRAYS, FRAGMENT_ARRAYS] {
        let (ws, mid) = workspace(src);
        let v = skull_codegen_glsl::resolve_root(&ws, mid, "vt").expect("vert root");
        let f = skull_codegen_glsl::resolve_root(&ws, mid, "fr").expect("frag root");
        let checked = skull_codegen_glsl::check_pipeline_profile(&ws, v, f).map_err(|e| e.message);
        let emitted =
            skull_codegen_glsl::emit_shader_pipeline(&ws, mid, "vt", "fr", GlslProfile::Core330)
                .map(|_| ())
                .map_err(|e| e.message);
        assert_eq!(checked, emitted);
    }
}

#[test]
fn the_check_rejects_an_oversized_block_exactly_as_emission_does() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV, @size(1025) big: UniformArray<Vec4>) -> Vary {
            let p = at(big, 0)
            return Vary { position: Vec4 { x: a.aPos.x + p.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary) -> Vec4 {
            return Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#;
    let (ws, mid) = workspace(src);
    let v = skull_codegen_glsl::resolve_root(&ws, mid, "vt").expect("vert root");
    let f = skull_codegen_glsl::resolve_root(&ws, mid, "fr").expect("frag root");
    let checked = skull_codegen_glsl::check_pipeline_profile(&ws, v, f).map_err(|e| e.message);
    let emitted =
        skull_codegen_glsl::emit_shader_pipeline(&ws, mid, "vt", "fr", GlslProfile::Core330)
            .map(|_| ())
            .map_err(|e| e.message);
    assert!(checked.is_err(), "the check must reject too");
    assert_eq!(checked, emitted);
}
