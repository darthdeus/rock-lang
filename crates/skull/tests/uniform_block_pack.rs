//! `@uniform_expand` -- the std140 packer. The expansion emits one DIRECT typed store per primitive leaf, at the
//! offsets `gpu_layout::uniform_block` computed, into views passed as params.
//!
//! What these pin that a layout unit test cannot: that the offsets reach the
//! EMITTED code as literal indices, that a store is a store rather than a call,
//! and that the byte offset became an ELEMENT index. `BloomBlurU` is the
//! fixture throughout because its std140 layout diverges from its C layout, so
//! the divergence is visible as a skipped index rather than as a number that
//! happens to agree.

use crate::common::{bone_rejections, emit_module, function_body, run_main};

const CANONICAL_VALUES: &str = r#"
value ProbeUniform {
    radius: float,
    direction: Vec2,
    enabled: bool,
    count: int,
    packed: uint,
}

fn main() {
    let b = uniform_values(ProbeUniform {
        radius: 1.0,
        direction: v(2.0, 3.0),
        enabled: true,
        count: 7,
        packed: 0xffffffff,
    })
    print(b.fingerprint != 0)
    print(b.leaves.len())
}
"#;

/// The submitted packet is a compiler-owned logical value: it has scalar
/// leaves and a semantic group fingerprint, but no std140 offsets or padding.
/// The first assertion is deliberately the RED gate for the new intrinsic;
/// the old compiler rejects its annotation before it can emit anything.
#[test]
fn uniform_values_expands_to_canonical_leaves() {
    let rejects = bone_rejections(CANONICAL_VALUES);
    assert!(
        rejects.is_empty(),
        "uniform_values was rejected: {rejects:#?}"
    );

    let lua = emit_module(CANONICAL_VALUES);
    let body = function_body(&lua, "main");
    assert!(
        !body.contains("uniform_values("),
        "the compiler-owned call survived:\n{body}"
    );
    for forbidden in ["std140", "offset", "padding", "binding"] {
        assert!(
            !body.contains(forbidden),
            "canonical values leaked {forbidden}:\n{body}"
        );
    }
    for variant in ["Float", "Bool", "Int", "Uint"] {
        assert!(
            body.contains(&format!("__variant = (\"{variant}\")")),
            "canonical `{variant}` leaf was not built:\n{body}"
        );
    }
    let out = run_main(&lua);
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["true", "6"]);
}

