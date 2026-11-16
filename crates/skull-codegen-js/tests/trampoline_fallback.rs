//! Reducible shared-tail bodies use placement scopes, including loops and long
//! arms. Irreducible bodies still use the block trampoline. The corpus files run
//! these bodies on both backends; this checks JS placement and the scratch-bank
//! ABI across shared-tail control flow.

use crate::common::{emit_closure, ws_with};

fn assert_trampolines(js: &str, root: &str) {
    assert!(
        js.contains("for (;;) {") && js.contains("switch (__bb"),
        "`{root}` should take the block trampoline:\n{js}"
    );
}

fn assert_structured(js: &str, root: &str) {
    assert!(
        !js.contains("for (;;) {") && !js.contains("switch (__bb"),
        "`{root}` should use structured placement:\n{js}"
    );
}

#[test]
fn shared_tail_corpus_bodies_use_structured_placement() {
    let ws = ws_with(include_str!(
        "../../../tests/control_flow/js_trampoline_fallback.rock"
    ));
    for root in ["nested_loop_tail", "over_cap_tail"] {
        assert_structured(&emit_closure(&ws, root).unwrap(), root);
    }
    let js = emit_closure(&ws, "unstructurable_pair").unwrap();
    assert_structured(&js, "unstructurable_pair");
    assert!(
        js.contains("__rock_ret1 = ") && !js.contains("return ["),
        "a structured multi-value return must publish to the scratch bank:\n{js}"
    );
}

#[test]
fn irreducible_cfg_uses_the_block_trampoline() {
    use skull_codegen_js::emit::emit_fn;
    use skull_mir::mir::{Inst, Lit, MirFunc, Term};

    let ws = ws_with("fn main() {}");
    let mut mf = MirFunc::new(ustr::Ustr::from("root"));
    let enter_right = mf.new_cell(None);
    let loop_again = mf.new_cell(None);
    mf.params = vec![enter_right, loop_again];
    let left = mf.new_block();
    let right = mf.new_block();
    let exit = mf.new_block();
    let route = mf.emit(mf.entry, Inst::Load(enter_right));
    mf.set_term(
        mf.entry,
        Term::Branch {
            cond: route,
            then_bb: right,
            else_bb: left,
        },
    );
    let again = mf.emit(left, Inst::Load(loop_again));
    mf.set_term(
        left,
        Term::Branch {
            cond: again,
            then_bb: right,
            else_bb: exit,
        },
    );
    let reset = mf.emit(right, Inst::Const(Lit::Bool(false)));
    mf.emit(
        right,
        Inst::Store {
            cell: loop_again,
            value: reset,
        },
    );
    mf.set_term(right, Term::Goto(left));
    let result = mf.emit(exit, Inst::Const(Lit::Int(42)));
    mf.set_term(exit, Term::Return(vec![result]));

    let js = emit_fn(&ws, &mf, "root").expect("irreducible body emits");
    assert_trampolines(&js, "root");
    crate::common::run_node(&format!(
        r#"{js}
for (const enterRight of [false, true]) {{
    for (const loopAgain of [false, true]) {{
        if (root(enterRight, loopAgain) !== 42) {{
            throw new Error("irreducible CFG returned the wrong value");
        }}
    }}
}}
"#
    ));
}

#[test]
fn a_shared_tail_caller_reads_the_scratch_bank() {
    let ws = ws_with(include_str!("../../../tests/nobox/js_return_abi.rock"));
    let js = emit_closure(&ws, "shared_tail_caller").unwrap();
    assert!(
        js.contains("= __rock_ret1;"),
        "expected a scratch-slot capture:\n{js}"
    );
}

#[test]
fn structured_body_does_not_trampoline() {
    let ws = ws_with(
        "fn root() -> int {\n\
             let acc = 0\n\
             let i = 0\n\
             while i < 3 { acc = acc + i; i = i + 1 }\n\
             if acc > 2 { return acc + 100 }\n\
             return acc\n\
         }",
    );
    let js = emit_closure(&ws, "root").unwrap();
    assert!(
        !js.contains("__bb"),
        "a structured body must not trampoline:\n{js}"
    );
}
