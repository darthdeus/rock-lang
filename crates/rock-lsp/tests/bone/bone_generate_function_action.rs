//! The bone "Generate function" code action (`compute_code_actions_workspace`).
//!
//! When the cursor is on a call `name(...)` to a function that doesn't exist,
//! the bone backend offers an action that inserts a stub `fn name(params) { ? }`
//! after the enclosing function. Param types come from the argument types (or
//! `any` when unknown); param names reuse a simple-identifier argument, else
//! `argN`. A concrete result type (from a typed use site) becomes `-> T`; an
//! unconstrained result stays Unit. The body is a `?` expression hole.
//!
//! The strongest check here: apply the edit, then confirm the result has no
//! ERRORS on bone -- i.e. the generated stub actually resolves the
//! previously-unresolved call. (The stub's `?` body is a hole, which is itself
//! a WARNING-level TODO, so "resolves the call" means "no errors", not "no
//! diagnostics".)

use crate::common::*;
use lsp_types::*;

fn gen_action(actions: &[CodeAction]) -> Option<&CodeAction> {
    actions
        .iter()
        .find(|a| a.title.contains("Generate function"))
}

/// Type-check `src` on a fresh bone workspace; returns its diagnostics.
fn bone_diags(src: &str) -> Vec<Diagnostic> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checked.rock");
    bone_ws(&path, src)
        .bone
        .diagnostics_for(path.to_str().unwrap())
}

#[test]
fn generated_stubs_resolve_calls() {
    for (src, line, ch, signature, title) in [
        (
            "fn main() {\n    do_thing(5)\n}\n",
            1,
            6,
            "fn do_thing(arg0: int)",
            Some("do_thing"),
        ),
        (
            "fn main() {\n    let count = 5\n    tally(count)\n}\n",
            2,
            4,
            "fn tally(count: int)",
            None,
        ),
        (
            "global count: int = 1\nfn main() {\n    want(count)\n}\n",
            2,
            6,
            "fn want(count: any)",
            None,
        ),
        (
            "fn main() {\n    let count = 5\n    place(count, 9)\n}\n",
            2,
            4,
            "fn place(count: int, arg1: int)",
            None,
        ),
        (
            "fn main() {\n    let s: string = label(7)\n}\n",
            1,
            20,
            "fn label(arg0: int) -> string",
            None,
        ),
    ] {
        let actions = run(src, line, ch);
        let action = gen_action(&actions).expect("expected a generate-function action");
        if let Some(name) = title {
            assert!(action.title.contains(name), "title: {}", action.title);
        }
        let out = apply(src, action);
        assert!(out.contains(signature), "missing {signature:?}: {out}");
        assert!(out.contains('?'), "body is a hole: {out}");
        let diagnostics = bone_diags(&out);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.severity != Some(DiagnosticSeverity::ERROR)),
            "generated stub should resolve the call: {diagnostics:?}\n{out}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expression hole")),
            "the stub body is a visible TODO warning: {diagnostics:?}"
        );
    }
}

#[test]
fn generates_after_an_earlier_ambiguous_call() {
    let src = "\
mod a { pub fn hit() {} }
mod b { pub global hit: int = 0 }
use a::*;
use b::*;
fn main() {
    hit()
    want(3)
}
";
    let actions = run(src, 6, 6);
    let action = gen_action(&actions).expect("expected a generate-function action");
    assert!(action.title.contains("want"), "title: {}", action.title);

    let out = apply(src, action);
    assert!(
        out.contains("fn want(arg0: int)"),
        "param from literal: {out:?}"
    );
    let errors: Vec<_> = bone_diags(&out)
        .into_iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert!(
        errors.iter().any(|d| d.message.contains("hit")),
        "the earlier ambiguous call remains an error: {errors:?}"
    );
}

#[test]
fn no_action_when_the_function_already_exists() {
    // `foo` exists but the call has a type error -- that's not a missing
    // function, so we must not offer to generate one.
    let src = "\
fn foo(x: int) {}

fn main() {
    foo(\"bad\")
}
";
    let actions = run(src, 3, 4);
    assert!(
        gen_action(&actions).is_none(),
        "existing function with a bad call: no generate action: {actions:?}"
    );
}

#[test]
fn no_action_when_cursor_is_not_on_a_call() {
    let src = "\
fn main() {
    let x = 1
}
";
    assert!(gen_action(&run(src, 1, 8)).is_none());
}
