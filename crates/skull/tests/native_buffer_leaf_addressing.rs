use crate::common::{emit_module, function_body};

const NESTED_STORE: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, id: int, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

fn fill(buf: NativeBuffer<Vert>, n: int) {
    for i in 0..n {
        let f = i as float
        buf[i] = Vert { pos: V2 { x: f, y: f }, uv: V2 { x: f, y: f }, id: i, w: 1.0 }
    }
}

fn main() {
    let buf = nb_vert(4)
    fill(buf, 4)
}
"#;

const NESTED_READ: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, id: int, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

fn total(buf: NativeBuffer<Vert>, n: int) -> float {
    let acc = 0.0
    for i in 0..n {
        acc = acc + buf[i].pos.x + buf[i].w
    }
    return acc
}

fn main() {
    let buf = nb_vert(4)
    print("{total(buf, 4)}")
}
"#;

#[test]
fn a_whole_element_store_bounds_checks_once_without_ctor_or_slot_reference() {
    let lua = emit_module(NESTED_STORE);
    let body = function_body(&lua, "fill");
    assert!(!body.contains("V2_ct("));
    assert!(!body.contains("__nb_checked_get("));

    let checks = body.matches("checked").count();
    assert_eq!(checks, 1);

    let check_at = body.find("checked").expect("a bounds check");
    let first_view_use = body
        .find(".__v")
        .unwrap_or_else(|| panic!("a typed-view use, so the ordering below is checked:\n{body}"));
    assert!(check_at < first_view_use);
}

#[test]
fn nested_value_read_takes_no_slot_reference() {
    let lua = emit_module(NESTED_READ);
    let body = function_body(&lua, "total");
    assert!(!body.contains("__nb_checked_get("));
}

const SINGLE_FIELD_READ: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, id: int, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

fn read_one(buf: NativeBuffer<Vert>, i: int) -> float {
    return buf[i].pos.x
}

fn main() {
    let b = nb_vert(2)
    print("{read_one(b, 0)}")
}
"#;

#[test]
fn a_single_field_leaf_read_loads_only_that_leaf() {
    let lua = emit_module(SINGLE_FIELD_READ);
    let body = function_body(&lua, "read_one");
    let loads = body.matches("[(").count() + body.matches("[__").count();
    let view_reads = body.matches(".__v").count();
    assert!(view_reads <= 1);
    assert!(loads <= 1);
    // Dropping the unused loads is only sound while the bounds check is hoisted.
    assert!(body.contains("__nb_checked_index("));
}

const F32_CTOR_STORE: &str = r#"
value V2f { x: f32, y: f32 }
value FVert { pos: V2f, w: f32, id: int, py: f32 }

@nomangle inline(lua) fn __nb_alloc_FVert(n: int) -> Ptr<FVert> = 'return ffi.new("FVert[?]", n)'
fn nb_fvert(n: int) -> NativeBuffer<FVert> { return NativeBuffer { ptr: __nb_alloc_FVert(n), len: n } }

@pure fn fvert(x: float, y: float, w: float, id: int, py: float = 0.0) -> FVert {
    return FVert { pos: V2f { x: x, y: y }, w: w, id: id, py: py }
}

fn fill_f32(buf: NativeBuffer<FVert>, n: int) {
    for i in 0..n {
        let f = (i as float) * 0.1
        buf[i] = fvert(f, f + 0.3, 0.7, i)
    }
}

fn main() {
    let buf = nb_fvert(4)
    fill_f32(buf, 4)
}
"#;

#[test]
fn an_inlined_f32_ctor_store_rounds_only_in_the_float_view() {
    let lua = emit_module(F32_CTOR_STORE);
    let body = function_body(&lua, "fill_f32");
    assert!(!body.contains("fvert("));
    assert!(body.contains(".__vf"));
    assert!(!body.contains("__rock_f32("));
}
