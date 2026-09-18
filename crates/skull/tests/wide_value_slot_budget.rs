//! A function that holds several wide `nobox` values must still load: LuaJIT
//! caps a function frame at 250 slots (`LJ_MAX_SLOTS`, "function or expression
//! too complex"), and a 50-leaf value costs 50 of them wherever it is live.
//!
//! The receive of a wide multi-return used to cost the width twice: the call
//! wrote its results into one set of locals and every leaf was then copied into
//! a second set that held the value. With three 50-leaf values live and a
//! 52-slot call on top, that copy was the difference between 202 slots and 252.
//! The result locals now ARE the value's locals.

use crate::common::{function_body, run_luajit, try_emit_module};

/// `held` values of a `leaves`-wide nobox struct, all live across the calls
/// that read them back.
fn fixture(leaves: usize, held: usize) -> String {
    let fields: Vec<String> = (0..leaves).map(|k| format!("f{k}: float")).collect();
    let inits: Vec<String> = (0..leaves).map(|k| format!("f{k}: x + {k}.0")).collect();
    let copies: Vec<String> = (1..leaves).map(|k| format!("f{k}: w.f{k}")).collect();
    let mut src = format!(
        "nobox value W {{ {} }}\n\
         fn mk(x: float) -> W {{ return W {{ {} }} }}\n\
         fn tweak(w: W) -> W {{ return W {{ f0: w.f0 + 1.0, {} }} }}\n\
         fn check(w: W, k: int) -> float {{ return w.f0 + w.f{} + float(k) }}\n\
         fn run(x: float) -> float {{\n    let v0 = mk(x)\n",
        fields.join(", "),
        inits.join(", "),
        copies.join(", "),
        leaves - 1,
    );
    for i in 1..held {
        src.push_str(&format!("    let v{i} = tweak(v{})\n", i - 1));
    }
    src.push_str("    let total = 0.0\n");
    for i in 0..held {
        src.push_str(&format!("    total += check(v{i}, {i})\n"));
    }
    src.push_str("    return total\n}\nfn main() { print(\"{run(1.0)}\") }\n");
    src
}

#[test]
fn three_live_fifty_leaf_values_load_and_run() {
    let lua = try_emit_module(&fixture(50, 3)).expect("emits");
    // v0: 1 + 50, v1: 2 + 50 + 1, v2: 3 + 50 + 2.
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "wide_slot_budget_3x50"), "159");
}

#[test]
fn a_wide_multi_return_lands_in_the_locals_that_hold_the_value() {
    let lua = try_emit_module(&fixture(50, 2)).expect("emits");
    let run = function_body(&lua, "run");
    let receive = run
        .lines()
        .position(|line| line.contains("= mk_"))
        .unwrap_or_else(|| panic!("no receive of mk in:\n{run}"));
    let next = run.lines().nth(receive + 1).unwrap_or("");
    assert!(
        !(next.trim_start().starts_with('r') && next.contains("= c")),
        "the receive is copied leaf by leaf into a second set of locals:\n{run}"
    );
}
