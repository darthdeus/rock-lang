//! The bone "Inline local" code action (`compute_code_actions_workspace`).
//!
//! On a `let x = <init>` binding or any of its uses, the bone backend offers a
//! REFACTOR_INLINE action whose multi-edit replaces every use with the
//! initializer (parenthesized when the initializer isn't atomic) and deletes the
//! `let`. No `.rock.lsp` fixture type for code actions, so this is end-to-end.

use crate::common::*;
use lsp_types::*;

/// The action whose title mentions inlining a local, or None.
fn inline_action(actions: &[CodeAction]) -> Option<&CodeAction> {
    actions.iter().find(|a| a.title.starts_with("Inline local"))
}

#[test]
fn inlines_and_parenthesizes_a_binary_initializer() {
    let src = "\
fn f() -> int {
    let x = 1 + 2
    return x * x
}
";
    // cursor on the binding `x` (line 1)
    let actions = run(src, 1, 8);
    let action = inline_action(&actions).expect("expected an inline-local action");
    assert_eq!(action.kind, Some(CodeActionKind::REFACTOR_INLINE));
    let out = apply_all(src, action);
    assert_eq!(out, "fn f() -> int {\n    return (1 + 2) * (1 + 2)\n}\n");
}

#[test]
fn offered_on_a_use_and_leaves_an_atomic_init_unparenthesized() {
    let src = "\
fn f(v: int) -> int {
    let y = g(v)
    return y + y
}
fn g(a: int) -> int { a }
";
    // cursor on a USE of `y` (line 2)
    let actions = run(src, 2, 11);
    let action = inline_action(&actions).expect("expected an inline-local action on a use");
    let out = apply_all(src, action);
    assert_eq!(
        out,
        "fn f(v: int) -> int {\n    return g(v) + g(v)\n}\nfn g(a: int) -> int { a }\n"
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
