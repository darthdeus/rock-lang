//! Always-mangle is decl-set-independent: a fn's emitted name is a pure function
//! of its own `(name, signature)`, so it is IDENTICAL with or without a same-name
//! inline(js)/extern(js) backend sibling or an unrelated overload declared
//! elsewhere. This is what makes hot reload safe (see mangle_rename_hot_reload.rs)
//! and what fixed the stone-web arc-2 regression where a web sibling silently
//! renamed every previously-unique Lua definition and call site.
use crate::common::emit_module;

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
        // the bound shape spells a definition `__rock_mk_dbl_int_ret_int =
        // function(`, so a raw `function dbl` scan would report no bare `dbl`
        // for the wrong reason.
        let defs = crate::common::definitions_matching(&lua, "dbl");
        assert!(
            defs.contains(&"dbl_int_ret_int"),
            "[{label}] dbl(int) must emit as `dbl_int_ret_int`, got {defs:?}:\n{lua}"
        );
        assert!(
            !defs.contains(&"dbl") && !lua.contains("dbl__int_ret_int"),
            "[{label}] no bare `dbl` and no `__` in the mangled name, got {defs:?}:\n{lua}"
        );
        // The call site agrees with the definition (same name): the mangled
        // name appears at least twice (the `function` header + the call in
        // `use_it`).
        assert!(
            lua.matches("dbl_int_ret_int").count() >= 2,
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
        lua.contains("pick_int_ret_int") && lua.contains("pick_float_ret_float"),
        "both overload arms must mangle:\n{lua}"
    );
    assert!(
        !lua.contains("pick__int_ret_int") && !lua.contains("pick__float_ret_float"),
        "the mangled names must contain no `__`:\n{lua}"
    );
}
