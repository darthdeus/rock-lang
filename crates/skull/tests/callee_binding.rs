//! The whole-program half of the callee binding: the
//! assembled artifact's second pass and executable incremental rebinding.
//!
//! The mode comes from `emit_full_with_callee_binding`, not the environment:
//! `ROCK_SKULL_CALLEE_BINDING` is read once per PROCESS and these tests share a
//! binary with every other skull test.

use skull::mir_emit::{emit_full_with_callee_binding, emit_overlay};
use skull_codegen_lua::CalleeBindingMode;

use crate::common::{function_body, ws_with_stdlib};

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
    let mid_bind = position("mid_int = __rock_mk_");
    let top_bind = position("top_int = __rock_mk_");
    assert!(
        mid_bind < top_bind,
        "`mid` must bind before `top` captures it"
    );
}

/// The emitted BODY is byte-identical in both shapes: the factory's parameters
/// are named exactly like the globals they replace, so no call site is
/// rewritten and a test that asserts body shape reads the same either way.
/// `common::function_body` reads both, which is what lets the suite run under
/// the switch.
#[test]
fn the_bound_body_is_byte_identical_to_the_unbound_one() {
    let ws = ws_with_stdlib(SRC);
    let mid = skull::user_module_id(&ws).unwrap();
    let global = emit_full_with_callee_binding(&ws, mid, CalleeBindingMode::Global)
        .expect("emit_full")
        .0
        .lua;
    let bound = emit_full_with_callee_binding(&ws, mid, CalleeBindingMode::Upvalue)
        .expect("emit_full")
        .0
        .lua;
    for name in ["mid_int", "top_int"] {
        let unbound_body = function_body(&global, name);
        let bound_body = function_body(&bound, name);
        let inner = unbound_body
            .strip_prefix(&format!("function {name}"))
            .expect("unbound definition")
            .trim_end_matches('\n');
        assert!(
            bound_body.contains(inner),
            "`{name}`'s body changed under binding:\n--- unbound ---\n{unbound_body}\n--- bound ---\n{bound_body}"
        );
    }
}

/// A mutually recursive pair shares one call-graph component, so neither leg is
/// captured -- the condensation rank cannot order them and their closures do
/// not exist yet.
#[test]
fn a_mutually_recursive_pair_stays_a_global_lookup() {
    let lua = emit(SRC);
    let header = lua
        .lines()
        .find(|line| line.starts_with("__rock_mk_is_odd_int = function("))
        .expect("is_odd factory");
    assert!(
        header.contains("leaf_int"),
        "expected the off-component callee bound:\n{header}"
    );
    assert!(
        !header.contains("is_even"),
        "expected the mutually recursive callee left global:\n{header}"
    );
}

/// `MirEmit::emits_main` is the emitter's own answer to "does this artifact
/// define the entry point", and the CLI appends `__rock_main_wrapper(main)` on
/// it. It cannot be recovered from the text: under the bound shape `main`'s
/// body lives in a factory and the string `function main(` appears nowhere, so
/// the caller that used to search for it wrote a chunk that defined the whole
/// game and then ran nothing.
#[test]
fn emits_main_is_reported_under_both_shapes() {
    let ws = ws_with_stdlib(SRC);
    let mid = skull::user_module_id(&ws).unwrap();
    for binding in [CalleeBindingMode::Global, CalleeBindingMode::Upvalue] {
        let emit = emit_full_with_callee_binding(&ws, mid, binding)
            .expect("emit_full")
            .0;
        assert!(emit.emits_main, "`main` is defined, under {binding:?}");
        if binding == CalleeBindingMode::Upvalue {
            assert!(
                !emit.lua.contains("function main("),
                "the bound shape does not spell `function main(` -- which is why \
                 searching the text for it is not the oracle"
            );
        }
    }

    let no_main = ws_with_stdlib("fn leaf(a: int) -> int { a + 1 }\n");
    let mid = skull::user_module_id(&no_main).unwrap();
    let emit = emit_full_with_callee_binding(&no_main, mid, CalleeBindingMode::Upvalue)
        .expect("emit_full")
        .0;
    assert!(!emit.emits_main, "no `main` is defined");
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

fn over_budget_inline_caller_fixture(helper_value: i32) -> String {
    let mut src = format!(
        "@inline @nomangle fn helper(flag: bool) -> int {{ {helper_value} }}\n\
         @nomangle fn caller() -> int {{\n"
    );
    for i in 0..401 {
        src.push_str(&format!("let unused_{i} = {i}\n"));
    }
    src.push_str("return helper(false)\n}\n");
    src
}

#[test]
fn bound_overlay_rebinds_transitive_callers_and_escaped_code_refs() {
    let v1 = format!("{SRC}\n@nomangle fn saved() -> fn(int) -> int {{ top }}\n");
    let ws1 = ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) =
        emit_full_with_callee_binding(&ws1, mid, CalleeBindingMode::Upvalue).expect("full");
    let v2 = v1.replace("a * 3 + 1", "a * 3 + 2");
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "leaf")]).expect("overlay");
    let script = format!(
        "{}\nlocal saved_top = saved()\nassert(top_int(3) == 33)\n{}\nassert(top_int(3) == 36)\nassert(saved_top(3) == 36)\nprint('PASS')\n",
        full.lua, overlay.lua,
    );
    let out = rock_host::lua_runner::run_lua_code_with_timeout(&script, Some(test_utils::HANG_BACKSTOP)).expect("loaded overlay");
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("PASS"), "{}", out.combined());
}

