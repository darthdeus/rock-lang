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

use crate::common::{bone_rejections, emit_module, function_body, run_luajit};

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
    assert!(rejects.is_empty(), "uniform_values was rejected: {rejects:#?}");

    let lua = emit_module(CANONICAL_VALUES);
    let body = main_body(&lua);
    assert!(!body.contains("uniform_values("), "the compiler-owned call survived:\n{body}");
    for forbidden in ["std140", "offset", "padding", "binding"] {
        assert!(!body.contains(forbidden), "canonical values leaked {forbidden}:\n{body}");
    }
    for variant in ["Float", "Bool", "Int", "Uint"] {
        assert!(
            body.contains(&format!("__variant = \"{variant}\"")),
            "canonical `{variant}` leaf was not built:\n{body}"
        );
    }
    let out = run_luajit(&format!("{lua}\nmain()\n"), "canonical_uniform_values");
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
fn main_body(lua: &str) -> String {
    function_body(lua, "main").to_string()
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
    let out = run_luajit(&format!("{}\nmain()\n", emit_module(src)), "uint_uniform_bits");
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["-1"]);
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

/// A value with a nested `nobox` member, bound to a local inside a lambda and
/// packed there. Shared by the assertion and control 1 so they cannot drift.
const PROBE: &str = r#"
value ProbeU {
    scalar: float,
    params: Vec2,
}

fn main() {
    let body = () => {
        let f = native_buffer_float(8)
        let i = native_buffer_int(8)
        let u = ProbeU { scalar: 1.0, params: v(5.0, 6.0) }
        pack_uniform_block(f.ptr, i.ptr, u)
        print(f[0])
        print(f[2])
        print(f[3])
    }
    body()
}
"#;

/// A nested `nobox` member reaches the packer when the value is bound to a
/// LOCAL inside a LAMBDA. The nested member is stored under its dotted leaf
/// names, so the immediate `.params` read names no field cell; redirecting it
/// to one anyway produced a load with no store, and the emitted body indexed a
/// nil local. The three controls below each drop one leg of that shape.
#[test]
fn a_nested_member_survives_a_local_inside_a_lambda() {
    let out = run_luajit(&format!("{}\nmain()\n", emit_module(PROBE)), "probe_lambda_local");
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "5", "6"]);
}

/// Control 1: the same lambda, same nested member, but the value goes STRAIGHT
/// to the packer instead of through a local. Isolates the local.
#[test]
fn control_a_literal_packed_directly_inside_a_lambda() {
    let src = PROBE.replace(
        "let u = ProbeU { scalar: 1.0, params: v(5.0, 6.0) }\n        pack_uniform_block(f.ptr, i.ptr, u)",
        "pack_uniform_block(f.ptr, i.ptr, ProbeU { scalar: 1.0, params: v(5.0, 6.0) })",
    );
    // A `.replace` that matched nothing yields the PROBE verbatim, and this
    // control would then silently re-run the main case instead of its own.
    assert_ne!(src, PROBE, "the control's substitution matched nothing");
    let out = run_luajit(
        &format!("{}\nmain()\n", emit_module(&src)),
        "probe_lambda_literal",
    );
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "5", "6"]);
}

/// Control 2: the same lambda AND the same local, with the nested `Vec2`
/// replaced by the scalar fields it would peel to. Isolates the NESTING -- the
/// leg the other controls both keep, so without it "all three legs" would be
/// asserted rather than shown.
#[test]
fn control_a_flat_value_in_a_lambda_local() {
    let out = run_luajit(
        &format!(
            "{}\nmain()\n",
            emit_module(
                r#"
value FlatU {
    scalar: float,
    px: float,
    py: float,
}

fn main() {
    let body = () => {
        let f = native_buffer_float(8)
        let i = native_buffer_int(8)
        let u = FlatU { scalar: 1.0, px: 5.0, py: 6.0 }
        pack_uniform_block(f.ptr, i.ptr, u)
        print(f[0])
        print(f[1])
        print(f[2])
    }
    body()
}
"#
            )
        ),
        "probe_lambda_flat",
    );
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "5", "6"]);
}

