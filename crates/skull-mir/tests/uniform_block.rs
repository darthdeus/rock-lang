//! std140 layout for an `@uniform` binding group -- the uniform half of the
//! interface artifact (docs/shader-binding-layout-plan.md P2).
//!
//! Every expected offset here is a LITERAL, read off the std140 rules by hand.
//! None may be computed from the layout function itself: a test that asks the
//! algorithm what it thinks and then agrees with it proves only that the
//! algorithm is deterministic.
//!
//! The rules being pinned (OpenGL 4.5 core, 7.6.2.2), for the shapes a uniform
//! group can hold today:
//!
//! | type  | align | size | note |
//! |-------|-------|------|------|
//! | float/int/bool | 4 | 4 | a block bool is 32-bit ON THE WIRE; Rock's C bool is 1 byte |
//! | Vec2  | 8  | 8  | |
//! | Vec3  | 16 | 12 | alignment 16, size 12 -- a following scalar takes the fourth lane |
//! | Vec4  | 16 | 16 | |
//! | Mat4  | 16 | 64 | four column vectors, matrix stride 16 |
//!
//! and the block itself rounds to a multiple of 16.
//!
//! This is NOT the struct's C layout, and the difference is the point: three
//! live in-tree structs (BloomBlurU, HeatShimmerU, SpriteFragU) disagree with
//! their C layout, so each has a case below carrying both numbers.

use std::sync::LazyLock;
use ustr::Ustr;

use skull_mir::gpu_layout::{uniform_block, LeafStore, UniformBlockPlan, WireKind, WireView};

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn ws_of(src: &str) -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errs = skull_mir::verdict::body_errors(&ws);
    assert!(errs.is_empty(), "fixture must type-check: {errs:?}");
    ws
}

fn plan_of(src: &str, struct_name: &str) -> UniformBlockPlan {
    let ws = ws_of(src);
    uniform_block(&ws, Ustr::from(struct_name)).expect("block layout must compute")
}

/// `(name, offset)` per member, the shape most cases assert.
fn offsets(p: &UniformBlockPlan) -> Vec<(&str, u32)> {
    p.members.iter().map(|m| (m.name.as_str(), m.offset)).collect()
}

// -- The three in-tree structs whose std140 layout is NOT their C layout.
// Each names the C offsets it diverges from, because that divergence is what
// makes a wrong packing produce wrong pixels rather than coincidentally-right
// ones.

/// bloom_blur_shader.rock. C layout puts direction at 4 and resolution at 12;
/// std140's Vec2 alignment of 8 moves both.
#[test]
fn bloom_blur_diverges_from_its_c_layout() {
    let p = plan_of(
        "value BloomBlurU { radius: float, direction: Vec2, resolution: float }",
        "BloomBlurU",
    );
    assert_eq!(offsets(&p), vec![("radius", 0), ("direction", 8), ("resolution", 16)]);
    // C layout is [0, 4, 12] with size 16 -- every field after the first moves.
    assert_eq!(p.size, 32);
}

/// heat_shimmer_shader.rock, the same generator one field longer.
#[test]
fn heat_shimmer_diverges_from_its_c_layout() {
    let p = plan_of(
        "value HeatShimmerU { uTime: float, uCenter: Vec2, uRadius: float, uStrength: float }",
        "HeatShimmerU",
    );
    assert_eq!(
        offsets(&p),
        vec![("uTime", 0), ("uCenter", 8), ("uRadius", 16), ("uStrength", 20)]
    );
    // C layout is [0, 4, 12, 16] size 20.
    assert_eq!(p.size, 32);
}

