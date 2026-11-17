use crate::common::{emit_module, emit_v1_package, function_body, mangled_name, run_luajit};

const HOLDER: &str = r#"
value V2 { x: float, y: float }

@nomangle inline(lua) fn __nb_alloc_V2(n: int) -> Ptr<V2> = 'return ffi.new("V2[?]", n)'

value BufHolder { buf: NativeBuffer<V2>, tag: int }

fn bump(h: BufHolder) {
    h.buf[1] = V2 { x: h.buf[1].x + 1.0, y: h.buf[1].y + 2.0 }
}

fn main() {
    let h = BufHolder { buf: NativeBuffer { ptr: __nb_alloc_V2(2), len: 2 }, tag: 1 }
    bump(h)
}
"#;

#[test]
fn native_buffer_in_value_is_not_rebuilt() {
    let lua = emit_module(HOLDER);
    let body = function_body(&lua, "bump");
    assert!(!body.contains("NativeBuffer_vt(") && !body.contains("Ptr_vt("));
}

const USER_NAMED: &str = r#"
value NativeBuffer { a: int, b: float }

fn take(p: NativeBuffer) -> float {
    return p.b + (p.a as float)
}

pub fn probe() -> float {
    let p = NativeBuffer { a: 1, b: 2.0 }
    let q = p
    return take(q)
}
"#;

#[test]
fn user_value_named_like_a_descriptor_keeps_scalarization() {
    let emitted = emit_v1_package("userdesc", &[("main.rock", USER_NAMED)]);
    let take = mangled_name(&emitted.lua, "take");
    let header = emitted
        .lua
        .lines()
        .find(|l| l.starts_with(&format!("function {take}(")))
        .expect("take is emitted");
    assert!(header.contains(", "));
    let probe = mangled_name(&emitted.lua, "probe");
    assert_eq!(
        run_luajit(&format!("{}\nprint({probe}())\n", emitted.lua)),
        "3"
    );
}
