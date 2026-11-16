//! `emit_with_closure` runs the same per-body transform pipeline as the luajit
//! path, so an `@inline`/`@pure` callee is spliced rather than co-emitted.
//! tests/inline/js_inline.rock runs these calls on both backends.

use crate::common::ws_with;

const EXHAUSTIVE_MATCH: &str = r#"
enum Choice { First, Second }
@inline fn choose<T>(choice: Choice, first: T, second: T) -> T {
    match choice { Choice::First => first, Choice::Second => second }
}
fn root(choice: Choice) -> int { choose(choice, 10, 20) + 1 }
"#;

#[test]
fn forced_inline_callees_are_spliced() {
    for (src, callee) in [
        (
            "@inline fn dbl(x: int) -> int { x * 2 }\nfn root() -> int { dbl(3) }",
            "dbl",
        ),
        (
            "@pure fn sq(x: int) -> int { x * x }\nfn root() -> int { sq(3) }",
            "sq",
        ),
        (EXHAUSTIVE_MATCH, "choose"),
    ] {
        let js = crate::common::emit_closure(&ws_with(src), "root").expect("emits");
        assert!(js.contains("function root("), "missing caller:\n{js}");
        assert!(!js.contains(callee), "{callee} was not spliced:\n{js}");
    }
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
    // Both arms share one terminal block; placement should emit it once.
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
    let unreachable = "throw new Error(\"rock: reached an unreachable block\");";
    assert_eq!(
        structured.matches(unreachable).count(),
        1,
        "shared unreachable block should be emitted once in structured output:\n{structured}"
    );
    assert!(
        trampoline.contains("switch (__bb"),
        "forced fallback should still use the trampoline:\n{trampoline}"
    );
    assert_eq!(trampoline.matches(unreachable).count(), 1, "{trampoline}");
    assert!(
        !structured.contains("switch (__bb"),
        "expected structured placement:\n{structured}"
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
