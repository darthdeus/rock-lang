use crate::common::{emit_module, mangled_name, run_luajit};

fn body<'a>(lua: &'a str, name: &str) -> &'a str {
    let name = mangled_name(lua, name);
    let start = lua.find(&format!("function {name}(")).expect("function");
    let rest = &lua[start..];
    &rest[..rest.find("\nend").expect("function end")]
}

#[test]
fn direct_tail_calls_preserve_results_and_effect_order() {
    let lua = emit_module(r#"
struct Output { value: int }
fn write(out: Output, x: int) { out.value = x }
fn value(x: int) -> int { return x + 1 }
fn forward_unit(out: Output, x: int) { write(out, x) }
fn forward_scalar(x: int) -> int { return value(x) }
fn discard_scalar(x: int) { value(x) }
fn work_after_call(out: Output, x: int) { write(out, x); out.value += 1 }
fn write_pair(out: Output, x: int, y: int) { out.value = x * 100 + y }
fn computed_unit(out: Output, x: int) {
    let a = x + 1
    let b = x + 2
    write_pair(out, a * a, b * b)
}
fn inspect_result(x: int) -> int { return value(x) + 1 }
inline(lua) fn raw_scalar(x: int) -> int = "return x, x + 1"
inline(lua) fn raw_unit(x: int) = "return x"
fn foreign_scalar(x: int) -> int { return raw_scalar(x) }
fn foreign_unit(x: int) { raw_unit(x) }
fn main() {
    let out = Output { value: 0 }
    forward_unit(out, 41)
    print("{out.value}")
    print("{forward_scalar(41)}")
    work_after_call(out, 41)
    print("{out.value}")
    print("{inspect_result(41)}")
    computed_unit(out, 10)
    print("{out.value}")
    discard_scalar(41)
}
"#);
    for name in ["forward_unit", "forward_scalar"] {
        assert!(body(&lua, name).lines().any(|line| {
            line.trim_start().starts_with("do return ") && line.contains('(')
        }), "{name} retains an unnecessary caller frame:\n{}", body(&lua, name));
    }
    for name in ["discard_scalar", "work_after_call", "inspect_result"] {
        assert!(!body(&lua, name).lines().any(|line| {
            line.trim_start().starts_with("do return ")
                && (line.contains("write_") || line.contains("value_"))
        }), "{name} changed return arity or skipped subsequent work");
    }
    let discard = mangled_name(&lua, "discard_scalar");
    let foreign_scalar = mangled_name(&lua, "foreign_scalar");
    let foreign_unit = mangled_name(&lua, "foreign_unit");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\nprint(select('#', {discard}(1)))\nprint(select('#', {foreign_scalar}(1)))\nprint(select('#', {foreign_unit}(1)))\n"), "direct_tail_calls"),
        "41\n42\n42\n43\n12244\n0\n1\n0");
}
