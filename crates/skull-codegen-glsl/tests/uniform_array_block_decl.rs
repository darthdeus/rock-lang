//! Uniform arrays use a per-stage std140 block, with declarations and layout
//! derived from the same plan.

use crate::support::{assert_has, assert_lacks, pipeline_emission};
use skull_codegen_glsl::ShaderEmission;
use skull_mir::gpu_layout::UniformArrayBlockPlan;

/// The one block at `binding`, or a panic naming what was actually there.
fn emit(src: &str, vert: &str, frag: &str) -> ShaderEmission {
    pipeline_emission(src, vert, frag).expect("emit")
}

fn emit_err(src: &str, vert: &str, frag: &str) -> String {
    pipeline_emission(src, vert, frag)
        .map(|_| ())
        .expect_err("expected a rejection")
}

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

/// The vertex stage's arrays: one block, at the vertex array binding, laid out
/// the way the host packs it.
#[test]
fn a_stages_arrays_are_declared_as_one_std140_block() {
    let e = emit(VERTEX_ARRAYS, "vt", "fr");
    let src = &e.source;
    assert_has(src, "layout(std140) uniform vt_Arrays {");
    assert_has(src, "    vec2 entity_positions[32];");
    assert_has(src, "    float entity_influences[32];");
    // A loose array and a block member of the same name would both compile, so
    // the block's presence does not imply the loose form is gone.
    assert_lacks(src, "uniform vec2 entity_positions[32];");
    assert_lacks(src, "uniform float entity_influences[32];");
    // The block has NO instance name, so bodies keep reading bare globals.
    assert_has(src, "entity_positions[");
    assert_lacks(src, "at(");

    assert_eq!(skull_codegen_glsl::UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS, 3);
    let p = block_at(&e, 3);
    assert_eq!(p.block_name.as_str(), "vt_Arrays");
    // A binding that means different things depending on what a program
    // declares is not enforceable, so arrays never share a scalar binding.
    let scalar: Vec<u32> = e.abi.uniform_blocks.iter().map(|(b, _)| *b).collect();
    assert!(!scalar.contains(&3), "{scalar:?}");
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
    // Emitted as a plain numeric literal, which LuaJIT and JS both represent
    // exactly only up to 53 bits.
    assert!(p.fingerprint() < (1u64 << 53));
    assert_ne!(p.fingerprint(), 0);
}

#[test]
fn a_fragment_stages_arrays_bind_at_its_own_slot_alone() {
    let e = emit(FRAGMENT_ARRAYS, "vt", "fr");
    assert_eq!(skull_codegen_glsl::UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS, 4);
    assert_eq!(block_at(&e, 4).block_name.as_str(), "fr_Arrays");
    // The vertex stage has no arrays, so it contributes no block rather than
    // a zero-member one.
    assert_eq!(e.abi.uniform_array_blocks.len(), 1);
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

/// Every element kind the array lane admits, in one block: the declaration's
/// spelling, the std140 offsets and stride, and the wire codes the host's
/// `wire_kind_components` table reads (float 0, Vec2 1, Vec4 3, int 4) all come
/// from one plan and must agree member by member.
#[test]
fn every_admitted_element_kind_declares_and_lays_out_consistently() {
    let src = r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(
            vary: Vary,
            @size(3) fs: UniformArray<float>,
            @size(5) is: UniformArray<int>,
            @size(2) v2s: UniformArray<Vec2>,
            @size(4) v4s: UniformArray<Vec4>,
        ) -> Vec4 {
            let f = at(fs, 0)
            let i = at(is, 0)
            let p = at(v2s, 0)
            let c = at(v4s, 0)
            return Vec4 { x: f + p.x + (i as float), y: c.y, z: 0.0, w: 1.0 }
        }
    "#;
    let e = emit(src, "vt", "fr");
    let p = block_at(&e, 4);
    assert_eq!(
        layout(p),
        vec![
            ("fs", 3, 0, 16),
            ("is", 5, 48, 16),
            ("v2s", 2, 128, 16),
            ("v4s", 4, 160, 16)
        ]
    );
    assert_eq!(p.size, 224);
    let codes: Vec<u32> = p.members.iter().map(|m| m.elem.code()).collect();
    assert_eq!(codes, vec![0, 4, 1, 3]);
    for line in [
        "    float fs[3];",
        "    int is[5];",
        "    vec2 v2s[2];",
        "    vec4 v4s[4];",
    ] {
        assert_has(&e.source, line);
    }
}

/// Emission rejects `src` with a diagnostic naming every needle.
fn reject(src: &str, needles: &[&str]) {
    let msg = emit_err(src, "vt", "fr");
    for needle in needles {
        assert!(msg.contains(needle), "missing `{needle}` in: {msg}");
    }
}

cases! { reject:
    /// `uint` is a scalar block kind, but an array of it has no host push, so
    /// the emitter refuses it at the `@size` param rather than declaring a block
    /// the runtime can never fill.
    a_uint_array_is_rejected_at_emission(r#"
        value QV { aPos: Vec2 }
        value Vary { position: Vec4 }
        @shader(vertex)
        fn vt(@attr a: QV) -> Vary {
            return Vary { position: Vec4 { x: a.aPos.x, y: 0.0, z: 0.0, w: 1.0 } }
        }
        @shader(fragment)
        fn fr(vary: Vary, @size(4) masks: UniformArray<uint>) -> Vec4 {
            let m = at(masks, 0)
            return Vec4 { x: (m as int) as float, y: 0.0, z: 0.0, w: 1.0 }
        }
    "#, &["`uint`", "masks"]);

    /// 16 KB is the ES 3.0 guaranteed MAX_UNIFORM_BLOCK_SIZE. A desktop driver
    /// offers far more, so nothing downstream would catch this.
    an_oversized_array_block_is_rejected_at_the_size_annotation(r#"
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
    "#, &["16384", "big"]);

    a_zero_capacity_array_is_rejected(r#"
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
    "#, &["capacity 0"]);

    /// The block name is MINTED from the entry, so a user struct that happens
    /// to spell it must reject rather than produce two blocks with one name.
    an_array_block_name_cannot_collide_with_a_uniform_group(r#"
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
    "#, &["vt_Arrays"]);
}
