//! Shared-branch-tail structuring (T8c). The single `unstructured control flow
//! (shared branch tail)` reject conflated three CFG-shape classes; each is now
//! structured by its own technique. The node-executed value checks are the correctness oracle; the
//! mutation tests confirm the structuring is load-bearing (drop an arm / mistarget
//! a labeled continue -> a wrong result, not a silent pass).

use crate::common::ws_with;

crate::runtime_eval!(eval_int: JS_NB_CHECK_SHIMS, JS_VEC_CHECK_SHIMS, JS_TABLE_PRIM, JS_STRUCTURAL_EQ, JS_CALL_HELPER);

// Class A -- a loop inside a lambda body. structure::analyze roots at lambda
// entries, so the closure's `while` is a registered loop (was walked flat into
// the shared-tail reject). The captured accumulator is shared by reference; the
// loop counter is lambda-owned (not a per-iteration capture).
#[test]
fn lambda_body_loop_structures() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let sum = 0\n\
             let run = () => {\n\
                 let i = 1\n\
                 while i <= 5 { sum = sum + i; i = i + 1 }\n\
             }\n\
             run()\n\
             sum\n\
         }",
    );
    let js = crate::common::emit_closure(&ws, "root").expect("lambda-body loop should structure");
    eval_int(&js, "root", 15);
}

// Class B -- an inlined iterator combinator TERMINAL chain. Post-inline the
// `next()` skip-loop nests inside the for-loop; a body block `continue`s the
// OUTER loop (a multi-level jump the labeled-loop path structures).
#[test]
fn combinator_terminal_chain_structures() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let v = [1, 2, 3]\n\
             v.map(_ * 2).filter(_ > 2).reduce(0, (a, b) => a + b)\n\
         }",
    );
    let js = crate::common::emit_closure(&ws, "root")
        .expect("combinator terminal chain should structure");
    eval_int(&js, "root", 10);
}

// Class C.1 -- multiple `continue` paths converging on a shared latch tail (the
// T8a-regressed shape): a forward shared tail, duplicated into each arm.
#[test]
fn multiple_continue_shared_tail_structures() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let v = [1, 2, 3, 4, 5, 6]\n\
             let sum = 0\n\
             for x in v {\n\
                 if x % 2 == 0 { if x % 4 == 0 { continue } }\n\
                 else { if x % 3 == 0 { continue } }\n\
                 sum = sum + x\n\
             }\n\
             sum\n\
         }",
    );
    let js = crate::common::emit_closure(&ws, "root")
        .expect("multiple-continue shared tail should structure");
    eval_int(&js, "root", 14);
}

// Class C.2 -- an or-pattern success arm reached from N alternatives (a forward
// shared tail with 2+ Goto predecessors), duplicated into each alternative.
#[test]
fn or_pattern_shared_arm_structures() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let x = 2\n\
             return match x { 1 | 2 | 3 => 42, _ => 0 }\n\
         }",
    );
    let js =
        crate::common::emit_closure(&ws, "root").expect("or-pattern shared arm should structure");
    eval_int(&js, "root", 42);
    // The alternative not on the matched value still routes correctly.
    let ws0 = ws_with(
        "fn root() -> int {\n\
             let x = 7\n\
             return match x { 1 | 2 | 3 => 42, _ => 0 }\n\
         }",
    );
    let js0 =
        crate::common::emit_closure(&ws0, "root").expect("or-pattern default should structure");
    eval_int(&js0, "root", 0);
}
