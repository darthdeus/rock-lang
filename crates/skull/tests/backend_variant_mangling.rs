//! Always-mangle is decl-set-independent: a fn's emitted name is a pure function
//! of its own `(name, signature)`, so it is IDENTICAL with or without a same-name
//! inline(js)/extern(js) backend sibling or an unrelated overload declared
//! elsewhere. This is what makes hot reload safe (see mangle_rename_hot_reload.rs)
//! and what fixed the stone-web arc-2 regression where a web sibling silently
//! renamed every previously-unique Lua definition and call site.
use crate::common::emit_module;

#[test]
fn return_only_overloads_in_separate_modules_are_rejected() {
    let ws = crate::common::ws_with_stdlib_module(
        "fn return_pair(x: int) -> int { return x }",
        "fn return_pair(x: int) -> string { return \"value\" }",
    );
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let result = skull::mir_emit::emit_workspace_mir(&ws, mid);
    let error = match result {
        Ok(_) => panic!("return-only overloads compiled successfully"),
        Err(error) => error,
    };
    assert!(error.message.contains("differ only in return type"), "{}", error.message);
    assert!(error.message.contains("return_pair"), "{}", error.message);
    let files: Vec<_> = error.labels.iter().map(|label| label.span.file.as_str()).collect();
    assert!(files.contains(&"<test>") && files.contains(&"<stdlib:probe>"), "{files:?}");
}

#[test]
fn function_parameter_return_types_keep_distinct_symbols() {
    let lua = emit_module(
        r#"
fn parameter_pair(g: fn(int) -> int) -> int { return g(1) }
fn parameter_pair(g: fn(int) -> string) -> string { return g(1) }
"#,
    );
    let defs = crate::common::definitions_matching(&lua, "parameter_pair");
    assert_eq!(defs.len(), 2, "{defs:?}");
    assert!(defs.iter().any(|name| name.contains("fn_int_ret_int")), "{defs:?}");
    assert!(defs.iter().any(|name| name.contains("fn_int_ret_string")), "{defs:?}");
}

#[test]
fn no_argument_backend_sibling_keeps_one_logical_function() {
    let lua = emit_module(
        r#"
fn portable_now() -> float { return 1.0 }
extern(js) fn portable_now() -> float
fn read_clock() -> float { return portable_now() }
"#,
    );
    assert_eq!(crate::common::definitions_matching(&lua, "portable_now").len(), 1);
}

const DBL_ALONE: &str = r#"
fn dbl(x: int) -> int { return x + x }
fn use_it() -> int { return dbl(21) }
"#;

const DBL_WITH_JS_SIBLING: &str = r#"
fn dbl(x: int) -> int { return x + x }
inline(js) fn dbl(x: int) -> int = """return x * 3"""
fn use_it() -> int { return dbl(21) }
"#;

const DBL_WITH_OVERLOAD: &str = r#"
fn dbl(x: int) -> int { return x + x }
fn dbl(s: string) -> string { return s }
fn use_it() -> int { return dbl(21) }
"#;

/// The `dbl(int)` Lua definition mangles to the SAME name regardless of a js
/// sibling or an unrelated overload -- and it always mangles (no bare `function
/// dbl(`), because every non-exempt fn mangles by signature.
#[test]
fn dbl_int_name_is_decl_set_independent() {
    for (label, src) in [
        ("alone", DBL_ALONE),
        ("js sibling", DBL_WITH_JS_SIBLING),
        ("overload elsewhere", DBL_WITH_OVERLOAD),
    ] {
        let lua = emit_module(src);
        // Read definition HEADERS, in whichever callee shape the build emits:
        // the bound shape spells a definition `__rock_mk_dbl_int =
        // function(`, so a raw `function dbl` scan would report no bare `dbl`
        // for the wrong reason.
        let defs = crate::common::definitions_matching(&lua, "dbl");
        assert!(
            defs.contains(&"dbl_int"),
            "[{label}] dbl(int) must emit as `dbl_int`, got {defs:?}:\n{lua}"
        );
        assert!(
            !defs.contains(&"dbl") && !lua.contains("dbl__int"),
            "[{label}] no bare `dbl` and no `__` in the mangled name, got {defs:?}:\n{lua}"
        );
        // The call site agrees with the definition (same name): the mangled
        // name appears at least twice (the `function` header + the call in
        // `use_it`).
        assert!(
            lua.matches("dbl_int").count() >= 2,
            "[{label}] the call site must target the same mangled name as the definition:\n{lua}"
        );
    }
}

/// A real (Lua-emittable) overload mangles both arms under the single-`_` scheme,
/// with no `__` anywhere in the produced identifiers.
#[test]
fn real_lua_overload_mangles_both_arms() {
    let lua = emit_module(
        r#"
fn pick(x: int) -> int { return x }
fn pick(x: float) -> float { return x }

fn use_both() -> float { return float(pick(1)) + pick(2.0) }
"#,
    );
    assert!(
        lua.contains("pick_int") && lua.contains("pick_float"),
        "both overload arms must mangle:\n{lua}"
    );
    assert!(
        !lua.contains("pick__int") && !lua.contains("pick__float"),
        "the mangled names must contain no `__`:\n{lua}"
    );
}
