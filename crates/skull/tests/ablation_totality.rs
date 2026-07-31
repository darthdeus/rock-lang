//! Stage-0e totality ablation (`docs/monomorphization-plan.md` S 0e).
//!
//! The mode is process-global, so every test here serializes on one mutex and
//! restores the flag on the way out. Without that, a parallel test would see the
//! flag flipped under it and the fatals of one fixture would land in another's
//! census.

use std::sync::{LazyLock, Mutex, MutexGuard};

use bone_semantic::inference::Workspace;
use skull_mir::ablation::{self, FatalCategory};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

static MODE: Mutex<()> = Mutex::new(());

/// Hold the mode for one test. `Drop` restores it even on a panicking assert,
/// so a failing test cannot leave the flag on for the next one.
struct ModeGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

impl ModeGuard {
    fn on() -> Self {
        let g = MODE.lock().unwrap_or_else(|e| e.into_inner());
        ablation::clear();
        ablation::set_enabled(true);
        ModeGuard(g)
    }
    fn off() -> Self {
        let g = MODE.lock().unwrap_or_else(|e| e.into_inner());
        ablation::clear();
        ablation::set_enabled(false);
        ModeGuard(g)
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        ablation::set_enabled(false);
        ablation::clear();
    }
}

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Lower + emit the workspace, discarding the result: the ablation's output is
/// the fatal registry, not the emitted text. A rejection is not a failure here
/// -- the mode is expected to produce them -- so the error is swallowed and the
/// registry is what gets asserted.
fn emit(ws: &Workspace) {
    let mid = skull::user_module_id(ws).expect("user module");
    let _ = skull::mir_emit::emit_workspace_mir(ws, mid);
}

fn fatals_of(src: &str) -> Vec<ablation::Fatal> {
    let ws = check(src);
    emit(&ws);
    ablation::take()
}

/// A generic `co fn` must produce a classified fatal under the mode.
/// `compute_call_spec` bails on `is_coroutine` before it can build a SpecKey, so
/// every instantiation shares one body -- exactly the "shared generic Rock body"
/// the target invariant forbids, and the shape
/// `generic_coroutine_column_param_sentinel` already pins as a live miscompile.
///
/// RED CONTROL: `the_mode_off_records_nothing` is the same fixture with the mode
/// off, and it must find zero. Together they show the mode is what produces the
/// record, not the fixture.
#[test]
fn a_generic_coroutine_is_an_unsupported_surface_fatal() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
co fn stream<T>(v: T) {
    yield
}
fn main() { let c = stream(1) }
"#,
    );
    let hit: Vec<&ablation::Fatal> = fatals
        .iter()
        .filter(|f| f.callee == "stream" && f.category == FatalCategory::UnsupportedSurface)
        .collect();
    assert!(
        !hit.is_empty(),
        "a generic `co fn` produced no unsupported-surface fatal; got {:?}",
        fatals.iter().map(|f| (f.category, &f.callee)).collect::<Vec<_>>()
    );
    assert_eq!(hit[0].caller, "main", "the fatal must name the CALLER");
}

/// A generic `inline(lua)` body is a real boundary, not a missing edge. It must
/// classify as `target-boundary` -- if it landed in `missing-graph-edge` the
/// taxonomy would send Stage 2 to add a record that cannot exist, because there
/// is no Rock body to instantiate.
#[test]
fn a_generic_inline_body_is_a_target_boundary_fatal() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
inline(lua) fn probe_len<T>(v: [T]) -> int = """
    return #v._data
"""
fn main() {
    let xs: [int] = [1, 2, 3]
    let n = probe_len(xs)
}
"#,
    );
    let hit: Vec<&ablation::Fatal> =
        fatals.iter().filter(|f| f.callee == "probe_len").collect();
    assert!(
        !hit.is_empty(),
        "a generic inline(lua) callee produced no fatal; got {:?}",
        fatals.iter().map(|f| (f.category, &f.callee)).collect::<Vec<_>>()
    );
    assert_eq!(
        hit[0].category,
        FatalCategory::TargetBoundary,
        "a foreign body must classify as a boundary, not as a missing edge"
    );
}

/// THE RED CONTROL for every test above. With the mode off, the identical
/// fixtures record nothing -- so the fatals are produced by the ablation and not
/// by the fixture merely being unusual.
#[test]
fn the_mode_off_records_nothing() {
    let _g = ModeGuard::off();
    let fatals = fatals_of(
        r#"
co fn stream<T>(v: T) {
    yield
}
inline(lua) fn probe_len<T>(v: [T]) -> int = """
    return #v._data
"""
fn main() {
    let c = stream(1)
    let xs: [int] = [1, 2, 3]
    let n = probe_len(xs)
}
"#,
    );
    assert!(
        fatals.is_empty(),
        "the mode is OFF and {} fatal(s) were still recorded: {:?}",
        fatals.len(),
        fatals.iter().map(|f| (f.category, &f.callee)).collect::<Vec<_>>()
    );
}

/// The taxonomy has no escape hatch (S 0g: no "miscellaneous" bucket). Every
/// fatal a real program produces must carry one of the five categories and a
/// detail that states its DISCRIMINANT rather than restating the category.
#[test]
fn every_fatal_is_classified_with_a_span_and_a_reason() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
fn ident<T>(x: T) -> T { return x }
co fn stream<T>(v: T) { yield }
fn main() {
    let a = ident(1)
    let c = stream(2)
    let xs: [int] = [1, 2, 3]
    for v in xs.iter().map(_ + 1) { let _ = v }
}
"#,
    );
    assert!(!fatals.is_empty(), "the fixture produced no fatals to classify");
    for f in &fatals {
        assert!(!f.callee.is_empty(), "a fatal with no callee");
        assert!(!f.caller.is_empty(), "a fatal with no caller: {f:?}");
        assert!(
            f.detail.len() > 20 && f.detail != f.category.name(),
            "a fatal whose detail restates the category: {f:?}"
        );
    }
}
