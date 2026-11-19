use crate::common::{emit_module, function_body};

const VALUE_INST: &str = r#"
value V2 { x: float, y: float }

@nomangle inline(lua) fn __nb_alloc_V2(n: int) -> Ptr<V2> = 'return ffi.new("V2[?]", n)'

@inline fn gset<T>(b: NativeBuffer<T>, i: int, v: T) { b[i] = v }
@inline fn gget<T>(b: NativeBuffer<T>, i: int) -> T { return b[i] }

fn use_buf(a: V2) -> V2 {
    let b: NativeBuffer<V2> = NativeBuffer { ptr: __nb_alloc_V2(4), len: 4 }
    gset(b, 0, a)
    return gget(b, 0)
}

fn main() { let r = use_buf(V2 { x: 1.0, y: 2.0 }); print("{r.x}") }
"#;

#[test]
fn generic_inline_at_value_type_splices_zero_box() {
    let lua = emit_module(VALUE_INST);
    let body = function_body(&lua, "use_buf");

    assert!(!body.contains("V2_ct("));
    assert!(!body.contains("__nb_checked_set"));
    assert!(body.contains("__nb_checked_index"));
    assert!(!body.contains("__nb_checked_get"));
}

const TRANSITIVE_CONCRETE_INTRINSIC: &str = r#"
value TV2 { x: float, y: float }

fn make_indirect<T>(cap: int) -> FlatVec<T> {
    return flat_vec_new(cap)
}

fn main() {
    let fv: FlatVec<TV2> = make_indirect(4)
    print("{fv.len}")
}
"#;

#[test]
fn non_inline_generic_chain_uses_concrete_native_buffer_instance() {
    let lua = emit_module(TRANSITIVE_CONCRETE_INTRINSIC);
    assert!(lua.contains(r#"__rock_nb_alloc("TV2"#));
}
