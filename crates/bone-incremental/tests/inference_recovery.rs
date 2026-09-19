//! Inference (type-check) error recovery: a body whose inference fails partway
//! still yields the types it inferred BEFORE the failure -- so hover / member
//! completion on an earlier binding keeps working in a function with one bad
//! line. Same boundary as lowering recovery ("plan C", `error_recovery.rs`): a
//! body is module-local, so recovering its partial inference is
//! convergence-safe. Interface errors (signatures / fields / globals) still
//! fail-fast.
//!
//! Premise verified against real rock: `rock-lsp hover` on `p` in `SRC` below
//! shows `Pair<int, string>` even though the next line is a type error. bone
//! discards the WHOLE body's inference today (`infer_module` drops `gamma.out`
//! on the `?`, so `per_fn` has no entry and `query::type_at` returns `None`).
//!
//! RED until the inference-recovery refactor lands: the `Expr::Block` loop in
//! `inference.rs` catches per-statement errors and keeps `gamma.out`;
//! `infer_module` stores the partial `BodyInference` in `per_fn` and records
//! the error(s) in `InferenceResult.errors`.

use bone_semantic::prelude::*;
use bone_incremental::session::Session;

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<inference_recovery>")).expect("parse")
}

fn ws_of(src: &str) -> (Workspace, ModuleId) {
    let ws = semantic_pass(&mut [parse(src)])
        .expect("a body INFERENCE error must not abort the pass");
    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|m| *m != ModuleId::builtin())
        .expect("user module survives");
    (ws, mid)
}

// `p`'s inferred, substituted type at its `let` binding -- this is what member
// completion on `p.` reads out of `binding_types`.
fn type_of_p(src: &str) -> Option<String> {
    let (ws, mid) = ws_of(src);
    let inf = ws.inference(mid).expect("inference for user module");
    let off = src.find("let p").expect("source has `let p`") + "let ".len();
    let at = bone_semantic::query::type_at(&ws, mid, inf, off)?;
    Some(ws.render(at.body, at.ty))
}

// A clean program: proves the offset + `type_at` binding path resolve `p` to
// the substituted generic type. Guards the assertion the RED tests below rely
// on (so a future regression there can't masquerade as "recovery works").
#[test]
fn sanity_valid_program_resolves_substituted_binding() {
    let valid = "\
struct Pair<A, B> {
    first: A,
    second: B,
}

fn main() {
    let p = Pair { first: 1, second: \"hi\" }
    let z = p
}
";
    assert_eq!(type_of_p(valid).as_deref(), Some("Pair<int, string>"));
}

// The source under recovery: `p` is bound, then a later statement fails to
// type-check (`string` assigned to `int`). The binding established BEFORE the
// error must keep its substituted type.
const SRC: &str = "\
struct Pair<A, B> {
    first: A,
    second: B,
}

fn main() {
    let p = Pair { first: 1, second: \"hi\" }
    let n: int = \"not an int\"
}
";

// RED: a binding established before the failing statement keeps its inferred,
// substituted type despite the later error.
#[test]
fn binding_before_inference_error_keeps_substituted_type() {
    assert_eq!(type_of_p(SRC).as_deref(), Some("Pair<int, string>"));
}

// RED: recovery keeps partial state, it does NOT hide the failure -- the unify
// error must still be recorded (never swallowed).
#[test]
fn inference_error_still_surfaces() {
    let (ws, mid) = ws_of(SRC);
    let inf = ws.inference(mid).expect("inference for user module");
    assert!(
        !inf.errors.is_empty(),
        "the unify failure must be recorded in InferenceResult.errors, not discarded"
    );
}

// `p` is bound AFTER the failing statement. Resolving it requires the Block
// loop to CONTINUE past the error, not just keep state up to the first failure.
// Verified against real rock: hover on `p` here shows `Pair<int, string>`.
const SRC_AFTER: &str = "\
struct Pair<A, B> {
    first: A,
    second: B,
}

fn main() {
    let n: int = \"not an int\"
    let p = Pair { first: 1, second: \"hi\" }
}
";

// RED: a binding established AFTER the failing statement still resolves
// (per-statement continue-past, not stop-at-first).
#[test]
fn binding_after_inference_error_still_resolves() {
    assert_eq!(type_of_p(SRC_AFTER).as_deref(), Some("Pair<int, string>"));
}

// RED: two independent type errors in ONE body BOTH surface. Continue-past
// without a per-body `Vec` would let the second overwrite the first in the
// `errors` map -- a swallowed error. Verified against real rock: it reports
// both mismatches.
#[test]
fn two_inference_errors_in_one_body_both_surface() {
    let src = "\
fn main() {
    let a: int = \"bad one\"
    let b: int = \"bad two\"
}
";
    let mut s = Session::new(vec![], vec![], &bone_core::modules::NsAliases::default()).expect("session");
    let _ = s.update_file("c.rock", src);
    let diags = s.diagnostics();
    assert!(
        diags.len() >= 2,
        "both type errors must surface (no swallow); got {}: {:?}",
        diags.len(),
        diags
    );
}