/// sprite_batch_shader.rock. The divergence generator here is `bool`, which no
/// amount of padding can reconcile: Rock's C layout models it as ONE byte and
/// GL reads a block bool as a 32-bit value.
#[test]
fn sprite_frag_bools_are_four_bytes_on_the_wire() {
    let p = plan_of(
        "value SpriteFragU { uColor: Vec4, uLightsEnabled: bool, uUnlit: bool, uLightCount: int, \
         uAmbientColor: Vec4, uGlowEnabled: bool, uHasNormalTex: bool, uNormalLightParams: Vec2 }",
        "SpriteFragU",
    );
    assert_eq!(
        offsets(&p),
        vec![
            ("uColor", 0),
            ("uLightsEnabled", 16),
            ("uUnlit", 20),
            ("uLightCount", 24),
            ("uAmbientColor", 32),
            ("uGlowEnabled", 48),
            ("uHasNormalTex", 52),
            ("uNormalLightParams", 56),
        ]
    );
    // C layout is [0, 16, 17, 20, 24, 40, 41, 44] size 52: the two bool runs
    // pack one-per-byte and drag everything after them.
    assert_eq!(p.size, 64);

    // The wire kind is what carries the 1-byte-to-32-bit conversion. A packer
    // that wrote a Rock bool's byte here would leave three bytes of garbage
    // that GL reads as part of the value.
    assert_eq!(p.members[1].kind, WireKind::Bool32);
}

// -- Rules that no in-tree struct exercises yet, pinned before a consumer
// depends on them.

/// Vec3 has alignment 16 but size TWELVE, so a following scalar occupies the
/// fourth lane rather than starting a new one. Getting this wrong is a silent
/// 4-byte shift of everything downstream.
#[test]
fn a_scalar_after_vec3_takes_the_fourth_lane() {
    let p = plan_of("value V { dir: Vec3, gain: float }", "V");
    assert_eq!(offsets(&p), vec![("dir", 0), ("gain", 12)]);
    assert_eq!(p.size, 16);
}

/// A Mat4 is four column vectors: alignment 16, size 64, matrix stride 16.
#[test]
fn mat4_runs_are_sixty_four_apart() {
    let p = plan_of(
        "value UiVertU { uProjection: Mat4, uView: Mat4, uTransform: Mat4 }",
        "UiVertU",
    );
    assert_eq!(offsets(&p), vec![("uProjection", 0), ("uView", 64), ("uTransform", 128)]);
    assert_eq!(p.size, 192);
    assert_eq!(p.members[0].matrix_stride, Some(16));
}

#[test]
fn a_vec2_after_a_mat4_aligns_to_eight_not_sixteen() {
    let p = plan_of("value V { m: Mat4, bounds: Vec2 }", "V");
    assert_eq!(offsets(&p), vec![("m", 0), ("bounds", 64)]);
    // 64 + 8 = 72, rounded to the block's 16.
    assert_eq!(p.size, 80);
}

/// The block rounds to 16 even when it holds one float -- a four-byte upload
/// into a sixteen-byte block is the shape a size assertion has to expect.
#[test]
fn a_scalar_only_block_still_rounds_to_sixteen() {
    let p = plan_of("value VignetteU { strength: float }", "VignetteU");
    assert_eq!(offsets(&p), vec![("strength", 0)]);
    assert_eq!(p.size, 16);
    assert_eq!(p.align, 16);
}

/// A struct whose C layout happens to coincide. Worth a case of its own: it is
/// the shape that would make a C-layout-based packer look correct, and the one
/// that hides the bug the divergent cases above expose.
#[test]
fn ui_sdf_bar_coincides_with_its_c_layout_in_offsets_but_not_size() {
    let p = plan_of(
        "value UiSdfBarU { uSize: Vec2, uRatio: float, uRadius: float, uFill: Vec4, uBg: Vec4, \
         uTime: float }",
        "UiSdfBarU",
    );
    assert_eq!(
        offsets(&p),
        vec![
            ("uSize", 0),
            ("uRatio", 8),
            ("uRadius", 12),
            ("uFill", 16),
            ("uBg", 32),
            ("uTime", 48),
        ]
    );
    // Identical to the C offsets -- but C size is 52 and the block is 64.
    assert_eq!(p.size, 64);
}

// -- Rejections. Every one of these is a shape the packer has no defined wire
// form for; accepting it would mean uploading bytes GL reads as something else.

