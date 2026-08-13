//! Stage-0e totality ablation (`docs/monomorphization-plan.md` S 0e).
//!
//! The mode is process-global, so every test here holds `common::AblationGuard`
//! and restores the flag on the way out. That guard's lock is shared with every
//! other file in this binary that touches the mode -- a per-file `static MODE`
//! serializes a file against itself and against nothing else.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull_mir::ablation::{self, FatalCategory};
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

/// An `Iterator`-bounded body has a concrete form now (plan S 12, mono cut C+D
/// step D-1): it is emitted STANDALONE at each instantiation, with its
/// placeholder `next` calls resolved from the tree its specialization demand
/// carries. So under the mode it must record nothing at all.
///
/// The growth call is the ADMISSION CONTROL, and it is a `polymorphic-growth`
/// one on purpose: that is an S 0h REJECT, permanent by decision, so it cannot
/// close underneath this fixture the way the bounded surface just did -- which
/// is exactly how this test's own control rotted (it was `total` itself).
#[test]
fn a_bounded_body_is_no_longer_an_unsupported_surface() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}
fn main() {
    depth(1, 12)
    let xs: [int] = [1, 2, 3]
    let n = total(xs.iter())
}
"#,
    );
    assert!(
        fatals.iter().any(|f| f.category == FatalCategory::PolymorphicGrowth),
        "the control did not fire, so this fixture proves nothing about `total`; got {:?}",
        fatals.iter().map(|f| (f.category, &f.callee)).collect::<Vec<_>>()
    );
    let hit: Vec<&ablation::Fatal> = fatals.iter().filter(|f| f.callee == "total").collect();
    assert!(
        hit.is_empty(),
        "an `Iterator`-bounded body is emitted standalone now, so a call to one \
         must record no fatal; got {:?}",
        hit.iter().map(|f| (f.category, f.detail.as_str())).collect::<Vec<_>>()
    );
}

/// The other half of the same axis, and the reason the control above moved: a
/// generic `co fn` USED to be the unsupported surface. D-3 gave it a concrete
/// form -- a standalone specialized definition through `coro_transform` -- so
/// under the mode it must now record nothing at all.
///
/// The growth call in the same fixture is the admission control: without it a
/// zero for `stream` would be indistinguishable from a fixture the ablation
/// never saw. It is a `polymorphic-growth` call because that is an S 0h REJECT
/// and therefore permanent -- the bounded call that used to stand here stopped
/// firing the moment D-1 landed.
#[test]
fn a_generic_coroutine_is_no_longer_an_unsupported_surface() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
co fn stream<T>(v: T) {
    yield
}
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn main() {
    let c = stream(1)
    depth(1, 12)
}
"#,
    );
    assert!(
        fatals.iter().any(|f| f.category == FatalCategory::PolymorphicGrowth),
        "the control did not fire, so this fixture proves nothing about `stream`; got {:?}",
        fatals.iter().map(|f| (f.category, &f.callee)).collect::<Vec<_>>()
    );
    let hit: Vec<&ablation::Fatal> = fatals.iter().filter(|f| f.callee == "stream").collect();
    assert!(
        hit.is_empty(),
        "a generic `co fn` specializes now, so a call to one must record no \
         fatal; got {:?}",
        hit.iter().map(|f| (f.category, f.detail.as_str())).collect::<Vec<_>>()
    );
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
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}
inline(lua) fn probe_len<T>(v: [T]) -> int = """
    return #v._data
"""
fn main() {
    let xs: [int] = [1, 2, 3]
    let t = total(xs.iter())
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
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn main() {
    let a = ident(1)
    let c = stream(2)
    let xs: [int] = [1, 2, 3]
    for v in xs.iter().map(_ + 1) { let _ = v }
    depth(1, 12)
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

/// The `polymorphic-growth` detector must FIRE. Round 1 reported that category
/// as zero with no detector behind it, which is not a measurement -- so the
/// detector exists now, and this is what makes its zero on the corpus mean
/// something.
///
/// `depth<T>` wraps its own argument (`f<T> -> f<[T]>`), so each recursive call
/// instantiates one level deeper. Plan S 7 records that this shape compiles and
/// RUNS today, which is why rejecting it is a surface removal rather than a
/// no-op -- and why it needs a detector rather than an assumption.
#[test]
fn polymorphic_growth_is_detected() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn main() { depth(1, 12) }
"#,
    );
    let growth: Vec<&ablation::Fatal> = fatals
        .iter()
        .filter(|f| f.category == FatalCategory::PolymorphicGrowth)
        .collect();
    assert!(
        !growth.is_empty(),
        "polymorphic recursion produced no growth fatal; categories seen: {:?}",
        fatals.iter().map(|f| f.category).collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(growth[0].callee, "depth", "the growth fatal must name the growing callee");
}

/// The bounding CONTROL for the detector: an ordinary nested container must NOT
/// trip it. Without this the growth detector could be firing on depth alone and
/// the corpus zero would be luck rather than a property.
#[test]
fn ordinary_nesting_is_not_polymorphic_growth() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let xs: [[int]] = [[1, 2], [3]]
    let a = ident(xs)
}
"#,
    );
    let growth: Vec<&ablation::Fatal> = fatals
        .iter()
        .filter(|f| f.category == FatalCategory::PolymorphicGrowth)
        .collect();
    assert!(
        growth.is_empty(),
        "an ordinary nested container was misfiled as polymorphic growth: {growth:?}"
    );
}

