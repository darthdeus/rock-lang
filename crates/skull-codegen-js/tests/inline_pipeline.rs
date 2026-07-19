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

mod common;
use common::ws_with;

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
    let program = format!("{defs}\nconsole.log({root}());");
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

// SENTINEL (flip when post-inline shared-tail structuring lands, T8c). An
// iterator combinator TERMINAL chain now monomorphizes by inlining -- the reject
// moved off `CallBoundMulti` (proving the inliner runs), but the inlined
// map/filter/reduce lowers to a `while` whose multiple element-skip arms
// converge on a shared latch tail. That shared-branch-tail is the plan's
// explicitly-deferred relooper gap (the 299-bucket) and needs tail-duplication /
// flag-based structuring, not the pipeline share. Pin the honest reject so this
// goes red the moment that structuring makes it emit.
#[test]
fn combinator_terminal_chain_awaits_shared_tail_structuring() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let v = [1, 2, 3]\n\
             v.map(_ * 2).filter(_ > 2).reduce(0, (a, b) => a + b)\n\
         }",
    );
    let err = emit_closure(&ws, "root").expect_err("terminal chain still awaits shared-tail work");
    assert!(
        err.contains("shared branch tail"),
        "expected the inlined chain to reach the shared-tail gap (not the old \
         CallBoundMulti reject), got: {err}"
    );
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
