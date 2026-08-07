//! SENTINEL -- an `Iterator`-bounded body has no standalone JS emission
//! (`docs/monomorphization-plan.md` S 12, mono cut C+D step D-1).
//!
//! The JS SIBLING of `crates/skull/tests/bounded_standalone_emission_sentinel.rs`,
//! and it exists because the Lua one does not cover this backend. JS keeps its
//! own specialization queue (`spec_queue` / `collect_spec_bound_calls` in
//! `emit.rs`), so the Lua-side plumbing D-1 needs does not reach it -- two
//! populations for one question, which is the shape this arc keeps finding.
//!
//! JS also rejects a residual placeholder through a DIFFERENT surface: its
//! instruction match has no `CallBoundMulti` arm, so the call falls to the
//! generic `JS subset: unsupported effect` catch-all rather than to a
//! bounded-specific diagnostic. Measured end to end, one program under the mode
//! produces three rejections -- two shared with Lua, plus that one.
//!
//! Correct behavior is that the bounded body emits standalone at each
//! instantiation. **The assertion below is INVERTED to pin the rejection.** When
//! D-1 covers JS it goes red; flip it and assert the emitted definition.
//!
//! The ablation flag is process-global. `cargo test` runs this crate's tests as
//! threads in one binary, so the guard below holds a lock shared by every test
//! in it that touches the mode -- today only this file.

use crate::common::ws_with;

use skull_mir::lower::ScalarCtx;

/// THE ablation lock for this test binary. One mutex, not one per file: a
/// per-file `static` serializes a file against itself and against nothing else.
static ABLATION_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ModeGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl ModeGuard {
    fn on() -> Self {
        let g = ABLATION_MODE.lock().unwrap_or_else(|e| e.into_inner());
        skull_mir::ablation::clear();
        skull_mir::ablation::set_enabled(true);
        ModeGuard(g)
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        skull_mir::ablation::set_enabled(false);
        skull_mir::ablation::clear();
    }
}

fn emit_js(ws: &bone_semantic::inference::Workspace) -> Result<String, String> {
    let scx = ScalarCtx::new(ws).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    skull_codegen_js::emit::emit_program(ws, &scx, mid)
        .map(|p| p.program)
        .map_err(|e| e.message)
}

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

/// The same program with the bound removed. The admission control: it must emit
/// with inlining disabled, so the rejection above is about the BOUND rather than
/// about the mode refusing to emit anything.
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

/// SENTINEL. Correct behavior: `Ok`, with a standalone `total` definition in the
/// emitted JS. When D-1 covers this backend, flip to `is_ok()` and assert the
/// definition.
#[test]
fn a_bounded_body_cannot_emit_to_js_with_inlining_disabled_sentinel() {
    let _g = ModeGuard::on();
    let err = emit_js(&ws_with(BOUNDED)).err();
    assert!(
        err.is_some(),
        "PINNED BEHAVIOR (BUG): with inlining disabled, JS emission of a call to \
         an `Iterator`-bounded fn must be REJECTED today -- its body's placeholder \
         bound-calls are resolved only by the splice, and JS has no arm for one. \
         If this now emits, D-1 reached the JS backend: flip the assertion and \
         assert the standalone definition."
    );
}

/// Not inverted: this asserts the CORRECT behavior and must stay green through
/// D-1. Without it the sentinel would be satisfied by the mode rejecting every
/// program, which says nothing about bounded bodies.
#[test]
fn an_unbounded_body_emits_to_js_with_inlining_disabled() {
    let _g = ModeGuard::on();
    let err = emit_js(&ws_with(CONCRETE)).err();
    assert!(
        err.is_none(),
        "the same program without a bounded generic must still emit to JS with \
         inlining disabled, or the sentinel cannot attribute its rejection to the \
         bound; got {err:?}"
    );
}
