//! Cut 1: a uniform single-scalar-payload
//! enum is a `value`-struct-style cdata `{ int __tag; <T> _0; }` -- NO
//! `{__variant}` table, reflection sacrificed for performance. It reuses the whole
//! value-struct machinery: `<Enum>_ct` ctor, value-struct SROA (a local construct+
//! match scalarizes to two int locals + int-tag compare, 0 alloc), value `__eq`
//! (byte equality so escaping enums compare correctly), scalar ABI through calls.
use crate::common::{emit_module, require_luajit};

const SRC: &str = r#"
enum Shape { Circle(int), Square(int), Point }

fn classify(n: int) -> int {
    let s = Shape::Circle(n)
    if n % 3 == 1 { s = Shape::Square(n * 2) }
    if n % 3 == 2 { s = Shape::Point }
    return match s {
        Shape::Circle(rad) => rad + 1,
        Shape::Square(side) => side * 2,
        Shape::Point => 7,
    }
}

// Escaping: stored in a Vec, so the enum is a real cdata (not SROA scalars). The
// value `__eq` metatype must make equal-field cdata compare equal.
fn eq_escaping() -> int {
    let xs = [Shape::Circle(5), Shape::Circle(5), Shape::Square(5)]
    let a = if xs[0] == xs[1] { 1 } else { 0 }
    let b = if xs[0] == xs[2] { 1 } else { 0 }
    return a * 10 + b
}

@test(output = "1 4 7 4 | 10")
fn t() {
    print("{classify(0)} {classify(1)} {classify(2)} {classify(3)} | {eq_escaping()}")
}

fn main() { t() }
"#;

#[test]
fn cdata_enum_emits_ctype_and_no_table() {
    let lua = emit_module(SRC);
    assert!(
        lua.contains("} Shape;") && lua.contains("Shape_ct = ffi.typeof"),
        "expected a Shape cdef + ctor:\n{}",
        lua.split("\nfunction ").next().unwrap_or(&lua)
    );
    // No `{__variant}` table anywhere for a cdata enum (reflection sacrificed).
    assert!(!lua.contains("__variant = \"Circle\""), "cdata enum must not build a table:\n{lua}");
    // The value `__eq` metatype is attached (escaping equality).
    assert!(
        lua.contains("ffi.metatype, \"Shape\""),
        "expected an ffi.metatype for the Shape enum cdata:\n{lua}"
    );
    // Match int-compares the tag (`.__tag`), not a `.__variant` string.
    assert!(lua.contains(".__tag"), "expected a `.__tag` int-tag match:\n{lua}");
    // (The non-escaping construct+match SROAs to scalars on the full CLI pipeline --
    // the alloc-zero is gated by benchmarks/value-scalarize/enum_payload_scalarize.rock,
    // not asserted here, since `emit_module` runs a lighter pass set than `rock compile`.)
}

// Cut 3: a cdata enum as a value-struct FIELD flattens into the host cdef
// (`{ int z; Blend blend; }` with `Blend = { int __tag; int _0; }`), so the enum
// is C-layout-storable (NativeBuffer / array element) -- the lever-2 storage win.
const FIELD_SRC: &str = r#"
enum Blend { Normal, Add(int), Mul(int) }
value Cmd { z: int, blend: Blend }

fn apply(c: Cmd) -> int {
    return c.z + match c.blend {
        Blend::Normal => 0,
        Blend::Add(n) => n,
        Blend::Mul(n) => c.z * n,
    }
}

@test(output = "15 30")
fn t() {
    let a = Cmd { z: 5, blend: Blend::Add(10) }
    let b = Cmd { z: 5, blend: Blend::Mul(5) }
    print("{apply(a)} {apply(b)}")
}

fn main() { t() }
"#;

