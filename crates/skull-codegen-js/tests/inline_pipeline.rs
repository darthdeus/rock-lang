//! Shared transform-pipeline emission (T8a). `emit_with_closure` now runs the
//! same per-body sequence the luajit path runs (lower -> inline -> cleanup ->
//! recover_counted_loops -> residual-bound-call validation), so an `@inline`/
//! `@pure` callee is SPLICED (no standalone co-emission), an iterator combinator
//! chain monomorphizes by inlining (was a `CallBoundMulti` reject), and a call
//! omitting a defaulted param is a legal shorter shape (was the arity-guard
//! false positive). The node-executed value checks are the correctness oracle.

use crate::common::ws_with;

/// Evaluate `<root>()` under node over the emitted defs.
fn eval_int(defs: &str, root: &str, expect: i64) {
    crate::common::eval_int(
        &[
            skull_codegen_js::JS_NB_CHECK_SHIMS,
            skull_codegen_js::JS_VEC_CHECK_SHIMS,
            skull_codegen_js::JS_TABLE_PRIM,
            skull_codegen_js::JS_STRUCTURAL_EQ,
            skull_codegen_js::JS_CALL_HELPER,
        ],
        defs,
        root,
        expect,
    );
}

// A call to an `@inline` fn is SPLICED into the caller: no standalone `function
// dbl` is co-emitted, and the run is correct.
#[test]
fn inline_callee_is_spliced() {
    let ws = ws_with("@inline fn dbl(x: int) -> int { x * 2 }\nfn root() -> int { dbl(3) }");
    let js = crate::common::emit_closure(&ws, "root").expect("emits");
    assert!(
        !js.contains("function dbl"),
        "@inline callee should be spliced, not co-emitted:\n{js}"
    );
    eval_int(&js, "root", 6);
}

// A `@pure` fn call is likewise spliced (both `@inline` and `@pure` force inline).
#[test]
fn pure_callee_is_spliced() {
    let ws = ws_with("@pure fn sq(x: int) -> int { x * x }\nfn root() -> int { sq(3) }");
    let js = crate::common::emit_closure(&ws, "root").expect("emits");
    assert!(
        !js.contains("function sq"),
        "@pure callee should be spliced, not co-emitted:\n{js}"
    );
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
    let js = crate::common::emit_closure(&ws, "root")
        .expect("inlined combinator chain structures via labeled loops");
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
    let js = crate::common::emit_closure(&ws, "root").expect("omitted default should emit");
    eval_int(&js, "root", 6);
}
const EXHAUSTIVE_MATCH: &str = r#"
enum Choice { First, Second }
@inline fn choose<T>(choice: Choice, first: T, second: T) -> T {
    match choice { Choice::First => first, Choice::Second => second }
}
fn root(choice: Choice) -> int { choose(choice, 10, 20) + 1 }
"#;

#[test]
fn exhaustive_match_callee_is_spliced() {
    let ws = ws_with(EXHAUSTIVE_MATCH);
    let js = crate::common::emit_closure(&ws, "root").expect("exhaustive match emits");
    assert!(js.contains("function root("), "missing caller:\n{js}");
    assert!(
        !js.contains("choose"),
        "exhaustive match callee was not spliced:\n{js}"
    );
}

#[test]
fn exhaustive_match_fallback_throws() {
    for source in [
        EXHAUSTIVE_MATCH.to_string(),
        EXHAUSTIVE_MATCH.replace("@inline ", ""),
    ] {
        let ws = ws_with(&source);
        let js = crate::common::emit_closure(&ws, "root").expect("exhaustive match emits");
        let program = format!(
            r#"{js}
if (root(0) !== 11 || root(1) !== 21) throw new Error("wrong match result");
let caught = false;
try {{ root(99); }} catch (err) {{
    caught = true;
    if (err.message !== "rock: reached an unreachable block") throw err;
}}
if (!caught) throw new Error("exhaustive match fallback returned");
"#
        );
        crate::common::run_node(&program);
    }
}

#[test]
fn unreachable_is_terminal_in_structured_and_trampoline_output() {
    use skull_codegen_js::emit::{CalleeNames, GlobalCtx};
    use skull_mir::mir::{Inst, Lit, MirFunc, Term};

    let ws = ws_with("fn main() {}");
    let mut mf = MirFunc::new(ustr::Ustr::from("root"));
    let first = mf.new_cell(None);
    let second = mf.new_cell(None);
    mf.params = vec![first, second];
    let left = mf.new_block();
    let right = mf.new_block();
    let success = mf.new_block();
    let trap = mf.new_block();
    let condition = mf.emit(mf.entry, Inst::Load(first));
    mf.set_term(
        mf.entry,
        Term::Branch {
            cond: condition,
            then_bb: left,
            else_bb: right,
        },
    );
    // Both arms share the trap: the second visit uses tail duplication.
    for block in [left, right] {
        let condition = mf.emit(block, Inst::Load(second));
        mf.set_term(
            block,
            Term::Branch {
                cond: condition,
                then_bb: success,
                else_bb: trap,
            },
        );
    }
    let value = mf.emit(success, Inst::Const(Lit::Int(42)));
    mf.set_term(success, Term::Return(vec![value]));
    let structured = skull_codegen_js::emit::emit_fn(&ws, &mf, "root").expect("structured emit");
    let trampoline = skull_codegen_js::emit::emit_fn_trampoline(
        &ws,
        &mf,
        "root",
        &CalleeNames::default(),
        &Default::default(),
        &GlobalCtx::default(),
        mf.entry,
        &mf.params,
        true,
    )
    .expect("trampoline emit");
    assert!(
        !structured.contains("switch (__bb"),
        "expected tail duplication:\n{structured}"
    );
    for js in [structured, trampoline] {
        let program = format!(
            r#"{js}
for (const first of [true, false]) {{
    if (root(first, true) !== 42) throw new Error("wrong result");
    let caught = false;
    try {{ root(first, false); }} catch (err) {{
        caught = true;
        if (err.message !== "rock: reached an unreachable block") throw err;
    }}
    if (!caught) throw new Error("unreachable block returned");
}}
"#
        );
        crate::common::run_node(&program);
    }
}