/// Control 3: the same local and the same nested member, but at function scope
/// rather than in a lambda. Isolates the lambda.
#[test]
fn control_a_local_packed_at_function_scope() {
    let out = run_luajit(
        &format!("{}\nmain()\n", emit_module(
            r#"
value ProbeU {
    scalar: float,
    params: Vec2,
}

fn main() {
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    let u = ProbeU { scalar: 1.0, params: v(5.0, 6.0) }
    pack_uniform_block(f.ptr, i.ptr, u)
    print(f[0])
    print(f[2])
    print(f[3])
}
"#,
        )),
        "probe_fnscope_local",
    );
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "5", "6"]);
}

/// `uniform_block_id(expr)` reads only its expression's TYPE -- it must never
/// LOWER the expression. Load-bearing rather than incidental: it is what lets
/// `set_uniform_block_if_present` name its value twice while evaluating it once
/// (inside the found branch), which is what keeps a `SpriteFragU` off the
/// sprite flush's hot path when the shader declares its own group instead.
///
/// Measured with an observable side effect rather than by reading the emit, so
/// the property is pinned at runtime and not by a substring that could drift.
#[test]
fn the_fingerprint_never_evaluates_its_argument() {
    let out = run_luajit(
        &format!(
            "{}\nmain()\n",
            emit_module(
                r#"
value FpProbeU { a: float, b: float }

global CALLS: int = 0

fn make_probe() -> FpProbeU {
    CALLS = CALLS + 1
    return FpProbeU { a: 1.0, b: 2.0 }
}

fn main() {
    let id = uniform_block_id(make_probe())
    // Non-zero proves the intrinsic really produced this type's fingerprint,
    // so a zero `CALLS` cannot be the whole call having been dropped.
    print(id != 0)
    print(CALLS)

    // ... and the SAME constructor does run when it is actually packed, so the
    // zero above is the fingerprint's doing and not an inert fixture.
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    pack_uniform_block(f.ptr, i.ptr, make_probe())
    print(CALLS)
}
"#
            )
        ),
        "fingerprint_no_eval",
    );
    assert_eq!(
        out.split_whitespace().collect::<Vec<_>>(),
        ["true", "0", "1"],
        "expected a real fingerprint, ZERO constructor calls for it, and one \
         call once the value is genuinely packed"
    );
}

/// A block packed out of a GLOBAL is read one leaf at a time, each leaf loaded
/// right before its own store. Lowering the whole value first loads every leaf
/// into a register before the first store, and in a trace that keeps all of
/// them live in every exit snapshot taken during the pack -- the engine block's
/// 41 leaves against LuaJIT's 16-register side-trace budget
/// (docs/trace-pressure-work-plan.md). Pinned by register count: the packing
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

fn fn_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let at = lua.find(&format!("\nfunction {name}")).unwrap_or_else(|| panic!("{name} is emitted"));
    let rest = &lua[at + 1..];
    let end = rest.find("\nend").expect("function has an end");
    &rest[..end]
}

#[test]
fn a_global_block_packs_one_leaf_at_a_time() {
    let lua = emit_module(GLOBAL_SOURCE);
    let body = fn_body(&lua, "pack_global");
    let decl = body
        .lines()
        .find(|l| l.trim_start().starts_with("local "))
        .unwrap_or_else(|| panic!("no local declaration in:\n{body}"));
    let registers = decl.split(',').filter(|n| n.trim().starts_with('r') || n.trim().starts_with("local r")).count();
    assert!(
        registers < 32,
        "{registers} registers for a 32-leaf block: every leaf is loaded before the first store:\n{body}"
    );
    let out = run_luajit(&format!("{lua}\nmain()\n"), "global_block_pack");
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "6", "16", "17", "20"]);
}