#[test]
fn a_plain_struct_is_not_a_binding_group() {
    let ws = ws_of("struct NotValue { a: float }");
    let e = uniform_block(&ws, Ustr::from("NotValue")).expect_err("must reject");
    assert!(e.message.contains("value"), "got {}", e.message);
}

#[test]
fn an_empty_group_binds_nothing() {
    let ws = ws_of("value Empty {}");
    let e = uniform_block(&ws, Ustr::from("Empty")).expect_err("must reject");
    // Not a bare is_err(): a lookup failure would satisfy that too, and this
    // case is about the struct being FOUND and having nothing in it.
    assert!(e.message.contains("no fields"), "got {}", e.message);
}

/// A nested user struct has a std140 form (align rounded to 16, trailing
/// extent rounded) that nothing consumes yet. Rejecting is the honest answer
/// until a consumer needs it -- the alternative is inventing a layout no test
/// pins and no shader reads.
#[test]
fn a_nested_user_struct_is_rejected_until_a_consumer_needs_it() {
    let ws = ws_of("value Inner { a: float }\nvalue Outer { i: Inner, b: float }");
    let e = uniform_block(&ws, Ustr::from("Outer")).expect_err("must reject");
    assert!(e.message.contains("Inner"), "the message must name the field's type: {}", e.message);
}

// -- Leaf stores. The plan states member offsets; the packer emits one store
// per PRIMITIVE, because the value side arrives as a tree of leaves (a Mat4 is
// four Vec4s is sixteen floats). Deriving that expansion at each consumer is
// how the validator and the expander drift apart, so the plan states it once.

fn stores(p: &UniformBlockPlan) -> Vec<(u32, WireView)> {
    p.leaf_stores().iter().map(|s| (s.offset, s.view)).collect()
}

/// The dominant A1 shape -- six in-tree uniform structs carry two or three
/// Mat4s. std140's column stride of 16 equals four tightly packed floats, and
/// stdlib Mat4 is already four column-major Vec4s, so each run is contiguous:
/// no transpose, no per-column gap. Two of them, because a single Mat4 sits at
/// offset 0 and would read the same whether the member offset were applied or
/// dropped.
#[test]
fn consecutive_mat4s_expand_to_contiguous_sixteen_float_runs() {
    let p = plan_of("value M { a: Mat4, b: Mat4 }", "M");
    let want: Vec<(u32, WireView)> = (0..32).map(|i| (i * 4, WireView::Float)).collect();
    assert_eq!(stores(&p), want);
    assert_eq!(p.members[1].leaf_stores()[0].offset, 64);
}

/// The view is a property of the wire kind, not of the packer: a block bool is
/// a 32-bit value, so it stores through the int view alongside int, while
/// everything else goes through the float view.
#[test]
fn int_and_bool_leaves_store_through_the_int_view() {
    let p = plan_of("value B { flag: bool, count: int, scale: float }", "B");
    assert_eq!(
        stores(&p),
        vec![(0, WireView::Int), (4, WireView::Int), (8, WireView::Float)]
    );
}

/// The std140 gap is visible at LEAF level, which is the level the packer
/// writes at: `direction`'s two floats sit at 8 and 12, where the C layout
/// would have put them at 4 and 8.
#[test]
fn bloom_blurs_leaf_stores_carry_the_std140_gap() {
    let p = plan_of(
        "value BloomBlurU { radius: float, direction: Vec2, resolution: float }",
        "BloomBlurU",
    );
    assert_eq!(
        stores(&p),
        vec![
            (0, WireView::Float),
            (8, WireView::Float),
            (12, WireView::Float),
            (16, WireView::Float)
        ]
    );
    // One store per primitive leaf: the count the validator checks the value
    // side against.
    assert_eq!(p.leaf_stores().len(), 4);
    assert_eq!(p.members[1].leaf_stores(), vec![
        LeafStore { offset: 8, view: WireView::Float },
        LeafStore { offset: 12, view: WireView::Float },
    ]);
}
