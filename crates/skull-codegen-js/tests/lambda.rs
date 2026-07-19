//! Closure / lambda emission (docs/multi-backend-testing-plan.md T5). An
//! `Inst::Lambda` renders as a JS arrow `(<params>) => { <body> }` whose body is
//! the lambda's sub-CFG (same arenas); a captured cell renders to the same
//! `c<N>` name it carries in the enclosing scope, so JS lexical capture stands in
//! for the Lua upvalue -- no explicit env record. `Inst::CallValue` is an
//! indirect call `(<callee>)(args)`. The structural pins fix the emitted shape;
//! the node-executed behavioral checks are the correctness oracle.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;
use std::process::Command;

mod common;
use common::{emit_user, ws_with};

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
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run `<defs>; console.log(<call>)` under node and parse the printed int.
fn node_eval_int(defs: &str, call: &str) -> i64 {
    let program = format!("{defs}\nconsole.log({call});");
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
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
    let scx = ScalarCtx::new(ws);
    let r = test_fn_ref(ws, root);
    skull_codegen_js::emit::emit_with_closure(ws, &scx, &[(r, root.to_string())])
        .unwrap_or_else(|e| panic!("closure emit `{root}`: {}", e.message))
}

// --- structural pins ---

// `CallValue`: an indirect call through a fn-typed param is `(callee)(args)`.
#[test]
fn callvalue_through_fn_param() {
    let ws = ws_with("fn apply(f: (int) -> int, x: int) -> int { f(x) }");
    assert_eq!(
        emit_user(&ws, "apply"),
        "\
function apply(c0, c1) {
  let v0 = c0;
  let v1 = c1;
  let v2 = (v0)(v1);
  return v2;
}
"
    );
}

// A capturing lambda: `c0` (the enclosing param `n`) is read inside the arrow
// under its own name -- lexical capture, no env record. The arrow body declares
// no cells (it only reads).
#[test]
fn capturing_arrow_reads_enclosing_cell() {
    let ws = ws_with(
        "fn capture(n: int) -> int {\n\
             let g = (x: int) => x + n\n\
             g(10)\n\
         }",
    );
    assert_eq!(
        emit_user(&ws, "capture"),
        "\
function capture(c0) {
  let c2;
  let v4 = ((c1) => {
    let v0 = c1;
    let v1 = c0;
    return (v0 + v1);
  });
  c2 = v4;
  let v6 = c2;
  let v8 = (v6)(10);
  return v8;
}
"
    );
}

// A capture-free lambda is hoisted to a single module-level `const` (luajit P3
// parity): same-site references share one value, so `==` on a fn-valued field
// holds. A capturing lambda stays an inline arrow (fresh per call).
#[test]
fn capture_free_lambda_is_hoisted() {
    let ws = ws_with("fn mk() -> fn() -> int { () => 42 }");
    let js = emit_user(&ws, "mk");
    assert!(
        js.starts_with("const mk__lam"),
        "capture-free lambda hoisted to a module const:\n{js}"
    );
    assert!(js.contains("() => {") && js.contains("return 42;"), "hoisted body:\n{js}");
    // The fn body references the shared hoisted value, not a fresh inline arrow.
    assert!(js.contains("(mk__lam"), "references the shared value:\n{js}");
    assert_eq!(js.matches("=>").count(), 1, "exactly one arrow (the hoisted def):\n{js}");
}

// --- behavioral (node) oracle ---

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
    assert_eq!(node_eval_int(&emit_user(&ws, "capture"), "capture(7)"), 17);
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
    assert_eq!(node_eval_int(&emit_user(&ws, "twice"), "twice()"), 25);
}

// Capturing + MUTATING an enclosing cell: `sum` is owned by the enclosing scope
// (stored there too), so a single JS binding is shared -- the arrow's writes
// accumulate into it (upvalue mutation).
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
    assert_eq!(node_eval_int(&emit_user(&ws, "accum"), "accum()"), 7);
}
