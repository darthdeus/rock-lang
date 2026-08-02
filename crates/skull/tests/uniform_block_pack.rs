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

use crate::common::{emit_module, run_luajit};

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

/// The shape that miscompiles: a value with a nested `nobox` member, bound to
/// a local inside a lambda and packed there. Shared by the sentinel and the
/// live assertion so they cannot drift apart.
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

/// SENTINEL (`docs/test-sentinels.md`) -- PINNED BUG, inverted assertion.
///
/// CORRECT behavior: this prints `1 5 6`, like both controls below. What it
/// does today is die at runtime with `attempt to index local` -- the nested
/// `Vec2`'s producer is eliminated while the packer still reads it BOXED out
/// of a cell nothing ever stores to:
///
/// ```lua
/// r1 = c0        -- c0 is declared and never assigned
/// r2 = r1.x
/// ```
///
/// The discriminant is all three of: a nested `nobox` member (`Vec2`), the
/// value bound to a LOCAL, and that local living inside a LAMBDA. Each control
/// below drops exactly one and passes. `ProbeU` is a plain `value` that merely
/// CONTAINS a nobox member, so it never enters `group_binds` -- that path wants
/// the value itself to be flat-enum or nobox (`lower/stmt.rs`'s
/// `threaded_shape`) -- which is where the producer and the consumer stop
/// agreeing.
///
/// WHEN THIS FAILS the fix landed: delete this test and un-`ignore` the real
/// assertion in `a_nested_member_survives_a_local_inside_a_lambda` below,
/// in the same commit.
#[test]
fn sentinel_a_nested_member_in_a_lambda_local_reads_an_unassigned_cell() {
    let lua = format!("{}\nmain()\n", emit_module(PROBE));
    let path = std::env::temp_dir().join(format!("skull_sentinel_{}.lua", std::process::id()));
    std::fs::write(&path, &lua).expect("write program");
    let out = std::process::Command::new("luajit")
        .arg(&path)
        .output()
        .expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success() && stderr.contains("attempt to index local"),
        "PINNED BEHAVIOR (BUG): packing a nested nobox member out of a lambda \
         local indexes an unassigned cell. This now succeeds, so the fix \
         landed -- flip per the comment above.\nstderr:\n{stderr}"
    );
}

/// The real assertion the sentinel above is standing in for. `#[ignore]`d
/// rather than deleted so the fix has a ready oracle: run it with
/// `cargo test -p skull -- --ignored a_nested_member`.
#[test]
#[ignore = "pinned by sentinel_a_nested_member_in_a_lambda_local_reads_an_unassigned_cell"]
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
    let out = run_luajit(
        &format!("{}\nmain()\n", emit_module(&src)),
        "probe_lambda_literal",
    );
    assert_eq!(out.split_whitespace().collect::<Vec<_>>(), ["1", "5", "6"]);
}

/// Control 2: the same local and the same nested member, but at function scope
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
