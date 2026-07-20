//! T8h: a body the structurer rejects with the shared-branch-tail reason
//! (a nested-loop shared tail or an over-cap tail) re-emits through the block
//! trampoline `for (;;) switch (__bb)`, which accepts arbitrary CFGs. Spec:
//! docs/multi-backend-testing-plan.md "T8h spec". The node-executed value
//! checks are the correctness oracle; the shape assertions pin that the
//! fallback fired (and, dually, that a structured body does NOT trampoline).

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

// A nested loop inside an or-pattern success arm: dup_region rejects a loop
// header in the shared-tail region, so the structured path bails and the
// trampoline dispatches the loop as block edges.
#[test]
fn nested_loop_tail_trampolines() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let sel = 2\n\
             let out = match sel {\n\
                 1 | 2 | 3 => {\n\
                     let acc = 0\n\
                     let i = 0\n\
                     while i < 5 { acc = acc + i; i = i + 1 }\n\
                     acc\n\
                 }\n\
                 _ => 100\n\
             }\n\
             return out + sel\n\
         }",
    );
    let js = emit_closure(&ws, "root").expect("nested-loop tail should trampoline, not reject");
    assert!(js.contains("for (;;) {"), "expected a trampoline loop:\n{js}");
    assert!(js.contains("switch (__bb"), "expected a __bb dispatch:\n{js}");
    eval_int(&js, "root", 12);
}

// An or-pattern arm whose straight-line body exceeds the tail-dup inst cap.
// The trampoline flattens its blocks into sibling switch cases; the reused
// match-discriminant temp (`v<N> = c<sel>`, defined once, read in each
// comparison case) must hoist to fn scope, else it is undefined cross-case.
#[test]
fn over_cap_tail_trampolines_and_hoists_temp() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let sel = 2\n\
             let out = match sel {\n\
                 1 | 2 | 3 => {\n\
                     let a0 = sel + 1\n\
                     let a1 = a0 + 1\n\
                     let a2 = a1 + 1\n\
                     let a3 = a2 + 1\n\
                     let a4 = a3 + 1\n\
                     let a5 = a4 + 1\n\
                     let a6 = a5 + 1\n\
                     let a7 = a6 + 1\n\
                     let a8 = a7 + 1\n\
                     let a9 = a8 + 1\n\
                     let a10 = a9 + 1\n\
                     let a11 = a10 + 1\n\
                     let a12 = a11 + 1\n\
                     let a13 = a12 + 1\n\
                     let a14 = a13 + 1\n\
                     let a15 = a14 + 1\n\
                     let a16 = a15 + 1\n\
                     let a17 = a16 + 1\n\
                     a17\n\
                 }\n\
                 _ => 0\n\
             }\n\
             return out\n\
         }",
    );
    let js = emit_closure(&ws, "root").expect("over-cap tail should trampoline");
    assert!(js.contains("switch (__bb"), "expected a __bb dispatch:\n{js}");
    // The discriminant temp is declared at fn scope (an assignment `= c` inside a
    // case, not a per-case `let v<N> = c`), proving the cross-case hoist. Without
    // it, node would throw "v is not defined" -- so a green eval also guards this.
    eval_int(&js, "root", 20);
}

// An unstructurable body (nested loop in a shared arm) returning a TUPLE: the
// structured multi-value return arm is ported into the trampoline term printer,
// so `CallMulti` destructures the packed array the callee returns.
#[test]
fn multi_value_return_through_fallback() {
    let ws = ws_with(
        "fn pair(sel: int) -> (int, int) {\n\
             let s = match sel {\n\
                 1 | 2 | 3 => {\n\
                     let acc = 0\n\
                     let i = 0\n\
                     while i < 4 { acc = acc + i; i = i + 1 }\n\
                     acc\n\
                 }\n\
                 _ => 0\n\
             }\n\
             return (s, s + 1)\n\
         }\n\
         fn root() -> int {\n\
             let (a, b) = pair(2)\n\
             return a * 10 + b\n\
         }",
    );
    let js = emit_closure(&ws, "root").expect("multi-value return through fallback");
    assert!(js.contains("switch (__bb"), "expected the callee to trampoline:\n{js}");
    assert!(js.contains("return ["), "expected packed multi-value return:\n{js}");
    // pair(2) = (6, 7) -> 6*10 + 7.
    eval_int(&js, "root", 67);
}

// Untouched by construction: a body the structurer handles emits NO trampoline.
// The fallback fires only PAST the structured path, so currently-passing
// emission is byte-unchanged.
#[test]
fn structured_body_does_not_trampoline() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let x = 3\n\
             let acc = 0\n\
             let i = 0\n\
             while i < x { acc = acc + i; i = i + 1 }\n\
             if acc > 2 { return acc + 100 }\n\
             return acc\n\
         }",
    );
    let js = emit_closure(&ws, "root").expect("structured body should emit");
    assert!(!js.contains("__bb"), "a structured body must not trampoline:\n{js}");
    eval_int(&js, "root", 103);
}
