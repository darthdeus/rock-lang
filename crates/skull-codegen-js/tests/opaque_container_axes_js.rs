//! The TARGET-DEPENDENT container axes of the 2.4 trigger
//! (`docs/boundary-contracts.md`). Its Arena twin, which answers the same on
//! every target, is `crates/skull/tests/opaque_container_axes.rs`.
//!
//! `FlatVec<E>` is `{buf = NativeBuffer{ptr, len}, len}` and `NativeBuffer<E>`
//! is `{ptr, len}`. On luajit `ptr` is an `E*`, so `ptr[i]` yields an element
//! REFERENCE and an opaque body reading it is right. JS backs both with flat
//! slots, where `ptr[i]` is a leaf and a whole-element read silently returns a
//! number. So this is a JS-ONLY rejection -- the same asymmetry
//! `spec_key_has_target_storage` already makes, and the reason 2.3 correction 2
//! called "NativeBuffer can never be affected" Lua-only reasoning.
//!
//! The live production instance is `__vbo_as_float_ptr(buf: NativeBuffer<Vertex>)`
//! (`stone/engine/web/js_ffi.rock`), which the acceptance contract names.


const FLATVEC_PARAM: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe(f: FlatVec<G>) -> int = """
    return f.len
"""
@nomangle inline(js) fn probe(f: FlatVec<G>) -> int = """
    return f.len;
"""

fn main() {
    let f: FlatVec<G> = flat_vec_new()
    print("{probe(f)}")
}
"#;

const NATIVEBUFFER_PARAM: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe(b: NativeBuffer<G>) -> int = """
    return b.len
"""
@nomangle inline(js) fn probe(b: NativeBuffer<G>) -> int = """
    return b.len;
"""

fn main() {
    let b: NativeBuffer<G> = native_buffer_alloc(2)
    print("{probe(b)}")
}
"#;

/// The PRIMITIVE-element bound. `NativeBuffer<float>` has no leaf plan -- there
/// is no per-element view to lose -- so it stays admitted on JS too. Without
/// this, "reject every NativeBuffer" would pass the two cases above.
const NATIVEBUFFER_PRIMITIVE: &str = r#"
@nomangle inline(lua) fn probe(b: NativeBuffer<float>) -> int = """
    return b.len
"""
@nomangle inline(js) fn probe(b: NativeBuffer<float>) -> int = """
    return b.len;
"""

fn main() {
    let b: NativeBuffer<float> = native_buffer_alloc(2)
    print("{probe(b)}")
}
"#;

/// An `ExpandKind` body is exempt BY CONSTRUCTION (2.3 correction 3): its CPU
/// body is `error("..is compiler-expanded")` and never runs, so rejecting it
/// would reject a body that cannot misread anything. `gpu_mesh.rock`'s two
/// `@vertex_expand FlatVec<T>` bodies are the production instance.
///
/// Deliberately NON-generic and over `NativeBuffer` rather than `FlatVec`. Both
/// choices avoid a rejection that has nothing to do with the exemption and
/// would read as the exemption failing: a generic spelling drags in
/// `native_buffer_alloc<T>`'s "needs a concrete element type", and `FlatVec`'s
/// constructor reaches that same rejection on JS even at a concrete element.
/// `NativeBuffer` is the same axis of this rule and emits on both targets --
/// `a_native_buffer_at_a_leaf_addressed_element_rejects_on_js` is the proof
/// that this fixture WOULD reject without the exemption.
const EXPANSION_INTRINSIC: &str = r#"
value G { x: float, y: float }

@vertex_expand inline(lua) fn probe_stride(buf: NativeBuffer<G>) -> int = """
    error("probe_stride is compiler-expanded")
"""
@vertex_expand inline(js) fn probe_stride(buf: NativeBuffer<G>) -> int = """
    throw new Error("probe_stride is compiler-expanded");
"""

fn main() {
    let b: NativeBuffer<G> = native_buffer_alloc(2)
    print("{probe_stride(b)}")
}
"#;

/// Emit the `main` closure for the JS target.
fn js(src: &str) -> Result<(), String> {
    let ws = crate::common::ws_with(src);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let root = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .map(|(fid, _)| skull_mir::mir::FuncRef { module: mid, func: fid })
        .expect("main");
    skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "main".to_string())])
        .map(|_| ())
        .map_err(|e| e.message)
}

/// The luajit control, through the ordinary module emit.
fn lua(src: &str) -> Result<(), String> {
    let ws = crate::common::ws_with(src);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    skull::mir_emit::emit_workspace_mir(&ws, mid).map(|_| ()).map_err(|e| e.message)
}

#[test]
fn a_flatvec_at_a_leaf_addressed_element_rejects_on_js() {
    let msg = js(FLATVEC_PARAM).expect_err("must reject on JS");
    assert!(msg.contains("FlatVec<G>") && msg.contains("probe"), "got: {msg}");
}

/// The TARGET bound. luajit's `ptr` is an `E*`, so the identical body is right
/// there -- without this the rejection above would read as "FlatVec is banned".
#[test]
fn the_same_flatvec_body_is_admitted_on_lua() {
    assert!(lua(FLATVEC_PARAM).is_ok(), "luajit `ptr[i]` is an element reference");
}

#[test]
fn a_native_buffer_at_a_leaf_addressed_element_rejects_on_js() {
    let msg = js(NATIVEBUFFER_PARAM).expect_err("must reject on JS");
    assert!(msg.contains("NativeBuffer<G>"), "got: {msg}");
}

#[test]
fn the_same_native_buffer_body_is_admitted_on_lua() {
    assert!(lua(NATIVEBUFFER_PARAM).is_ok());
}

/// The ELEMENT bound, on the target that rejects.
#[test]
fn a_primitive_element_buffer_is_admitted_on_js() {
    let result = js(NATIVEBUFFER_PRIMITIVE);
    assert!(result.is_ok(), "no leaf plan means no representation the boxed form does not have: {result:?}");
}

/// The acceptance contract's "expansion intrinsics are exempt BY CONSTRUCTION",
/// and BY CONSTRUCTION is literal: the call is replaced during expansion and
/// never reaches the check. A `fn_expand_kind` guard was added here, mutated,
/// and found INERT -- this stayed green with the guard disabled -- so it was
/// deleted. This test is what keeps the structural claim honest.
#[test]
fn an_expansion_intrinsic_over_a_native_buffer_is_admitted() {
    assert!(js(EXPANSION_INTRINSIC).is_ok(), "js: {:?}", js(EXPANSION_INTRINSIC));
    assert!(lua(EXPANSION_INTRINSIC).is_ok());
}
