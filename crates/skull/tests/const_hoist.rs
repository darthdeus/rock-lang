use crate::common::{emit_module, emit_v1_package, function_body, run_luajit, run_main};

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

const TWO_CONST_NESTED: &str = r#"
value FixVec2 { x: float, y: float }
value Outer { a: FixVec2, b: FixVec2, c: float }

// `Outer` exceeds `MAX_COLUMN_LEAVES`, so the Vec boxes its elements and the
// ctor must be built; a NativeBuffer or column Vec would leave nothing to hoist.
fn fill(out: [Outer], n: int) {
    for i in 0..n {
        out.push(Outer { a: FixVec2 { x: 3.0, y: 7.0 }, b: FixVec2 { x: 3.0, y: 7.0 }, c: i as float })
    }
}

fn main() {
    let out: [Outer] = []
    fill(out, 4)
    print("{out.len}")
}
"#;

#[test]
fn const_value_struct_ctor_is_hoisted_to_module_scope() {
    let lua = emit_module(TWO_CONST_NESTED);
    let builds = count(&lua, "FixVec2_ct(3.0, 7.0)");
    assert_eq!(builds, 1);
    assert!(lua.contains("__skull_kc_") && lua.contains("= FixVec2_ct(3.0, 7.0)"));
    let body = function_body(&lua, "fill");
    assert_eq!(count(body, "FixVec2_ct("), 0);
    assert!(body.contains("__skull_kc_"));
}

const PACKAGE_MAIN: &str = r#"
value Seg { a: a::Pt, b: a::Pt, c: float }
fn main() {
    let out: [Seg] = []
    for i in 0..2 {
        out.push(Seg { a: a::Pt { x: 3.0, y: 7.0 }, b: a::make(i as float), c: 1.0 })
    }
    print("{a::label(out[1].a)} {a::label(out[1].b)}")
}
"#;

const PACKAGE_MOD_A: &str = r#"
mod a;

pub value Pt {
    x: float,
    y: float,
}

pub fn make(x: float) -> Pt {
    return Pt { x, y: x }
}

pub fn label(p: Pt) -> string {
    return "{p.x}/{p.y}"
}
"#;

#[test]
fn hoisted_package_value_ctor_uses_the_emitted_type_symbol() {
    let emitted = emit_v1_package(
        "kcpkg",
        &[("main.rock", PACKAGE_MAIN), ("a.rock", PACKAGE_MOD_A)],
    );
    assert!(emitted.lua.contains("__skull_kc_"));
    assert_eq!(run_main(&emitted.lua), "3/7 1/1");
}

const CDATA_ENUM_UNITS: &str = r#"
enum Slice { Sliced, Empty, Leather(int) }
struct Holder { slice: Slice }

@nomangle fn in_field() -> any { Holder { slice: Slice::Sliced } }
@nomangle fn reset(h: Holder) { h.slice = Slice::Sliced }
@nomangle fn give(h: Holder, n: int) { h.slice = Slice::Leather(n) }
@nomangle fn in_vec(n: int) -> [Slice] {
    let v: [Slice] = []
    for _ in 0..n { v.push(Slice::Empty) }
    v
}
@nomangle fn returned(k: int) -> Slice { if k == 0 { Slice::Sliced } else { Slice::Leather(k) } }
@nomangle fn maybe(k: int) -> Slice? { if k < 0 { null } else { Slice::Empty } }
@nomangle fn is_sliced(h: Holder) -> bool { h.slice == Slice::Sliced }
"#;

#[test]
fn constant_cdata_enum_values_are_shared_wherever_they_are_stored() {
    let lua = emit_module(CDATA_ENUM_UNITS);
    for f in ["in_field", "reset", "in_vec", "returned", "maybe"] {
        let body = function_body(&lua, f);
        assert!(
            !body.contains("Slice_ct((0)") && !body.contains("Slice_ct((1)"),
            "{f}: {body}"
        );
    }
    let out = run_luajit(&format!(
        r#"{lua}
local h = in_field()
local v = in_vec(2)
assert(h.slice == returned(0) and rawequal(v._data[1], v._data[2]) and v._data[1] == maybe(0))
give(h, 5)
assert(h.slice._0 == 5 and returned(0).__tag == 0 and returned(0)._0 == 0)
assert(not is_sliced(h))
reset(h)
assert(is_sliced(h) and h.slice == returned(0) and returned(3) == returned(3))
assert(maybe(-1) == nil)
local slots = {{}}
for i = 1, 200 do slots[i] = false end
for _ = 1, 1000 do for i = 1, 200 do reset(h); slots[i] = returned(0) end end
collectgarbage("collect"); collectgarbage("stop")
local before = collectgarbage("count")
for _ = 1, 1000 do for i = 1, 200 do reset(h); slots[i] = returned(0) end end
local bytes = (collectgarbage("count") - before) * 1024
assert(bytes < 20000, bytes .. " bytes for 200000 unit stores")
print("ok")
"#
    ));
    assert_eq!(out, "ok");
}
