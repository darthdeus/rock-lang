//! Closure / lambda emission. Closure
//! conversion lowers each lambda to a hoisted sibling plus either a reusable
//! CodeRef or a CodeRef-bearing environment record. `Inst::CallValue` is an
//! indirect dispatch through `__rock_call`; direct calls retain their physical
//! callee. Structural pins fix the emitted shape; node checks are the behavior
//! oracle.

use skull_mir::mir::FuncRef;
use crate::common::{emit_user, ws_with};

fn test_fn_ref(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == name {
            return FuncRef { module: mid, func: fid };
        }
    }
    panic!("no fn `{name}` in <test> module");
}

fn node_available() -> bool {
    crate::common::node().arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run `<defs>; console.log(<call>)` under node and parse the printed int. The
/// `__rock_call` dispatch helper is prepended (every indirect call routes
/// through it after `convert_closures`).
fn node_eval_int(defs: &str, call: &str) -> i64 {
    let program =
        format!("{}{defs}\nconsole.log({call});", skull_codegen_js::JS_CALL_HELPER);
    let out = crate::common::node().arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<i64>().unwrap_or_else(|_| panic!("node did not print an int: {s:?}\n{program}"))
}

/// Co-emit `root` and its plain-Rock callees into one JS program.
fn emit_closure(ws: &bone_semantic::inference::Workspace, root: &str) -> String {
    let r = test_fn_ref(ws, root);
    skull_codegen_js::emit::emit_with_closure(ws, &[(r, root.to_string())])
        .unwrap_or_else(|e| panic!("closure emit `{root}`: {}", e.message))
}

// --- structural pins (post-`convert_closures`: records + hoisted siblings) ---

// `CallValue`: an indirect call through a fn-typed param routes through the
// `__rock_call` dispatch helper (handles both a raw fn and an env record).
#[test]
fn callvalue_through_fn_param() {
    let ws = ws_with("fn apply(f: (int) -> int, x: int) -> int { f(x) }");
    let js = emit_user(&ws, "apply");
    assert!(js.contains("__rock_call(v0, v1)"), "indirect call via __rock_call:\n{js}");
}

#[test]
fn direct_calls_remain_direct_while_values_use_slots() {
    let ws = ws_with(
        "fn target(x: int) -> int { x + 1 }\n\
         fn direct() -> int { target(1) }\n\
         fn main() { direct() }\n",
    );
    let js = emit_closure(&ws, "main");
    let start = js.find("function direct").expect("direct body");
    let body = js[start..].split("\n}\n").next().expect("direct body");
    assert!(body.contains("target_"), "direct call keeps its physical call: {body}");
    assert!(!body.contains("__rock_code_slot_ref"), "direct call must not route through a CodeRef: {body}");
}

// A capturing lambda becomes an env record `{ __slot, c<A>: .. }` at its
// creation site plus a hoisted sibling fn reading the capture from its env.
#[test]
fn capturing_lambda_builds_env_record() {
    let ws = ws_with(
        "fn capture(n: int) -> int {\n\
             let g = (x: int) => x + n\n\
             g(10)\n\
         }",
    );
    let js = emit_closure(&ws, "capture");
    assert!(js.contains("__slot: __rock_code_slot_ref("), "creation site builds a CodeRef record:\n{js}");
    assert!(!js.contains("__fn: capture__lambda0"), "Rock closures must not freeze a backend body:\n{js}");
    assert!(js.contains("function capture__lambda0("), "hoisted sibling body:\n{js}");
}

// A capture-free lambda resolves one reusable slot (no env allocation):
// same-site references share one callable identity.
#[test]
fn capture_free_lambda_is_hoisted() {
    let ws = ws_with("fn mk() -> fn() -> int { () => 42 }");
    let js = emit_closure(&ws, "mk");
    assert!(js.contains("function mk__lambda0("), "hoisted top-level fn:\n{js}");
    assert!(js.contains("return 42;"), "hoisted body:\n{js}");
    assert!(js.contains("__rock_code_slot_ref("), "capture-free values use the reusable slot:\n{js}");
    assert!(!js.contains("__fn: mk__lambda0"), "no frozen backend body for a capture-free lambda:\n{js}");
}

// --- behavioral (node) oracle: emit through the full pipeline (the pass runs
// only in `emit_with_closure`, not the raw `emit_fn`). ---

#[test]
fn capturing_arrow_executes() {
    if !node_available() {
        eprintln!("SKIP capturing_arrow_executes: node not found");
        return;
    }
    let ws = ws_with(
        "fn capture(n: int) -> int {\n\
             let g = (x: int) => x + n\n\
             g(10)\n\
         }",
    );
    assert_eq!(node_eval_int(&emit_closure(&ws, "capture"), "capture(7)"), 17);
}

#[test]
fn hof_receives_and_calls_lambda() {
    if !node_available() {
        eprintln!("SKIP hof_receives_and_calls_lambda: node not found");
        return;
    }
    let ws = ws_with(
        "fn apply(f: (int) -> int, x: int) -> int { f(x) }\n\
         fn root() -> int { apply(y => y + 1, 5) }",
    );
    assert_eq!(node_eval_int(&emit_closure(&ws, "root"), "root()"), 6);
}

// A lambda called more than once with different args: each call is a fresh
// invocation (no shared param state across calls).
#[test]
fn lambda_reentrant_across_calls() {
    if !node_available() {
        eprintln!("SKIP lambda_reentrant_across_calls: node not found");
        return;
    }
    let ws = ws_with(
        "fn twice() -> int {\n\
             let f = (x: int) => x * x\n\
             f(3) + f(4)\n\
         }",
    );
    assert_eq!(node_eval_int(&emit_closure(&ws, "twice"), "twice()"), 25);
}

// Capturing + MUTATING an enclosing cell: `sum` is a shared box, so the closure's
// writes accumulate into the one binding the enclosing scope also reads.
#[test]
fn lambda_mutates_captured_cell() {
    if !node_available() {
        eprintln!("SKIP lambda_mutates_captured_cell: node not found");
        return;
    }
    let ws = ws_with(
        "fn accum() -> int {\n\
             let sum = 0\n\
             let add = (x: int) => { sum = sum + x }\n\
             add(3)\n\
             add(4)\n\
             sum\n\
         }",
    );
    assert_eq!(node_eval_int(&emit_closure(&ws, "accum"), "accum()"), 7);
}
