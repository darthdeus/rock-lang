//! Every `static` cell the emitted program actually USES must be registered.
//!
//! Registration is what makes a cell visible to hot reload's reset/preserve
//! guards today, and to the worker boot checkpoint that `docs/sim-speed-plan.md`
//! B1 builds on the same collection. Both consumers can only act on cells the
//! registry names, so an unregistered live cell is invisible state.
//!
//! The check is CROSS-VIEW: one view is the set of `__static_*` cells appearing
//! in emitted bodies, the other is the registry table `__rock_known_globals`.
//! Neither is derived from the other -- the first comes from the Lua text the
//! program will run, the second from `all_statics` in
//! `crates/skull/src/mir_emit.rs` -- so a collection that misses an owner shows
//! up as a cell in one view and not the other. A count would not do: cells are
//! named, and the interesting failure is one specific owner going missing.

use crate::common::emit_module;
use std::collections::BTreeSet;

/// Cells referenced anywhere in the emitted program, registry line excluded.
fn live_cells(lua: &str) -> BTreeSet<String> {
    let registry = registry_line(lua);
    let mut out = BTreeSet::new();
    for (i, _) in lua.match_indices("__static_") {
        // The registry table is the OTHER view; counting it here would make the
        // comparison compare a thing with itself.
        if registry.as_ref().is_some_and(|r| lua[i..].starts_with(r.as_str())) {
            continue;
        }
        let rest = &lua[i..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        // The done flag is registered under its own key alongside the cell, so
        // both are tracked; normalize to the cell so a failure names one thing.
        out.insert(name.trim_end_matches("__done").to_string());
    }
    out
}

fn registry_line(lua: &str) -> Option<String> {
    let start = lua.find("_G.__rock_known_globals = {")?;
    let rest = &lua[start..];
    let end = rest.find("}\n")?;
    Some(rest[..end].to_string())
}

/// Cells the registry names.
fn registered_cells(lua: &str) -> BTreeSet<String> {
    let line = registry_line(lua).expect("the emitted program must register its globals");
    let mut out = BTreeSet::new();
    for (i, _) in line.match_indices("[\"__static_") {
        let rest = &line[i + 2..];
        let end = rest.find('"').expect("a quoted registry key");
        out.insert(rest[..end].trim_end_matches("__done").to_string());
    }
    out
}

fn assert_every_live_cell_is_registered(lua: &str, what: &str) {
    let live = live_cells(lua);
    let registered = registered_cells(lua);
    assert!(
        !live.is_empty(),
        "{what}: no static cell reached the emitted program, so this compared \
         nothing -- the fixture is not exercising a static at all"
    );
    let missing: Vec<&String> = live.difference(&registered).collect();
    assert!(
        missing.is_empty(),
        "{what}: these cells are live in the emitted program but absent from \
         `__rock_known_globals`, so no reset/preserve guard and no boot \
         checkpoint can see them: {missing:?}\nlive: {live:?}\nregistered: {registered:?}"
    );
}

/// The control: an ordinary fn's static, whose definition is emitted standalone
/// and whose chunk therefore reaches `all_statics`. This is the case that has
/// always worked, and it is here so a failure in the cases below cannot be read
/// as "the check itself is wrong".
#[test]
fn an_ordinary_functions_static_is_registered() {
    let lua = emit_module(
        r#"
global RESULT: int = 0
fn hits() -> int {
    static n = 0
    n = n + 1
    return n
}
fn main() { RESULT = hits() }
"#,
    );
    assert_every_live_cell_is_registered(&lua, "an ordinary fn's static");
}

/// The dropped-definition shape: a generic fn with an `Iterator` bound has NO
/// standalone emitted definition (`mir_emit`'s fn-def loop skips it) and exists
/// only inlined at its call sites. Its cell used to miss `all_statics`
/// entirely, so nothing could reset, preserve or checkpoint it while the
/// inlined copies read and wrote it every call.
///
/// The inliner now carries a spliced callee's `statics` into the body it
/// splices into, which is where those cells actually live. Was a sentinel; the
/// fix and this assertion landed together (`docs/sim-speed-plan.md` B1).
#[test]
fn a_bounded_generics_static_is_registered_through_its_call_sites() {
    let lua = emit_module(
        r#"
global RESULT: int = 0
fn tally<I: Iterator<int>>(it: I) -> int {
    static calls = 0
    calls = calls + 1
    let n = 0
    for x in it { n = n + x }
    return n + calls
}
fn main() { RESULT = tally([1, 2, 3].iter()) }
"#,
    );
    assert!(
        live_cells(&lua).iter().any(|c| c.contains("_tally_")),
        "the fixture must put the bounded generic's cell in the emitted program, \
         or it is not testing what it says: {:?}",
        live_cells(&lua)
    );
    assert_every_live_cell_is_registered(&lua, "a bounded generic's static");
}

/// A `co fn` cannot call a bounded fn, so real sim code reaches one through an
/// ordinary wrapper -- the shape `games/testbed/tests/sims/b1_static_dropped.rock`
/// uses. The cell belongs to the bounded owner either way (a `static` is one
/// cell per owning fn), so the wrapper is where it must be registered from.
#[test]
fn a_bounded_generic_reached_through_a_wrapper_is_registered() {
    let lua = emit_module(
        r#"
global RESULT: int = 0
fn tally<I: Iterator<int>>(it: I) -> int {
    static calls = 0
    calls = calls + 1
    let n = 0
    for x in it { n = n + x }
    return n + calls
}
fn wrapper(a: int, b: int) -> int { return tally([a, b].iter()) }
fn main() { RESULT = wrapper(1, 2) }
"#,
    );
    assert_every_live_cell_is_registered(&lua, "a bounded generic behind a wrapper");
}

/// One cell per owning fn, not per call site: a second call site must not mint
/// a second cell, and registration must not report the same one twice.
#[test]
fn two_call_sites_share_one_registered_cell() {
    let lua = emit_module(
        r#"
global RESULT: int = 0
fn tally<I: Iterator<int>>(it: I) -> int {
    static calls = 0
    calls = calls + 1
    let n = 0
    for x in it { n = n + x }
    return n + calls
}
fn a() -> int { return tally([1].iter()) }
fn b() -> int { return tally([2, 3].iter()) }
fn main() { RESULT = a() + b() }
"#,
    );
    let live: Vec<String> = live_cells(&lua).into_iter().filter(|c| c.contains("_tally_")).collect();
    assert_eq!(live.len(), 1, "two call sites, one cell: {live:?}");
    assert_every_live_cell_is_registered(&lua, "a bounded generic called twice");

    let line = registry_line(&lua).expect("registry");
    let cell = &live[0];
    assert_eq!(
        line.matches(&format!("[{cell:?}]")).count(),
        1,
        "the cell must be registered exactly once, not once per splice"
    );
}

/// A specialized instantiation shares its source fn's cell name (the owner tag
/// is the fn name plus its DECLARED param types, not the substitution), so this
/// is the case that would break if a per-instance owner tag were ever
/// introduced without extending the collection to `emit_specialized_callees`.
#[test]
fn a_specialized_generics_static_is_registered() {
    let lua = emit_module(
        r#"
value V2 { x: float, y: float }
global RESULT: float = 0.0

@inline fn tagged<T>(v: T) -> T {
    static seen = 0
    seen = seen + 1
    return v
}

fn use_it(a: V2) -> float {
    let r = tagged(a)
    return r.x
}

fn main() { RESULT = use_it(V2 { x: 1.0, y: 2.0 }) }
"#,
    );
    assert!(
        live_cells(&lua).iter().any(|c| c.contains("_tagged_")),
        "the fixture must reach the generic's static: {:?}",
        live_cells(&lua)
    );
    assert_every_live_cell_is_registered(&lua, "a specialized generic's static");
}
