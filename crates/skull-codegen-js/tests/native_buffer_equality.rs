use skull_mir::mir::FuncRef;

const SRC: &str = r#"
value P2 { x: float, y: float }
value Empty {}
value Mixed { flag: bool, tag: int, x: f32, y: float }

fn pair(cap: int) -> FlatVec<P2> {
    let v: FlatVec<P2> = flat_vec_new(cap)
    v.push(P2 { x: 1.0, y: 2.0 })
    v.push(P2 { x: 3.0, y: 4.0 })
    return v
}
fn flat_vec_equality_ignores_capacity() -> bool {
    return pair(4) == pair(32)
}
fn flat_vec_equality_reads_only_live_elements() -> bool {
    let reused = pair(4)
    reused.push(P2 { x: 9.0, y: 9.0 })
    reused.clear()
    reused.push(P2 { x: 1.0, y: 2.0 })
    reused.push(P2 { x: 3.0, y: 4.0 })
    return reused == pair(4)
}
fn flat_vec_empty_vectors_are_equal() -> bool {
    let a: FlatVec<P2> = flat_vec_new(2)
    let b: FlatVec<P2> = flat_vec_new(8)
    return a == b
}
fn scalar_flat_vec_equality() -> bool {
    let a: FlatVec<float> = flat_vec_new(2)
    let b: FlatVec<float> = flat_vec_new(8)
    a.push(1.5)
    b.push(1.5)
    if a != b { return false }
    b.push(3.0)
    return a != b
}
fn empty_scalar_native_buffers_are_equal() -> bool {
    if native_buffer_int(0) != native_buffer_int(0) { return false }
    let a: FlatVec<int> = flat_vec_new(1)
    let b: FlatVec<int> = flat_vec_new(4)
    return a == b
}
fn zero_sized_native_buffer_elements_are_equal() -> bool {
    let a: NativeBuffer<Empty> = native_buffer_alloc(2)
    let b: NativeBuffer<Empty> = native_buffer_alloc(2)
    return a == b
}
fn native_buffer_equality_checks_live_bytes() -> bool {
    let a: NativeBuffer<Mixed> = native_buffer_alloc(2)
    let b: NativeBuffer<Mixed> = native_buffer_alloc(2)
    a[1] = Mixed { flag: true, tag: -7, x: 1.5, y: 1.0000000001 }
    b[1] = Mixed { flag: true, tag: -7, x: 1.5, y: 1.0000000001 }
    if a != b { return false }
    b[1] = Mixed { flag: true, tag: -7, x: 1.5, y: 1.0000000002 }
    if a == b { return false }
    let empty: NativeBuffer<Mixed> = native_buffer_alloc(0)
    return a != empty
}
fn flat_vec_equality_checks_live_bytes() -> bool {
    let a: FlatVec<Mixed> = flat_vec_new(1)
    let b: FlatVec<Mixed> = flat_vec_new(8)
    a.push(Mixed { flag: true, tag: -7, x: 1.5, y: 1.0000000001 })
    b.push(Mixed { flag: true, tag: -7, x: 1.5, y: 1.0000000001 })
    if a != b { return false }
    b[0] = Mixed { flag: false, tag: -7, x: 1.5, y: 1.0000000001 }
    return a != b
}
"#;

fn check(root: &str) {
    let ws = crate::common::ws_with(SRC);
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (fid, _) = ws.module(mid).functions.iter()
        .find(|(_, f)| f.name.as_str() == root).expect("root");
    let js = skull_codegen_js::emit::emit_with_closure(
        &ws, &[(FuncRef { module: mid, func: fid }, root.to_string())],
    ).expect("buffer equality emits");
    let program = format!(
        "{}{}{}{}\n{js}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRUCTURAL_EQ,
    );
    let out = crate::common::node().arg("-e").arg(&program).output().expect("node");
    assert!(out.status.success(), "{root}: {}\n{program}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true", "{root}");
}

#[test]
fn flat_vec_equality_ignores_capacity() { check("flat_vec_equality_ignores_capacity"); }
#[test]
fn flat_vec_equality_reads_only_live_elements() { check("flat_vec_equality_reads_only_live_elements"); }
#[test]
fn flat_vec_empty_vectors_are_equal() { check("flat_vec_empty_vectors_are_equal"); }
#[test]
fn scalar_flat_vec_equality() { check("scalar_flat_vec_equality"); }
#[test]
fn empty_scalar_native_buffers_are_equal() { check("empty_scalar_native_buffers_are_equal"); }
#[test]
fn native_buffer_equality_checks_live_bytes() { check("native_buffer_equality_checks_live_bytes"); }
#[test]
fn flat_vec_equality_checks_live_bytes() { check("flat_vec_equality_checks_live_bytes"); }

#[test]
fn zero_sized_native_buffer_elements_are_equal() { check("zero_sized_native_buffer_elements_are_equal"); }