#[test]
fn bound_overlay_rebinds_a_residual_inline_caller_without_rebuilding_its_chunk() {
    let v1 = over_budget_inline_caller_fixture(3);
    let ws1 = ws_with_stdlib(&v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) =
        emit_full_with_callee_binding(&ws1, mid, CalleeBindingMode::Upvalue).expect("full");

    let v2 = over_budget_inline_caller_fixture(5);
    let ws2 = ws_with_stdlib(&v2);
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "helper")]).expect("overlay");
    let stats = cache.repair_stats();
    assert_eq!(stats.chunks_rebuilt, 1, "the residual caller chunk should be reused: {stats:?}");

    let script = format!(
        "{}\nassert(caller() == 3)\n{}\nassert(caller() == 5)\nprint('PASS')\n",
        full.lua, overlay.lua,
    );
    let out = rock_host::lua_runner::run_lua_code_with_timeout(&script, Some(test_utils::HANG_BACKSTOP))
        .expect("loaded residual-caller overlay");
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("PASS"), "{}", out.combined());
}

#[test]
fn bound_overlay_cells_read_the_live_global_storage() {
    let v1 = "global COUNT: int = 0\nfn delta(n: int) -> int { n + 1 }\n@nomangle fn bump() { COUNT = delta(COUNT) }\n";
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) =
        emit_full_with_callee_binding(&ws1, mid, CalleeBindingMode::Upvalue).expect("full");
    let ws2 = ws_with_stdlib(&v1.replace("delta(COUNT)", "delta(COUNT) + 1"));
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "bump")]).expect("overlay");
    let script = format!(
        "{}\nbump()\n{}\nbump()\nassert(COUNT == 3)\nCOUNT = 20\nbump()\nassert(COUNT == 22)\nprint('PASS')\n",
        full.lua, overlay.lua,
    );
    let out = rock_host::lua_runner::run_lua_code_with_timeout(&script, Some(test_utils::HANG_BACKSTOP)).expect("loaded overlay");
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("PASS"), "{}", out.combined());
}

#[test]
fn bound_overlay_new_backedge_keeps_capture_binding_acyclic() {
    let v1 = "fn leaf(n: int) -> int { n }\nfn top(n: int) -> int { leaf(n) }\n@nomangle fn answer() -> int { top(3) }\n";
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) =
        emit_full_with_callee_binding(&ws1, mid, CalleeBindingMode::Upvalue).expect("full");
    let ws2 = ws_with_stdlib(&v1.replace("{ n }", "{ if n == 0 { return 10 }\n top(n - 1) }"));
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "leaf")]).expect("overlay");
    let script = format!("{}\n{}\nassert(answer() == 10)\nprint('PASS')\n", full.lua, overlay.lua);
    let out = rock_host::lua_runner::run_lua_code_with_timeout(&script, Some(test_utils::HANG_BACKSTOP)).expect("loaded overlay");
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("PASS"), "{}", out.combined());
}


#[test]
fn repeated_stripped_overlays_share_one_storage_table_and_old_closures_live() {
    let v1 = "global COUNT: int = 0\n@nomangle fn bump() { COUNT = COUNT + 1 }\n";
    let ws1 = ws_with_stdlib(v1);
    let mid = skull::user_module_id(&ws1).unwrap();
    let (full, mut cache) =
        emit_full_with_callee_binding(&ws1, mid, CalleeBindingMode::Upvalue).expect("full");
    let ws2 = ws_with_stdlib(&v1.replace("COUNT + 1", "COUNT + 2"));
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "bump")]).expect("overlay");
    let script = format!(
        r#"
local function stripped(code)
  return assert(loadstring(string.dump(assert(loadstring(code)), true)))
end
local real_join = debug.upvaluejoin
local joins = 0
debug.upvaluejoin = function(...) joins = joins + 1; return real_join(...) end
stripped({full})()
local old_bump = bump
local storage = __rock_globals
local patch = stripped({overlay})
for i = 1, 25 do patch() end
stripped({full})()
patch()
assert(joins == 0, 'a chunk joined upvalues; every chunk reads the one storage table')
assert(__rock_globals == storage, 'a reload replaced the global storage')
COUNT = 10
old_bump()
bump()
assert(COUNT == 13)
print('PASS')
"#,
        full = skull_codegen_lua::lua_string(&full.lua),
        overlay = skull_codegen_lua::lua_string(&overlay.lua),
    );
    let out = rock_host::lua_runner::run_lua_code_with_timeout(&script, Some(test_utils::HANG_BACKSTOP)).expect("loaded bytecode overlays");
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("PASS"), "{}", out.combined());
}
