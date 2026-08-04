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

/// SENTINEL -- asserts the CURRENTLY BROKEN behavior, and it is B1's first RED
/// (`docs/sim-speed-plan.md`).
///
/// A generic fn with an `Iterator` bound has NO standalone emitted definition:
/// `mir_emit`'s fn-def loop skips it (`fn_has_bounds`) because it exists only
/// inlined at its call sites. Its chunk is therefore never pulled, so its
/// `static` cell never reaches `all_statics` -- while the inlined copies at the
/// call sites read and write that very cell at runtime. The cell is live and
/// unregistered.
///
/// This is the DROPPED-DEFINITION shape the plan distinguishes from the two
/// ordinary static rows: those cells are registered and merely not restored,
/// this one cannot be restored because nothing knows it exists.
///
/// FLIP WHEN FIXED: when the collection covers owners with no standalone
/// definition, this becomes `assert_every_live_cell_is_registered(&lua, ...)`
/// in the same commit as the fix, and its name loses `_sentinel`.
#[test]
fn a_bounded_generics_static_is_still_unregistered_sentinel() {
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
    let live = live_cells(&lua);
    let registered = registered_cells(&lua);
    let owned: Vec<&String> = live.iter().filter(|c| c.contains("_tally_")).collect();
    assert_eq!(
        owned.len(),
        1,
        "the fixture must put exactly one bounded-generic cell in the emitted \
         program, or it is not testing what it says: {live:?}"
    );
    assert!(
        !registered.iter().any(|c| c.contains("_tally_")),
        "the bounded generic's static cell is now registered -- the gap is \
         closed, so flip this sentinel to \
         `assert_every_live_cell_is_registered(&lua, ..)` and drop the \
         `_sentinel` suffix. registered: {registered:?}"
    );
}
