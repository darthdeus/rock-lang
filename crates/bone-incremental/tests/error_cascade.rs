//! Cascade suppression: a `let` whose RHS fails to type-check must NOT spawn a
//! second "undefined variable" diagnostic at every later use of the binding.
//! One root cause, one diagnostic -- the standard error-poisoning move (Rust's
//! `{error}` type, TypeScript's error-`any`): bind the failed `let`'s LHS to an
//! error type that unifies with everything, so downstream uses stay quiet.
//!
//! This is NOT rock-parity for its own sake -- a cascade is bad LSP UX (a
//! squiggle under perfectly-fine `use_it(x)` points at a symptom, not the bug,
//! and vanishes the instant the RHS is fixed). rock just happens to already do
//! the right thing, so it is the oracle.
//!
//! Premise verified against real rock (`rock-lsp diagnostics --backend lua`):
//! the source below yields exactly ONE error -- the root `undefined_thing` --
//! and no diagnostic on `x`. bone today emits TWO: the root plus
//! `Undefined variable \`x\`` (`diagnostics --backend bone`).
//!
//! RED until `Statement::Let` in `inference.rs` binds `name` to an error type
//! when its RHS inference fails, instead of leaving `name` unbound.

use bone_semantic::prelude::*;
use bone_incremental::session::Session;

// `x`'s RHS is undefined; `x` is then used once. The root error is real; the
// use of `x` must NOT produce a second diagnostic.
const SRC: &str = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    let x = undefined_thing
    let y = use_it(x)
}
";

fn diags(src: &str) -> Vec<String> {
    let mut s = Session::new(vec![], vec![], &bone_core::modules::NsAliases::default()).expect("session");
    let _ = s.update_file("c.rock", src);
    s.diagnostics().iter().map(|e| e.message.clone()).collect()
}

fn parse(src: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from("<error_cascade>")).expect("parse")
}

// Rendered type at byte `off` in `src`, read via the same `type_at` path hover
// / member-completion use. `None` if nothing resolves there (e.g. the enclosing
// body was never analyzed).
fn type_at_offset(src: &str, off: usize) -> Option<String> {
    let ws = semantic_pass(&mut [parse(src)]).expect("a body error must not abort the pass");
    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|m| *m != ModuleId::builtin())
        .expect("user module survives");
    let inf = ws.inference(mid).expect("inference for user module");
    let at = bone_semantic::query::type_at(&ws, mid, inf, off)?;
    Some(ws.render(at.body, at.ty))
}

// Rendered type at the first byte of `needle` in `src`.
fn type_at_needle(src: &str, needle: &str) -> Option<String> {
    type_at_offset(src, src.find(needle).expect("needle present"))
}

// Rendered type of the binding named by `marker` (e.g. "let x"); points just
// past the `let `.
fn type_of_binding(src: &str, marker: &str) -> Option<String> {
    type_at_offset(src, src.find(marker).expect("marker present") + "let ".len())
}

// Any diagnostic blaming the poisoned binding `name` (e.g. "`a`") -- a cascade.
fn cascade_of(src: &str, name: &str) -> Vec<String> {
    let tick = format!("`{name}`");
    diags(src).into_iter().filter(|m| m.contains(&tick)).collect()
}

// The root error must always surface -- recovery suppresses the CASCADE, never
// the real failure. Guards the suppression tests below from passing by
// swallowing everything.
#[test]
fn root_error_still_surfaces() {
    let msgs = diags(SRC);
    assert!(
        msgs.iter().any(|m| m.contains("undefined_thing")),
        "the root error must surface; got {msgs:?}"
    );
}

// RED: no diagnostic blames the poisoned binding `x`. The failed RHS poisons
// `x` with an error type, so `use_it(x)` type-checks silently.
#[test]
fn poisoned_binding_use_emits_no_cascade() {
    let msgs = diags(SRC);
    let cascade: Vec<_> = msgs
        .iter()
        .filter(|m| m.contains("`x`"))
        .collect();
    assert!(
        cascade.is_empty(),
        "a failed `let` RHS must not cascade onto uses of the binding; got {cascade:?}"
    );
}

// RED, sharper: exactly one error total for one root cause. Verified against
// real rock: a single `Undefined variable` error (lints/suggestions aside,
// which `diagnostics()` does not include here).
#[test]
fn single_root_cause_yields_single_error() {
    let msgs = diags(SRC);
    assert_eq!(
        msgs.len(),
        1,
        "one root cause must yield one diagnostic; got {}: {msgs:?}",
        msgs.len()
    );
}

// RED until the dedicated error type exists: a binding whose RHS failed must
// render as `<error>` on hover -- NOT as `never` (which would be a lie: the
// binding does not diverge, its type is unknown-because-erroneous) and NOT as
// `any` (which would suggest a deliberate dynamic type). This is the behavior
// that justifies a distinct `BuiltinTy::Error` over reusing `never`.
#[test]
fn poisoned_binding_renders_as_error_type() {
    assert_eq!(type_of_binding(SRC, "let x").as_deref(), Some("<error>"));
}

