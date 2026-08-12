use crate::common::{node, ws_with};
use skull_mir::mir::FuncRef;

fn node_available() -> bool {
    node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_root(src: &str, root: &str) -> String {
    let ws = ws_with(src);
    crate::common::emit_user(&ws, root)
}

fn eval(src: &str, expr: &str, expected: &str) {
    if !node_available() {
        eprintln!("SKIP uint JS execution: node not found");
        return;
    }
    let js = emit_root(src, "root");
    let program = format!(
        "{}{}{}\n{}\nconsole.log({expr});",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
        js,
    );
    let out = node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected, "{program}");
}

fn eval_closure(src: &str, expr: &str, expected: &str) {
    if !node_available() {
        eprintln!("SKIP uint JS execution: node not found");
        return;
    }
    let ws = ws_with(src);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "root")
        .map(|(fid, _)| fid)
        .expect("root");
    let js = skull_codegen_js::emit::emit_with_closure(
        &ws,
        &[(FuncRef { module: mid, func: fid }, "root".to_string())],
    )
    .unwrap_or_else(|e| panic!("closure emit: {}", e.message));
    let program = format!(
        "{}{}{}\n{}\nconsole.log({expr});",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_STRING_RUNTIME,
        js,
    );
    let out = node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected, "{program}");
}

#[test]
fn uint_wraps_exactly_and_orders_unsigned() {
    let src = "
fn root(a: uint, b: uint) -> uint { a + b }";
    eval(src, "root(4294967295, 1)", "0");

    let src = "
fn root(a: uint) -> uint { a * a }";
    eval(src, "root(4294967295)", "1");

    let src = "
fn root(a: uint) -> uint { ~a }";
    eval(src, "root(0)", "4294967295");

    let src = "
fn root(a: uint) -> uint { a >> 31 }";
    eval(src, "root(2147483648)", "1");

    let src = "
fn root(a: uint, b: uint) -> bool { a > b }";
    eval(src, "root(2147483648, 0)", "true");
}

#[test]
fn uint_entry_values_are_normalized() {
    let src = "fn root(a: uint) -> uint { a }";
    eval(src, "root(-1)", "4294967295");
}

#[test]
fn uint_numeric_conversion_differs_from_bitcast() {
    let src = "
fn root(a: uint) -> int { bitcast<int>(a) }";
    eval(src, "root(2147483648)", "-2147483648");
}

#[test]
fn uint_ranges_keep_unsigned_loop_and_materialized_types() {
    let src = "
fn root(lo: uint, hi: uint) -> uint {
    let result: uint = 0
    for i in lo..hi { result = i }
    result
}";
    eval(src, "root(2147483648, 2147483651)", "2147483650");

    let src = "
fn root() -> uint {
    let values: [uint] = 2147483648..2147483651
    values[2]
}";
    eval(src, "root()", "2147483650");
}

#[test]
fn uint_incdec_wraps() {
    let src = "fn root() -> uint { let x: uint = 0; x--; x }";
    eval(src, "root()", "4294967295");

    let src = "fn root() -> uint { let x: uint = 4294967295; x++; x }";
    eval(src, "root()", "0");
}

#[test]
fn uint_dynamic_shift_checks_the_runtime_count() {
    let src = "fn root(value: uint, count: uint) -> uint { value << count }";
    eval(src, "root(1, 31)", "2147483648");
}

#[test]
fn uint_formatting_and_interpolation_preserve_the_unsigned_value() {
    let src = "fn root() -> string { tostring(4294967295 as uint) }";
    eval(src, "root()", "4294967295");

    let src = "fn root() -> string { let value: uint = 4294967295 as uint; \"{value}\" }";
    eval(src, "root()", "4294967295");
}

#[test]
fn uint_survives_generic_value_struct_array_and_nullable_paths() {
    let src = "
value UintRecord { payload: uint }
fn identity<T>(value: T) -> T { return value }
fn root() -> uint {
    let record = UintRecord { payload: 4294967295 as uint }
    let values: [uint] = [4294967295 as uint]
    let maybe: uint? = 4294967295 as uint
    if maybe != null { return identity(record).payload & values[0] }
    return 0 as uint
}";
    eval_closure(src, "root()", "4294967295");
}

#[test]
fn native_buffer_uint_uses_unsigned_storage() {
    let src = "
fn root() -> uint {
    let b = native_buffer_uint(1)
    b[0] = 4294967295
    b[0]
}";
    let js = emit_root(src, "root");
    assert!(js.contains("new Uint32Array"), "uint buffer lost its physical type:\n{js}");
    eval(src, "root()", "4294967295");
}
