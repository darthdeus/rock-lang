//! An `Iterator`-bounded body emits STANDALONE to JS at each instantiation
//! (`docs/monomorphization-plan.md` S 12, mono cut C+D step D-1).
//!
//! The JS SIBLING of `crates/skull/tests/bounded_standalone_emission.rs`, and it
//! exists because the Lua one does not cover this backend. JS keeps its own
//! specialization queue (`spec_queue` / `collect_spec_bound_calls` in
//! `emit.rs`), so the Lua-side plumbing does not reach it -- two populations for
//! one question, which is the shape this arc keeps finding. Both now carry the
//! demand's `ResolvedBoundTree` and drive the one substitution in
//! `skull_transform::bound`.
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

/// Three separate things, for the same reason as the luajit sibling: emission
/// succeeding says nothing (a build that dropped `total` also reports no error),
/// a definition existing is checked on the DEFINITION line rather than on the
/// call, and the placeholder having been RESOLVED is checked by the concrete
/// `next` inside that definition -- which is the whole content of D-1.
#[test]
fn a_bounded_body_emits_standalone_to_js_with_inlining_disabled() {
    let _g = ModeGuard::on();
    let js = emit_js(&ws_with(BOUNDED))
        .expect("a bounded body must emit standalone to JS under its demand");
    let def = js
        .lines()
        .find(|l| l.trim_start().starts_with("function total_") && l.contains("_spec_"))
        .unwrap_or_else(|| panic!("no standalone specialized definition of `total` in:\n{js}"));
    let open = def.find('(').expect("a definition header has params");
    let symbol = def.trim_start()["function ".len()..open - (def.len() - def.trim_start().len())]
        .to_string();
    let body: Vec<&str> = js
        .lines()
        .skip_while(|l| *l != def)
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("function "))
        .collect();
    assert!(
        body.iter().any(|l| l.contains("next_")),
        "the standalone `total` calls no concrete `next`, so its placeholder \
         bound-call was not resolved: {body:?}"
    );
    assert!(
        js.lines().any(|l| !l.trim_start().starts_with("function ") && l.contains(&format!("{symbol}("))),
        "`{symbol}` is emitted but nothing calls it"
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
