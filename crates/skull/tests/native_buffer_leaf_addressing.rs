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