#[test]
fn cdata_enum_as_value_struct_field() {
    let lua = emit_module(FIELD_SRC);
    // The host `Cmd` cdef must exist (it was SKIPPED before -- a payload-enum field
    // disqualified the value layout). The enum field nests `Blend` inline.
    assert!(
        lua.contains("} Cmd;") && lua.contains("Blend blend;"),
        "expected a Cmd cdef nesting the Blend enum field:\n{}",
        lua.split("\nfunction ").next().unwrap_or(&lua)
    );
    require_luajit();
    let mut prog = lua;
    prog.push_str(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         t()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    );
    let path = std::env::temp_dir().join(format!("skull_enum_field_{}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "luajit failed:\n{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "15 30");
}

// Cut 2a: multi-field tuple payloads share positional slots `_0.._{n-1}`. A
// variant with fewer fields pads the rest; the cdef widens to all slots.
const MULTI_SRC: &str = r#"
enum Shape { Circle(int), Rect(int, int), Point }
fn area(s: Shape) -> int {
    return match s {
        Shape::Circle(r) => r * 3,
        Shape::Rect(w, h) => w * h,
        Shape::Point => 0,
    }
}
@test(output = "15 12 0")
fn t() {
    let a = area(Shape::Circle(5))
    let b = area(Shape::Rect(3, 4))
    let c = area(Shape::Point)
    print("{a} {b} {c}")
}
fn main() { t() }
"#;

#[test]
fn cdata_enum_multi_field_tuple() {
    let lua = emit_module(MULTI_SRC);
    assert!(
        lua.contains("int _0; int _1; } Shape;"),
        "expected a 2-slot Shape cdef:\n{}",
        lua.split("\nfunction ").next().unwrap_or(&lua)
    );
    require_luajit();
    let mut prog = lua;
    prog.push_str(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         t()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    );
    let path = std::env::temp_dir().join(format!("skull_enum_multi_{}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "luajit failed:\n{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "15 12 0");
}

// Cut 2b: record payloads -> named slots `_<field>`; a mixed tuple+record enum
// shares both. The baked `__tostring` renders rock's exact format (tuple
// `{ [1] = .. }`, record `{ field = .. }` sorted) -- static compile-time reflection.
const RECORD_SRC: &str = r#"
enum Shape { Circle(int), Box { w: int, h: int }, Point }
fn area(s: Shape) -> int {
    return match s {
        Shape::Circle(r) => r * 3,
        Shape::Box { w, h } => w * h,
        Shape::Point => 0,
    }
}
@test(output = "15 12 0 | Shape::Box { h = 4, w = 3 }")
fn t() {
    let a = area(Shape::Circle(5))
    let b = area(Shape::Box { w: 3, h: 4 })
    let c = area(Shape::Point)
    let d = Shape::Box { w: 3, h: 4 }
    print("{a} {b} {c} | {inspect(d)}")
}
fn main() { t() }
"#;

#[test]
fn cdata_enum_record_payload() {
    let lua = emit_module(RECORD_SRC);
    // Mixed tuple `_0` (Circle) + record `_w`/`_h` (Box) slots in one cdef.
    assert!(
        lua.contains("int _0; int _w; int _h; } Shape;"),
        "expected a mixed tuple+record Shape cdef:\n{}",
        lua.split("\nfunction ").next().unwrap_or(&lua)
    );
    require_luajit();
    let mut prog = lua;
    prog.push_str(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         t()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    );
    let path = std::env::temp_dir().join(format!("skull_enum_rec_{}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "luajit failed:\n{}", String::from_utf8_lossy(&out.stderr));
    // Record fields sorted alphabetically (h before w) -- matches rock's inspect.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "15 12 0 | Shape::Box { h = 4, w = 3 }");
}

// A cdata enum passed through a scalar-ABI call and then matched must NOT be
// repacked into a cdata just to read its tag. The value arrives as leaves
// (tag, _0); `match` should int-compare the tag leaf directly. The copy-prop
// `Field(<Enum>_ct(tag,_0), __tag) -> tag` forwarding folds the repack away (an
// immutable value cdata's field IS its operand), so the callee body has no
// `<Enum>_ct(` ctor -- otherwise every enum passed+matched allocs per call.
const PARAM_MATCH_SRC: &str = r#"
enum Target { Screen, Canvas(int) }
fn route(t: Target) -> int {
    return match t {
        Target::Screen => 0,
        Target::Canvas(id) => 100 + id,
    }
}
@test(output = "0 105")
fn t() {
    let a = route(Target::Screen)
    let b = route(Target::Canvas(5))
    print("{a} {b}")
}
fn main() { t() }
"#;

#[test]
fn matched_scalar_abi_enum_param_does_not_repack() {
    let lua = emit_module(PARAM_MATCH_SRC);
    let body = crate::common::function_body(&lua, "route");
    assert!(
        !body.contains("Target_ct("),
        "a matched scalar-ABI enum param must not repack to a cdata:\n{body}"
    );
}

#[test]
fn cdata_enum_runs_correctly() {
    require_luajit();
    let mut prog = emit_module(SRC);
    prog.push_str(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         t()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    );
    let path = std::env::temp_dir().join(format!("skull_enum_cdata_{}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "luajit failed:\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout.trim(), "1 4 7 4 | 10", "got {stdout:?}");
}
