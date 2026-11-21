use skull::mir_emit::emit_overlay;

use crate::common::{body_id, bound_callee_name, emit_full_of, function_body, ws_with_stdlib};

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
    emit_full_of(&ws, "emit_full").0.lua
}

fn assert_passes(script: &str) {
    let out =
        rock_host::lua_runner::run_lua_code_with_timeout(script, Some(test_utils::HANG_BACKSTOP))
            .expect("load script");
    assert!(out.success);
    assert!(out.stdout.contains("PASS"));
}

#[test]
fn a_mutually_recursive_pair_stays_a_global_lookup() {
    let lua = emit(SRC);
    let caller = "is_odd";
    let body = function_body(&lua, caller);
    let leaf = bound_callee_name(&lua, caller, "leaf_int");
    assert_ne!(leaf, "leaf_int");
    assert!(body.contains(&format!("{leaf}(")));
    let recursive = bound_callee_name(&lua, caller, "is_even_int");
    assert!(recursive == "is_even_int" && body.contains("is_even_int("));
}

#[test]
fn bound_overlay_cells_read_the_live_global_storage() {
    let v1 = "global COUNT: int = 0\nfn delta(n: int) -> int { n + 1 }\n@nomangle fn bump() { COUNT = delta(COUNT) }\n";
    let ws1 = ws_with_stdlib(v1);
    let (full, mut cache) = emit_full_of(&ws1, "full");
    let ws2 = ws_with_stdlib(&v1.replace("delta(COUNT)", "delta(COUNT) + 1"));
    let overlay = emit_overlay(&ws2, &mut cache, &[body_id(&ws2, "bump")]).expect("overlay");
    let script = format!(
        "{}\nbump()\n{}\nbump()\nassert(COUNT == 3)\nCOUNT = 20\nbump()\nassert(COUNT == 22)\nprint('PASS')\n",
        full.lua, overlay.lua,
    );
    assert_passes(&script);
}