/// `radius: float, direction: Vec2, resolution: float` -> std140 offsets
/// 0, 8, 12, 16 -> element indices 0, 2, 3, 4. Index 1 is the four-byte hole
/// std140 opens after the lone leading float, and its ABSENCE is the assertion
/// that matters: the C layout would have packed `direction` at index 1.
const BLOOM: &str = r#"
value BloomBlurU { radius: float, direction: Vec2, resolution: float }

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
/// The store indices are the std140 offsets divided by four, as literals.
#[test]
fn bloom_blur_packs_at_its_std140_element_indices() {
    let lua = emit_module(BLOOM);
    let body = function_body(&lua, "main");
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
/// a distinct allocation, so the two are told apart by which local they index.
#[test]
fn int_and_bool_leaves_store_through_the_other_view() {
    let lua = emit_module(
        r#"
value Flags { scale: float, on: bool, count: int }

fn main() {
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    let u = Flags { scale: 1.5, on: true, count: 7 }
    pack_uniform_block(f.ptr, i.ptr, u)
}

"#,
    );
    let body = function_body(&lua, "main");
    let fbuf = body
        .lines()
        .find(|l| l.contains(r#"__rock_nb_alloc("float"#))
        .and_then(|l| l.split_whitespace().next())
        .expect("the float view's local");
    let ibuf = body
        .lines()
        .find(|l| l.contains(r#"__rock_nb_alloc("int"#))
        .and_then(|l| l.split_whitespace().next())
        .expect("the int view's local");
    assert_ne!(
        fbuf, ibuf,
        "the fixture's two views must be distinct locals"
    );
    assert!(
        body.contains(&format!("{fbuf}[(0)] = (1.5)")),
        "the float stores through the float view:\n{body}"
    );
    for store in [format!("{ibuf}[(1)] = "), format!("{ibuf}[(2)] = ")] {
        assert!(
            body.contains(&store),
            "missing int-view store `{store}`:\n{body}"
        );
    }
}

#[test]
fn uint_uniform_leaves_preserve_the_low_bits_in_the_int_view() {
    let src = r#"
value Packed { value: uint }

fn main() {
    let f = native_buffer_float(1)
    let i = native_buffer_int(1)
    pack_uniform_block(f.ptr, i.ptr, Packed { value: 0xffffffff })
    print(i[0])
}
"#;
    let out = run_main(&emit_module(src));
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["-1"]);
}

/// Each leaf goes straight from a register into the view. Two ways that could
/// regress: the call survives unexpanded, or the group gets PACKED into its
/// leaf-table ctor first (which is what the same call emits without the
/// expansion, and which allocates per pack).
#[test]
fn the_pack_is_stores_not_a_call_and_never_boxes() {
    let lua = emit_module(BLOOM);
    let body = function_body(&lua, "main");
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

/// A block packed out of a GLOBAL is read one leaf at a time, each leaf loaded
/// right before its own store. Lowering the whole value first loads every leaf
/// into a register before the first store, and in a trace that keeps all of
/// them live in every exit snapshot taken during the pack -- the engine block's
/// 41 leaves against LuaJIT's 16-register side-trace budget
///. Pinned by register count: the packing
/// function must declare fewer registers than the block has leaves.
const GLOBAL_SOURCE: &str = r#"
// `Mat4` is stored as a flat leaf table (keys like `c0.x`) while `Vec4`
// nests, so the two storage conventions are both read through here.
value Wide { a: Vec4, b: Vec4, c: Vec4, d: Vec4, m: Mat4 }

global BLOCK: Wide = Wide {
    a: v(1.0, 2.0, 3.0, 4.0), b: v(5.0, 6.0, 7.0, 8.0),
    c: v(9.0, 10.0, 11.0, 12.0), d: v(13.0, 14.0, 15.0, 16.0),
    m: Mat4 {
        c0: v(17.0, 0.0, 0.0, 0.0), c1: v(0.0, 18.0, 0.0, 0.0),
        c2: v(0.0, 0.0, 19.0, 0.0), c3: v(0.0, 0.0, 0.0, 20.0),
    },
}

fn pack_global(f: Ptr<float>, i: Ptr<int>) {
    pack_uniform_block(f, i, BLOCK)
}

fn main() {
    let f = native_buffer_float(32)
    let i = native_buffer_int(32)
    pack_global(f.ptr, i.ptr)
    print(f.get(0).unwrap())
    print(f.get(5).unwrap())
    print(f.get(15).unwrap())
    print(f.get(16).unwrap())
    print(f.get(31).unwrap())
}
"#;

#[test]
fn a_global_block_packs_one_leaf_at_a_time() {
    let lua = emit_module(GLOBAL_SOURCE);
    let body = function_body(&lua, "pack_global");
    let decl = body
        .lines()
        .find(|l| l.trim_start().starts_with("local "))
        .unwrap_or_else(|| panic!("no local declaration in:\n{body}"));
    let registers = decl
        .split(',')
        .filter(|n| n.trim().starts_with('r') || n.trim().starts_with("local r"))
        .count();
    assert!(
        registers < 32,
        "{registers} registers for a 32-leaf block: every leaf is loaded before the first store:\n{body}"
    );
    let out = run_main(&lua);
    assert_eq!(
        out.split_whitespace().collect::<Vec<_>>(),
        ["1", "6", "16", "17", "20"]
    );
}
