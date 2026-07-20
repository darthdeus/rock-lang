//! Module-global emission (T8d): module-level `let`/`global` state renders into
//! a scoped `__G` object (`Inst::Global` read -> `__G.<name>`, `StoreGlobal`
//! write -> `__G.<name> = ...`), with each global's initializer emitted once at
//! load in transitive-dependency order; a top-level fn referenced as a VALUE
//! (`Inst::Global` naming a function) co-emits and renders as its JS name. Node
//! execution is the correctness oracle; `tests/js_module_globals.rock`
//! cross-checks against luajit.

use std::process::Command;

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

mod common;
use common::ws_with;

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> Result<String, String> {
    let scx = ScalarCtx::new(ws);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let mut r = None;
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == root {
            r = Some(FuncRef { module: mid, func: fid });
        }
    }
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r.unwrap(), root.to_string())])
        .map_err(|e| e.message)
}

fn eval_int(defs: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    // `__G` is the scoped module-globals object the emitter targets; the run
    // harness (js_test.rs) supplies it in its preamble.
    let program = format!(
        "{}{}{}{}\nvar __G = {{}};\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRING_RUNTIME,
    );
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let got: i64 = s.trim().parse().unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result\n{program}");
}

// A const module global read renders `__G.CANS` and inits once at load.
#[test]
fn const_global_read() {
    let src = "const CANS: int = 7\nfn root() -> int { CANS * 3 }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("const global emits");
    assert!(js.contains("__G.CANS"), "scoped global read:\n{js}");
    eval_int(&js, "root", 21);
}

// Mutable state: StoreGlobal + Global read through a call, reset per test.
#[test]
fn mutable_global_state() {
    let src = "global COUNTER: int = 0\n\
               fn bump() -> int { COUNTER = COUNTER + 1; return COUNTER }\n\
               fn root() -> int { COUNTER = 0; bump(); bump(); return bump() }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("mutable global emits");
    assert!(js.contains("__G.COUNTER = "), "StoreGlobal renders:\n{js}");
    eval_int(&js, "root", 3);
}

// A global initialized from another global -- init order must place BASE first.
#[test]
fn derived_global_init_order() {
    let src = "global BASE: int = 100\n\
               global DERIVED: int = BASE + 5\n\
               fn root() -> int { DERIVED }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("derived global emits");
    eval_int(&js, "root", 105);
}

// A top-level fn referenced as a value: Inst::Global naming a function.
#[test]
fn fn_as_value() {
    let src = "fn add_one(x: int) -> int { x + 1 }\n\
               fn root() -> int { let f = add_one; f(5) }";
    let ws = ws_with(src);
    let js = emit_closure(&ws, "root").expect("fn-as-value emits");
    eval_int(&js, "root", 6);
}