// A poisoned binding used MANY times must still produce no cascade -- guards
// against a fix that only suppresses the first downstream use.
#[test]
fn poisoned_binding_multiple_uses_no_cascade() {
    let src = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    let x = undefined_thing
    let a = use_it(x)
    let b = use_it(x)
    let c = use_it(x)
}
";
    let msgs = diags(src);
    let cascade: Vec<_> = msgs.iter().filter(|m| m.contains("`x`")).collect();
    assert!(
        cascade.is_empty(),
        "no use of a poisoned binding may cascade; got {cascade:?}"
    );
}

// ======================================================================
// Sibling binders: same poison principle, two distinct gaps.
//
// Every other binding form does `infer_expr(value/scrutinee)?` before binding
// its pattern vars, so a failed RHS leaves them unbound. Two shapes:
//
//   * `let (a, b) = ...` / `let N(x) = ...` bind into the SAME block as the
//     code that follows -- so a failure CASCADES, exactly like plain `let`.
//     Fix: poison the pattern vars on failure (then surface the root error).
//
//   * `for` / `while let` / `if let` bind into a NESTED body that is skipped
//     ENTIRELY when the scrutinee `?`-fails -- so hover/completion inside the
//     loop/branch go dark. No cascade today, but a recovery gap. Fix: poison
//     the pattern vars AND still infer the body.
//
// Verified against real rock (oracle): tuple-let reports only the root error
// (no `a` cascade); `for x in undefined { let z = ... }` keeps `z` in scope.
//
// RED until the failure paths route through the `Error` poison (and, for the
// nested forms, continue into the body). See docs/error-recovery-design.md.
// ======================================================================

// --- destructuring let: cascade, like plain let ---

const TUPLE: &str = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    let (a, b) = undefined_thing
    let z = use_it(a)
}
";

#[test]
fn let_tuple_root_error_surfaces() {
    assert!(diags(TUPLE).iter().any(|m| m.contains("undefined_thing")));
}

// RED: a failed tuple RHS must not cascade onto `a`.
#[test]
fn let_tuple_no_cascade() {
    let c = cascade_of(TUPLE, "a");
    assert!(c.is_empty(), "tuple-let must not cascade onto `a`; got {c:?}");
}

// RED: each destructured slot is poisoned to `<error>`.
#[test]
fn let_tuple_slots_poisoned() {
    assert_eq!(type_at_needle(TUPLE, "a, b").as_deref(), Some("<error>"));
}

// --- for: nested body must still be analyzed ---

const FOR: &str = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    for x in undefined_thing {
        let z = use_it(x)
    }
}
";

#[test]
fn for_root_error_surfaces() {
    assert!(diags(FOR).iter().any(|m| m.contains("undefined_thing")));
}

// RED: the loop body is inferred despite the failed iterable -- a body-local
// binding resolves to its real type (poisoned `x` flows through `use_it`).
#[test]
fn for_body_analyzed_past_failed_iter() {
    assert_eq!(type_of_binding(FOR, "let z").as_deref(), Some("int"));
}

// RED: the loop variable is poisoned, not unbound.
#[test]
fn for_loop_var_poisoned() {
    assert_eq!(type_at_needle(FOR, "x in").as_deref(), Some("<error>"));
}

// --- while let: nested body must still be analyzed ---

const WHILE_LET: &str = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    while let x = undefined_thing {
        let z = use_it(x)
    }
}
";

#[test]
fn while_let_root_error_surfaces() {
    assert!(diags(WHILE_LET).iter().any(|m| m.contains("undefined_thing")));
}

// RED: body analyzed past a failed `while let` scrutinee.
#[test]
fn while_let_body_analyzed() {
    assert_eq!(type_of_binding(WHILE_LET, "let z").as_deref(), Some("int"));
}

// --- if let: nested branch must still be analyzed ---

const IF_LET: &str = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    if let x = undefined_thing {
        let z = use_it(x)
    }
}
";

#[test]
fn if_let_root_error_surfaces() {
    assert!(diags(IF_LET).iter().any(|m| m.contains("undefined_thing")));
}

// RED: then-branch analyzed past a failed `if let` scrutinee.
#[test]
fn if_let_body_analyzed() {
    assert_eq!(type_of_binding(IF_LET, "let z").as_deref(), Some("int"));
}

// --- safe-nav let: nested body must still be analyzed ---
// `?.let` reuses the receiver's name for the unwrapped binding, so a failed
// receiver and the in-body reference share the identifier `undefined_thing`.

const SAFE_NAV_LET: &str = "\
fn use_it(n: int) -> int {
    n
}

fn main() {
    let r = undefined_thing?.let {
        let z = use_it(undefined_thing)
        z
    }
}
";

#[test]
fn safe_nav_let_root_error_surfaces() {
    assert!(diags(SAFE_NAV_LET).iter().any(|m| m.contains("undefined_thing")));
}

// RED: the `?.let` body is analyzed past a failed receiver -- a body-local
// binding resolves (poisoned receiver flows through `use_it`).
#[test]
fn safe_nav_let_body_analyzed() {
    assert_eq!(type_of_binding(SAFE_NAV_LET, "let z").as_deref(), Some("int"));
}
