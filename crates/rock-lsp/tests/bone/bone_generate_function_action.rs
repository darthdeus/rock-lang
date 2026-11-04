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
    std::fs::write(&path, src).unwrap();
    let mut bone = bone_query::BoneWorkspace::new();
    let p = path.to_str().unwrap();
    bone.update(p, src.to_string());
    bone.diagnostics_for(p)
}

/// Error-severity diagnostics only. A generated stub legitimately carries a
/// WARNING for its `?` body (an unfilled-hole TODO), so "the stub resolves the
/// call" means "no ERRORS", not "no diagnostics".
fn bone_errors(src: &str) -> Vec<Diagnostic> {
    bone_diags(src)
        .into_iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect()
}

#[test]
fn generates_unit_stub_for_a_statement_call() {
    let src = "\
fn main() {
    do_thing(5)
}
";
    // cursor on `do_thing` (line 1)
    let actions = run(src, 1, 6);
    let action = gen_action(&actions).expect("expected a generate-function action");
    assert!(action.title.contains("do_thing"), "title: {}", action.title);

    let out = apply(src, action);
    assert!(
        out.contains("fn do_thing(arg0: int)"),
        "param from literal: {out:?}"
    );
    assert!(out.contains("?"), "body is a hole: {out:?}");
    assert!(
        bone_errors(&out).is_empty(),
        "generated stub should resolve the call: {:?}\n--- src ---\n{out}",
        bone_errors(&out)
    );
    // The `?` body surfaces as a warning-level TODO -- the stub isn't silently
    // "done".
    assert!(
        bone_diags(&out)
            .iter()
            .any(|d| d.message.contains("expression hole")),
        "the stub body `?` is a visible TODO warning: {:?}",
        bone_diags(&out)
    );
}

#[test]
fn reuses_a_named_argument_as_the_param_name() {
    let src = "\
fn main() {
    let count = 5
    tally(count)
}
";
    let actions = run(src, 2, 4);
    let action = gen_action(&actions).expect("expected a generate-function action");
    let out = apply(src, action);
    assert!(
        out.contains("fn tally(count: int)"),
        "param name reuses the identifier arg: {out:?}"
    );
    assert!(
        bone_errors(&out).is_empty(),
        "should type-check: {:?}",
        bone_diags(&out)
    );
}

#[test]
fn mixes_named_and_positional_params() {
    let src = "\
fn main() {
    let count = 5
    place(count, 9)
}
";
    let actions = run(src, 2, 4);
    let action = gen_action(&actions).expect("expected a generate-function action");
    let out = apply(src, action);
    assert!(
        out.contains("fn place(count: int, arg1: int)"),
        "named first, positional second: {out:?}"
    );
    assert!(
        bone_errors(&out).is_empty(),
        "should type-check: {:?}",
        bone_diags(&out)
    );
}

#[test]
fn infers_concrete_return_type_from_a_typed_use_site() {
    let src = "\
fn main() {
    let s: string = label(7)
}
";
    let actions = run(src, 1, 20);
    let action = gen_action(&actions).expect("expected a generate-function action");
    let out = apply(src, action);
    assert!(
        out.contains("fn label(arg0: int) -> string"),
        "return type taken from the `let s: string` context: {out:?}"
    );
    assert!(
        bone_errors(&out).is_empty(),
        "should type-check: {:?}",
        bone_diags(&out)
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
