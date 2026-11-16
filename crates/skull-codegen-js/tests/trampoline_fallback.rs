//! A body the structurer rejects with the shared-branch-tail reason re-emits
//! through the block trampoline `for (;;) switch (__bb)`. The corpus files run
//! these bodies on both backends; this checks that on JS they really take the
//! fallback, and that a trampolined body keeps the scratch-bank ABI.

use crate::common::{emit_closure, ws_with};

fn assert_trampolines(js: &str, root: &str) {
    assert!(
        js.contains("for (;;) {") && js.contains("switch (__bb"),
        "`{root}` should take the block trampoline:\n{js}"
    );
}

#[test]
fn shared_tail_corpus_bodies_trampoline() {
    let ws = ws_with(include_str!(
        "../../../tests/control_flow/js_trampoline_fallback.rock"
    ));
    for root in ["nested_loop_tail", "over_cap_tail"] {
        assert_trampolines(&emit_closure(&ws, root).unwrap(), root);
    }
    let js = emit_closure(&ws, "unstructurable_pair").unwrap();
    assert_trampolines(&js, "unstructurable_pair");
    assert!(
        js.contains("__rock_ret1 = ") && !js.contains("return ["),
        "a trampolined multi-value return must publish to the scratch bank:\n{js}"
    );
}

#[test]
fn a_trampolined_caller_reads_the_scratch_bank() {
    let ws = ws_with(include_str!("../../../tests/nobox/js_return_abi.rock"));
    let js = emit_closure(&ws, "trampolined_caller").unwrap();
    assert_trampolines(&js, "trampolined_caller");
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
