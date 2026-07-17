//! String-value emitter tests (docs/multi-backend-testing-plan.md T2, Stage 1):
//! string literals with JS escaping, `+` concat routed through `lua_tostring`
//! (Lua `..` number-formatting parity), and the scalar interpolation holes
//! (`ToString` for int/bool/string, the `__rock_format_float` host port for
//! floats). The emitted expressions are node-verified end to end by the suite
//! runner; these pin the shape.

mod common;
use common::ws_with;

fn emit(ws: &bone_semantic::inference::Workspace, name: &str) -> String {
    let mf = common::lower_test_fn(ws, name);
    skull_codegen_js::emit::emit_fn(ws, &mf, name)
        .unwrap_or_else(|e| panic!("emit `{name}`: {}", e.message))
}

#[test]
fn string_literal_escapes() {
    let ws = ws_with(r#"fn s() -> string { return "a\"b\\c\n\td" }"#);
    assert_eq!(
        emit(&ws, "s"),
        "\
function s() {
  return \"a\\\"b\\\\c\\n\\td\";
}
"
    );
}

#[test]
fn string_concat_routes_through_lua_tostring() {
    let ws = ws_with(r#"fn c() -> string { return "a" + "b" }"#);
    assert_eq!(
        emit(&ws, "c"),
        "\
function c() {
  return (lua_tostring(\"a\") + lua_tostring(\"b\"));
}
"
    );
}

#[test]
fn int_interpolation_uses_lua_tostring() {
    let ws = ws_with(r#"fn i() -> string { let x = 7 return "v={x}" }"#);
    // `{x}` (int) lowers to a `ToString` inst -> lua_tostring; the literal
    // fragments are concat operands, also wrapped.
    assert_eq!(
        emit(&ws, "i"),
        "\
function i() {
  let c0;
  c0 = 7;
  let v3 = c0;
  return (lua_tostring(\"v=\") + lua_tostring(lua_tostring(v3)));
}
"
    );
}

#[test]
fn float_interpolation_uses_format_float_port() {
    let ws = ws_with(r#"fn f() -> string { let x = 1.5 return "v={x}" }"#);
    // `{x}` (float) lowers to a call of the `__rock_format_float` host helper.
    assert_eq!(
        emit(&ws, "f"),
        "\
function f() {
  let c0;
  c0 = 1.5;
  let v3 = c0;
  let v4 = __rock_format_float(v3);
  return (lua_tostring(\"v=\") + lua_tostring(v4));
}
"
    );
}
