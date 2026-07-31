//! Shared transform-pipeline emission (T8a). `emit_with_closure` now runs the
//! same per-body sequence the luajit path runs (lower -> inline -> cleanup ->
//! recover_counted_loops -> residual-bound-call validation), so an `@inline`/
//! `@pure` callee is SPLICED (no standalone co-emission), an iterator combinator
//! chain monomorphizes by inlining (was a `CallBoundMulti` reject), and a call
//! omitting a defaulted param is a legal shorter shape (was the arity-guard
//! false positive). The node-executed value checks are the correctness oracle.

use std::process::Command;

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;
use crate::common::ws_with;

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Co-emit `root` + its transitive callee closure through the shared pipeline.
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

/// Evaluate `<root>()` under node over the emitted defs (skips if node absent).
fn eval_int(defs: &str, root: &str, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let program = format!(
        "{}{}{}{}{}\n{defs}\nconsole.log({root}());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_CALL_HELPER,
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

// A call to an `@inline` fn is SPLICED into the caller: no standalone `function
// dbl` is co-emitted, and the run is correct.
#[test]
fn inline_callee_is_spliced() {
    let ws = ws_with("@inline fn dbl(x: int) -> int { x * 2 }\nfn root() -> int { dbl(3) }");
    let js = emit_closure(&ws, "root").expect("emits");
    assert!(!js.contains("function dbl"), "@inline callee should be spliced, not co-emitted:\n{js}");
    eval_int(&js, "root", 6);
}

// A `@pure` fn call is likewise spliced (both `@inline` and `@pure` force inline).
#[test]
fn pure_callee_is_spliced() {
    let ws = ws_with("@pure fn sq(x: int) -> int { x * x }\nfn root() -> int { sq(3) }");
    let js = emit_closure(&ws, "root").expect("emits");
    assert!(!js.contains("function sq"), "@pure callee should be spliced, not co-emitted:\n{js}");
    eval_int(&js, "root", 9);
}

// An iterator combinator TERMINAL chain monomorphizes by inlining, and the
// resulting nested loops (the inlined `next()` skip-loop continuing the outer
// for-loop) now structure via labeled loops (T8c Class B). Was the sentinel
// `combinator_terminal_chain_awaits_shared_tail_structuring` pinning the honest
// shared-branch-tail reject; flipped when the structuring landed.
#[test]
fn combinator_terminal_chain_structures() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let v = [1, 2, 3]\n\
             v.map(_ * 2).filter(_ > 2).reduce(0, (a, b) => a + b)\n\
         }",
    );
    let js = emit_closure(&ws, "root").expect("inlined combinator chain structures via labeled loops");
    eval_int(&js, "root", 10);
}

// A call omitting a defaulted param is a legal 2-arg call to a 3-param callee --
// pre-T8a the arity guard false-positived it as a value-struct ABI mismatch.
#[test]
fn omitted_default_arg_emits() {
    let ws = ws_with(
        "fn ranged(a: int, b: int, step: int = 1) -> int { a + b + step }\n\
         fn root() -> int { ranged(2, 3) }",
    );
    let js = emit_closure(&ws, "root").expect("omitted default should emit");
    eval_int(&js, "root", 6);
}
