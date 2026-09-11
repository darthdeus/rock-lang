//! The whole-program half of the callee binding (`docs/backends.md`): the
//! assembled artifact's SECOND PASS, and the hot-reload rule that keeps the
//! shape out of the incremental overlay.
//!
//! The mode comes from `emit_full_with_callee_binding`, not the environment:
//! `ROCK_SKULL_CALLEE_BINDING` is read once per PROCESS and these tests share a
//! binary with every other skull test.

use skull::mir_emit::{emit_full_with_callee_binding, emit_overlay};
use skull_codegen_lua::CalleeBindingMode;

use crate::common::ws_with_stdlib;

/// A program whose call graph has a chain (`top` -> `mid` -> `leaf`), a
/// mutually recursive pair, and a leaf with no callees at all.
const SRC: &str = "\
fn leaf(a: int) -> int { a * 3 + 1 }
fn mid(a: int) -> int { leaf(a) + leaf(a + 1) }
fn top(a: int) -> int { mid(a) + leaf(a) }
fn is_even(n: int) -> bool { if n == 0 { return true }\n  is_odd(n - 1) }
fn is_odd(n: int) -> bool { if n == 0 { return leaf(n) == 1 }\n  is_even(n - 1) }
fn main() { let _ = top(3) }
";

fn emit(src: &str) -> String {
    let ws = ws_with_stdlib(src);
    let mid = skull::user_module_id(&ws).unwrap();
    emit_full_with_callee_binding(&ws, mid, CalleeBindingMode::Upvalue)
        .expect("emit_full")
        .0
        .lua
}

/// The byte offset of `needle`, which must occur exactly once.
fn only(lua: &str, needle: &str) -> usize {
    let first = lua.find(needle).unwrap_or_else(|| panic!("missing `{needle}`"));
    assert!(
        lua[first + needle.len()..].find(needle).is_none(),
        "`{needle}` occurs more than once"
    );
    first
}

/// Every factory gets exactly one binding line, and EVERY binding line follows
/// EVERY definition: a factory holds the only copy of its body, so a capture
/// read before its own binding line ran would be main.lua's `nil`.
#[test]
fn every_binding_line_follows_every_definition() {
    let lua = emit(SRC);
    let factories: Vec<&str> = lua
        .lines()
        .filter_map(|line| line.strip_prefix("__rock_mk_"))
        .filter_map(|rest| rest.split_once(" = function(").map(|(name, _)| name))
        .collect();
    assert!(
        factories.iter().any(|n| n.starts_with("top_")) && factories.iter().any(|n| n.starts_with("mid_")),
        "expected `top` and `mid` bound: {factories:?}"
    );
    let last_definition = lua
        .match_indices("\n__rock_mk_")
        .last()
        .map(|(i, _)| i)
        .expect("at least one factory");
    for name in &factories {
        let bind = only(&lua, &format!("\n{name} = __rock_mk_{name}("));
        assert!(
            bind > last_definition,
            "`{name}`'s binding line precedes a factory definition"
        );
    }
}

/// The binding pass runs callees before callers: `mid` captures `leaf`, and
/// `top` captures both, so each must already hold its final closure.
#[test]
fn binding_lines_are_ordered_callees_before_callers() {
    let lua = emit(SRC);
    let position = |prefix: &str| -> usize {
        let at = lua
            .match_indices(&format!("\n{prefix}"))
            .map(|(i, _)| i)
            .find(|&i| lua[i + 1..].starts_with(prefix) && lua[i + 1..].contains(" = __rock_mk_"))
            .unwrap_or_else(|| panic!("no binding line for `{prefix}`"));
        at
    };
    let mid_bind = position("mid_int_ret_int = __rock_mk_");
    let top_bind = position("top_int_ret_int = __rock_mk_");
    assert!(
        mid_bind < top_bind,
        "`mid` must bind before `top` captures it"
    );
}

/// A mutually recursive pair shares one call-graph component, so neither leg is
/// captured -- the condensation rank cannot order them and their closures do
/// not exist yet.
#[test]
fn a_mutually_recursive_pair_stays_a_global_lookup() {
    let lua = emit(SRC);
    let header = lua
        .lines()
        .find(|line| line.starts_with("__rock_mk_is_odd_int_ret_bool = function("))
        .expect("is_odd factory");
    assert!(
        header.contains("leaf_int_ret_int"),
        "expected the off-component callee bound:\n{header}"
    );
    assert!(
        !header.contains("is_even"),
        "expected the mutually recursive callee left global:\n{header}"
    );
}

/// THE hot-reload rule. An incremental overlay redefines one global function;
/// a caller that captured it as an immutable upvalue holds the previous closure
/// and would never see the edited body. Emitting the overlay anyway would make
/// a body edit SILENTLY not take effect, so the overlay rejects instead.
/// `rock stone` avoids it by pinning the global-lookup shape for the session
/// (`skull::force_global_callee_binding`).
#[test]
fn the_incremental_overlay_rejects_a_bound_body() {
    let v1 = SRC;
    let v2 = SRC.replace("a * 3 + 1", "a * 3 + 2");
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (_full, mut cache) =
        emit_full_with_callee_binding(&ws1, mid, CalleeBindingMode::Upvalue).expect("emit_full");
    let ws2 = ws_with_stdlib(&v2);
    let edited = body_id(&ws2, "mid");
    let Err(err) = emit_overlay(&ws2, &mut cache, &[edited]) else {
        panic!("a bound body must not reach the overlay");
    };
    assert!(
        err.message.contains("incremental overlay cannot redefine")
            && err.message.contains("ROCK_SKULL_CALLEE_BINDING=global"),
        "expected the hot-reload rule's diagnostic, got: {}",
        err.message
    );
}

fn body_id(
    ws: &bone_semantic::inference::Workspace,
    name: &str,
) -> (bone_semantic::prelude::ModuleId, bone_semantic::hir::BodyOwner) {
    for module in ws.modules.iter() {
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() == name {
                return (module.id, bone_semantic::hir::BodyOwner::Function(fid));
            }
        }
    }
    panic!("no fn named {name}");
}
