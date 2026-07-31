//! `@uniform_expand` -- the std140 packer (docs/shader-binding-layout-plan.md
//! P2). The expansion emits one DIRECT typed store per primitive leaf, at the
//! offsets `gpu_layout::uniform_block` computed, into views passed as params.
//!
//! What these pin that a layout unit test cannot: that the offsets reach the
//! EMITTED code as literal indices, that a store is a store rather than a call,
//! and that the byte offset became an ELEMENT index. `BloomBlurU` is the
//! fixture throughout because its std140 layout diverges from its C layout, so
//! the divergence is visible as a skipped index rather than as a number that
//! happens to agree.

mod common;
use common::emit_module;

/// `radius: float, direction: Vec2, resolution: float` -> std140 offsets
/// 0, 8, 12, 16 -> element indices 0, 2, 3, 4. Index 1 is the four-byte hole
/// std140 opens after the lone leading float, and its ABSENCE is the assertion
/// that matters: the C layout would have packed `direction` at index 1.
const BLOOM: &str = r#"
value BloomBlurU { radius: float, direction: Vec2, resolution: float }

@uniform_expand inline(lua) fn pack_uniform_block<T>(fv: Ptr<float>, iv: Ptr<int>, u: T) = """
    error("pack_uniform_block is compiler-expanded")
"""

fn main() {
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    let u = BloomBlurU { radius: 1.0, direction: v(2.0, 3.0), resolution: 4.0 }
    pack_uniform_block(f.ptr, i.ptr, u)
}
"#;

/// The emitted body of `main`, which is where the expansion lands. The
/// intrinsic's own dead Lua definition is still emitted (nothing calls it), so
/// module-wide substring checks would match it and prove nothing.
fn main_body(lua: &str) -> String {
    let at = lua.find("\nfunction main()").expect("main is emitted");
    let rest = &lua[at + 1..];
    let end = rest.find("\nend").expect("main has an end");
    rest[..end].to_string()
}

/// The store indices are the std140 offsets divided by four, as literals.
#[test]
fn bloom_blur_packs_at_its_std140_element_indices() {
    let body = main_body(&emit_module(BLOOM));
    for idx in ["[(0)] = ", "[(2)] = ", "[(3)] = ", "[(4)] = "] {
        assert!(body.contains(idx), "missing store {idx} in:\n{body}");
    }
    assert!(
        !body.contains("[(1)] = "),
        "index 1 is std140's hole after `radius`; a store there means the C \
         layout leaked into the pack:\n{body}"
    );
}

/// `WireView` must reach the EMITTED code as a choice of base, not just as a
/// value in the plan: a 32-bit block `bool` and an `int` store through the int
/// view while the float beside them stores through the float view. Each view is
/// a distinct `ffi.new`, so the two are told apart by which local they index.
#[test]
fn int_and_bool_leaves_store_through_the_other_view() {
    let body = main_body(&emit_module(
        r#"
value Flags { scale: float, on: bool, count: int }

@uniform_expand inline(lua) fn pack_uniform_block<T>(fv: Ptr<float>, iv: Ptr<int>, u: T) = """
    error("pack_uniform_block is compiler-expanded")
"""

fn main() {
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    let u = Flags { scale: 1.5, on: true, count: 7 }
    pack_uniform_block(f.ptr, i.ptr, u)
}
"#,
    ));
    let fbuf = body
        .lines()
        .find(|l| l.contains(r#"ffi.new("float"#))
        .and_then(|l| l.split_whitespace().next())
        .expect("the float view's local");
    let ibuf = body
        .lines()
        .find(|l| l.contains(r#"ffi.new("int"#))
        .and_then(|l| l.split_whitespace().next())
        .expect("the int view's local");
    assert_ne!(fbuf, ibuf, "the fixture's two views must be distinct locals");
    assert!(
        body.contains(&format!("{fbuf}[(0)] = (1.5)")),
        "the float stores through the float view:\n{body}"
    );
    for store in [format!("{ibuf}[(1)] = "), format!("{ibuf}[(2)] = ")] {
        assert!(body.contains(&store), "missing int-view store `{store}`:\n{body}");
    }
}

/// Each leaf goes straight from a register into the view. Two ways that could
/// regress: the call survives unexpanded, or the group gets PACKED into its
/// leaf-table ctor first (which is what the same call emits without the
/// expansion, and which allocates per pack).
#[test]
fn the_pack_is_stores_not_a_call_and_never_boxes() {
    let body = main_body(&emit_module(BLOOM));
    assert!(
        !body.contains("pack_uniform_block"),
        "a call to the intrinsic survived rather than expanding:\n{body}"
    );
    assert!(
        !body.contains("BloomBlurU_lct") && !body.contains("BloomBlurU_ct"),
        "the group was boxed before packing -- the leaves must reach the store \
         unpacked:\n{body}"
    );
}
