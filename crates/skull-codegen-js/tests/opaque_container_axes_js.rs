//! The TARGET-DEPENDENT container axes of the opaque-body trigger. Its Arena
//! twin, which answers the same on every target, is
//! `crates/skull/tests/opaque_container_axes.rs`.
//!
//! `FlatVec<E>` is `{buf = NativeBuffer{ptr, len}, len}` and `NativeBuffer<E>`
//! is `{ptr, len}`. On luajit `ptr` is an `E*`, so `ptr[i]` yields an element
//! REFERENCE and an opaque body reading it is right. JS backs both with flat
//! slots, where `ptr[i]` is a leaf and a whole-element read silently returns a
//! number. So this is a JS-ONLY rejection -- the same asymmetry
//! `spec_key_has_target_storage` already makes.

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

const EXPANSION_INTRINSIC: &str = r#"
value G { x: f32, y: f32 }

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
    crate::common::emit_closure(&crate::common::ws_with(src), "main").map(|_| ())
}

/// The luajit control, through the ordinary module emit.
fn lua(src: &str) -> Result<(), String> {
    let ws = crate::common::ws_with(src);
    let mid = crate::common::test_mid(&ws);
    skull::mir_emit::emit_workspace_mir(&ws, mid)
        .map(|_| ())
        .map_err(|e| e.message)
}

#[test]
fn leaf_addressed_container_params_are_target_specific() {
    for (name, source, diagnostic) in [
        ("FlatVec<G>", FLATVEC_PARAM, Some("probe")),
        ("NativeBuffer<G>", NATIVEBUFFER_PARAM, None),
    ] {
        let error = js(source).expect_err("leaf-addressed element must reject on JS");
        assert!(error.contains(name), "{name}: {error}");
        if let Some(diagnostic) = diagnostic {
            assert!(error.contains(diagnostic), "{name}: {error}");
        }
        assert!(
            lua(source).is_ok(),
            "luajit element references admit {name}"
        );
    }
}

#[test]
fn a_primitive_element_buffer_is_admitted_on_js() {
    let result = js(NATIVEBUFFER_PRIMITIVE);
    assert!(
        result.is_ok(),
        "no leaf plan means no representation the boxed form does not have: {result:?}"
    );
}

#[test]
fn an_expansion_intrinsic_over_a_native_buffer_is_admitted() {
    assert!(
        js(EXPANSION_INTRINSIC).is_ok(),
        "js: {:?}",
        js(EXPANSION_INTRINSIC)
    );
    assert!(lua(EXPANSION_INTRINSIC).is_ok());
}