/// A generic call whose binding is `unit`, `any` or a function type is NOT a
/// fatal: those are valid logical types with a `MonoTy` constructor apiece, so
/// the key names them and each call has its own instantiation (plan S 2a/S 2b).
///
/// This test used to assert the opposite, and that is what it was for. While the
/// key was built by a second, narrower walk than the identity oracle, such a slot
/// was `None` -- and `None` is the absence of a type, shared by every binding
/// that landed in it, so `f<Vec2, unit>`, `f<Vec2, any>` and
/// `f<Vec2, fn(int)->int>` all mangled to one symbol. Both walks are now
/// [`skull_mir::monoty::convert`], so the category has no population by
/// construction rather than by policy.
///
/// The `depth` call is the ADMISSION CONTROL, and it is what makes `ident`'s
/// zero mean something: polymorphic growth is an S 0h REJECT and therefore
/// permanent, so the mode is demonstrably live over this very program. Without
/// it a zero here would be indistinguishable from a fixture the ablation never
/// saw. It used to be an `Iterator`-bounded call, which D-1 closed.
/// The emitted-identity half -- that the three calls name three different bodies
/// -- is `valid_type_identity.rs`; this asserts only that they stopped being
/// reported as unrepresentable.
#[test]
fn an_unadmitted_binding_is_no_longer_a_fatal() {
    let _g = ModeGuard::on();
    let fatals = fatals_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn nothing() {}
fn twice(n: int) -> int { return n * 2 }
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn main() {
    let u = ident(nothing())
    let a: any = 1
    let b = ident(a)
    let f: fn(int) -> int = twice
    let g = ident(f)
    depth(1, 12)
}
"#,
    );
    let control: Vec<&ablation::Fatal> = fatals
        .iter()
        .filter(|f| f.category == FatalCategory::PolymorphicGrowth)
        .collect();
    assert!(
        !control.is_empty(),
        "the control did not fire, so this fixture proves nothing about `ident`: \
         polymorphic growth must still be a fatal; got {:?}",
        fatals.iter().map(|f| (f.category, &f.callee)).collect::<Vec<_>>()
    );
    let hit: Vec<&ablation::Fatal> = fatals.iter().filter(|f| f.callee == "ident").collect();
    assert!(
        hit.is_empty(),
        "`unit`, `any` and `fn(int) -> int` are valid types the key now names, so a call \
         bound to one must record no fatal; got {:?}",
        hit.iter().map(|f| (f.category, f.detail.as_str())).collect::<Vec<_>>()
    );
}
