//! SENTINEL -- an `Iterator`-bounded body has no standalone emission
//! (`docs/monomorphization-plan.md` S 12, mono cut C+D step D-1).
//!
//! A bounded fn exists only as an inline splice: its body holds placeholder
//! `Inst::CallBoundMulti`s whose concrete `next` is known per instantiation, and
//! the inliner substitutes them from the SITE's `BoundTree` when it splices
//! (`skull_transform::inline::resolve_cloned_bound_calls`). With inlining
//! disabled there is no splice, so the call has no concrete callee at all and
//! the build is REJECTED.
//!
//! That makes the plan's standing gate -- "compiling with inlining DISABLED
//! remains correct" -- false today, which is why D-1 precedes E1: E1 runs with
//! inlining off by construction.
//!
//! Correct behavior is that the bounded body emits standalone at each
//! instantiation, with its placeholders resolved. **The assertions below are
//! INVERTED to pin the rejection.** When D-1 lands they go red; flip each to its
//! documented positive form and delete this paragraph.
//!
//! The mode is process-global, so this holds `common::AblationGuard` -- THE
//! lock for this binary, shared with `ablation_totality.rs`. A per-file mutex
//! would serialize this file against itself and against nothing else.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use ustr::Ustr;

use crate::common::AblationGuard as ModeGuard;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// A user fn taking an `Iterator`-bounded generic, called at one concrete
/// chain. `total`'s body holds the placeholder; `main` is the concrete root.
const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

/// The same program with the bounded generic REMOVED -- `total` takes the
/// concrete list. The admission control: it must emit with inlining disabled,
/// so a rejection above is about the BOUND and not about the mode refusing to
/// emit anything at all.
const CONCRETE: &str = r#"
fn total(xs: [int]) -> int {
    let s = 0
    for v in xs { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs)}")
}
"#;

fn emit_err(src: &str) -> Option<String> {
    let ws = check(src);
    let mid = skull::user_module_id(&ws).expect("user module");
    skull::mir_emit::emit_workspace_mir(&ws, mid).err().map(|e| e.message)
}

/// SENTINEL. Correct behavior: `emit_err` is `None` and the emitted Lua carries
/// a standalone definition of `total`. When D-1 lands, flip to
/// `assert!(err.is_none(), ...)` and assert the definition header.
#[test]
fn a_bounded_body_cannot_emit_with_inlining_disabled_sentinel() {
    let _g = ModeGuard::on();
    let err = emit_err(BOUNDED);
    assert!(
        err.as_deref().is_some_and(|m| m.contains("Iterator")),
        "PINNED BEHAVIOR (BUG): with inlining disabled, a call to an \
         `Iterator`-bounded fn must be REJECTED today, because the body's \
         placeholder bound-calls are resolved only by the splice. If this no \
         longer rejects, D-1 landed -- flip the assertion and assert the \
         standalone definition. Got: {err:?}"
    );
}

/// The admission control for the sentinel. Not inverted: this asserts the
/// CORRECT behavior and must stay green through D-1.
///
/// Without it the sentinel could be green because the mode rejects every
/// program, which would say nothing about bounded bodies.
#[test]
fn an_unbounded_body_emits_with_inlining_disabled() {
    let _g = ModeGuard::on();
    let err = emit_err(CONCRETE);
    assert!(
        err.is_none(),
        "the same program without a bounded generic must still emit with \
         inlining disabled, or the sentinel cannot attribute its rejection to \
         the bound; got {err:?}"
    );
}
