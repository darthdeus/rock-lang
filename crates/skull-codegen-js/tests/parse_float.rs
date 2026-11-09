//! parse_float emission (T8d). `string.parse_float` -> `__parse_float`, a strict
//! whole-string numeric parse reproducing Lua `tonumber` (empty/whitespace ->
//! null, non-numeric tail -> null, else the parsed double). The node-executed
//! checks are the correctness oracle; `tests/strings/js_parse_float.rock` cross-checks
//! against luajit.

use crate::common::ws_with;

crate::runtime_eval!(eval_str: JS_STRING_RUNTIME);

// A valid numeric string parses to its double.
#[test]
fn parses_valid_number() {
    let src = "fn root() -> string { let r = \"1.5\".parse_float(); if r == null { \"nil\" } else { tostring(r) } }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("parse_float emits");
    assert!(
        js.contains("__parse_float"),
        "renders the strict parse:\n{js}"
    );
    eval_str(&js, "root", "1.5");
}

// A non-numeric tail is nil (NaN -> null), NOT a lenient prefix parse.
#[test]
fn non_numeric_is_null() {
    let src = "fn root() -> string { let r = \"12abc\".parse_float(); if r == null { \"nil\" } else { tostring(r) } }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("parse_float emits");
    eval_str(&js, "root", "nil");
}

// The empty string is nil (Lua tonumber), NOT 0 (JS Number).
#[test]
fn empty_is_null_not_zero() {
    let src = "fn root() -> string { let r = \"\".parse_float(); if r == null { \"nil\" } else { tostring(r) } }";
    let ws = ws_with(src);
    let js = crate::common::emit_closure(&ws, "root").expect("parse_float emits");
    eval_str(&js, "root", "nil");
}
