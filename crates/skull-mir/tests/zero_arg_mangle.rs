//! A mangled name never equals its source name (ZERO_ARG_MANGLING):
//! a non-exempt function with no parameters
//! carries the `_0` arity marker, so Lua's `_G[name]` cannot reach it by its
//! declared name. Exempt surface names stay bare.

use crate::common::ws_of;
use skull_mir::mangle::{collision_errors, mangle_fn};
use ustr::Ustr;

fn mangled(ws: &bone_core::workspace::Workspace, name: &str) -> Vec<String> {
    let mid = ws.module_ids[&Ustr::from("<test>")];
    let found: Vec<String> = ws
        .module(mid)
        .functions
        .iter()
        .filter(|(_, f)| f.name.as_str() == name)
        .map(|(_, f)| mangle_fn(ws, f))
        .collect();
    assert!(!found.is_empty(), "no fn `{name}`");
    found
}

#[test]
fn zero_parameter_symbol_does_not_collide_with_restore_lookup_name() {
    let ws = ws_of("fn hidden_callback() {}");
    let mid = ws.module_ids[&Ustr::from("<test>")];
    let (_, callback) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "hidden_callback")
        .expect("restore callback declaration");
    assert_ne!(mangle_fn(&ws, callback), callback.name.as_str());
}

#[test]
fn zero_parameter_function_gets_the_arity_marker() {
    let ws = ws_of(
        r#"
fn foo() -> int { return 1 }
fn bar(x: int) -> int { return x }
"#,
    );
    assert_eq!(mangled(&ws, "foo"), ["foo_0"]);
    assert_eq!(mangled(&ws, "bar"), ["bar_int"]);
}

#[test]
fn exempt_zero_parameter_functions_keep_their_names() {
    let ws = ws_of(
        r#"
@nomangle fn kept() {}
@test fn decorated() {}
fn test_entry() {}
extern(lua) fn clock() -> float
fn main() {}
"#,
    );
    for name in ["kept", "decorated", "test_entry", "clock", "main"] {
        assert_eq!(mangled(&ws, name), [name]);
    }
}

#[test]
fn mangled_marker_clashing_with_exempt_bare_name_is_rejected() {
    let ws = ws_of(
        r#"
fn foo() -> int { return 1 }
@nomangle fn foo_0() -> int { return 2 }
"#,
    );
    let errors = collision_errors(&ws);
    assert_eq!(
        errors.len(),
        1,
        "{:?}",
        errors
            .iter()
            .map(|(n, e)| (n, &e.message))
            .collect::<Vec<_>>()
    );
    assert_eq!(errors[0].0, "foo_0");
}
