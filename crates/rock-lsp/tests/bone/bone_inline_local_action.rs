//! The bone "Inline local" code action (`compute_code_actions_workspace`).
//!
//! On a `let x = <init>` binding or any of its uses, the bone backend offers a
//! REFACTOR_INLINE action whose multi-edit replaces every use with the
//! initializer (parenthesized when the initializer isn't atomic) and deletes the
//! `let`. `tests/lsp/inline_local_variable.rock.lsp` drives the same action
//! through the JSON-RPC server.

use crate::common::*;
use lsp_types::*;

/// The action whose title mentions inlining a local, or None.
fn inline_action(actions: &[CodeAction]) -> Option<&CodeAction> {
    actions.iter().find(|a| a.title.starts_with("Inline local"))
}

#[test]
fn offered_on_a_use_and_leaves_an_atomic_init_unparenthesized() {
    let src = "\
fn f(v: int) -> int {
    let y = g(v)
    return y + 1
}
fn g(a: int) -> int { a }
";
    // cursor on a USE of `y` (line 2)
    let actions = run(src, 2, 11);
    let action = inline_action(&actions).expect("expected an inline-local action on a use");
    assert_eq!(action.kind, Some(CodeActionKind::REFACTOR_INLINE));
    let out = apply_all(src, action);
    assert_eq!(
        out,
        "fn f(v: int) -> int {\n    return g(v) + 1\n}\nfn g(a: int) -> int { a }\n"
    );
}

#[test]
fn no_action_on_a_parameter() {
    let src = "\
fn f(p: int) -> int {
    return p + 1
}
";
    // cursor on the param `p` in its use -- a param has no `let` to inline.
    assert!(
        inline_action(&run(src, 1, 11)).is_none(),
        "a parameter is not an inlineable local"
    );
}
