//! Loop-body value temps must be declared INSIDE the emitted `for ... do` block,
//! not in the function-top `local r...` line.
//!
//! Why: LuaJIT prunes snapshot slots above a jump's free-slot watermark
//! (`snap_usedef`, lj_snap.c). Top-declared temps sit BELOW the `for` loop's
//! control slots, so every loop snapshot keeps them alive forever; a side trace
//! at the loop exit then inherits every distinct temp value, and past ~16 live FP
//! values `asm_head_side` hard-aborts with "NYI: register coalescing too complex"
//! (no spill fallback), blacklisting the trace tree. Body-scoped `local`s sit
//! ABOVE the loop control slots and get pruned, collapsing the inherited set to
//! the genuinely-live values. Standalone repro + in-game abort sites:
//! `benchmarks/nyicoal/`, `docs/gc-pressure-reduction-plan.md` item 1.
use crate::common::{emit_module, function_body, run_luajit};

/// Multi-use loop-body temps (`t1`/`t2` below are used twice, so they are not
/// tree-folded and need real `r<N>` registers) get their `local` declaration
/// inside the `for` block.
#[test]
fn loop_body_temps_declared_inside_for() {
    let src = r#"
fn consume(a: float, b: float, c: float, d: float) -> float { return a + b + c + d }

fn hot(x: float, y: float, n: int) -> float {
    let acc = 0.0
    for i in 0..n {
        let t1 = x * 2.0 + y
        let t2 = y * 3.0 + x
        acc = acc + consume(t1, t2, t1 * t2, t1 - t2)
    }
    return acc
}

fn main() { print("{hot(1.0, 2.0, 8)}") }
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "hot");
    let for_pos = body.find(" for ").unwrap_or_else(|| panic!("no for loop in:\n{body}"));
    let after_for = &body[for_pos..];
    let decl = after_for
        .lines()
        .skip(1)
        .find(|l| l.trim_start().starts_with("local r"))
        .unwrap_or_else(|| panic!("no in-loop `local r` decl after the for:\n{body}"));
    // The in-loop decl must precede the first body label/instruction.
    let decl_pos = after_for.find(decl.trim_start()).unwrap();
    let label_pos = after_for.find("::bb").expect("body label");
    assert!(
        decl_pos < label_pos,
        "in-loop `local r` must come before the body label:\n{body}"
    );
}

/// A temp live across the loop (defined before, used after) must stay in the
/// function-top declaration, NOT move inside the loop.
#[test]
fn cross_loop_temp_stays_at_function_top() {
    let src = r#"
fn consume(a: float, b: float) -> float { return a + b }

fn hot(x: float, y: float, n: int) -> float {
    let acc = 0.0
    let pre = x * 7.0 + y
    for i in 0..n {
        let t1 = x * 2.0 + y
        acc = acc + consume(t1, t1 * 2.0)
    }
    return acc + pre * pre
}

fn main() { print("{hot(1.0, 2.0, 8)}") }
"#;
    let lua = emit_module(src);
    let body = function_body(&lua, "hot");
    // `pre` is multi-use (pre * pre after the loop): a real register whose value
    // crosses the loop. Every rN referenced after the loop's `end` must be
    // declared at fn top (the first `local` line), else it reads nil.
    let top_decl = body
        .lines()
        .find(|l| l.trim_start().starts_with("local "))
        .expect("top local line")
        .to_string();
    let loop_end = body.rfind("  end").expect("loop end");
    let tail = &body[loop_end..];
    for tok in tail.split(|c: char| !c.is_alphanumeric()) {
        if tok.starts_with('r') && tok[1..].chars().all(|c| c.is_ascii_digit()) && tok.len() > 1 {
            assert!(
                top_decl.contains(tok),
                "`{tok}` used after the loop but not declared at fn top.\ntop: {top_decl}\ntail: {tail}\nbody:\n{body}"
            );
        }
    }
}

#[test]
fn compound_while_loop_carried_field_loads_and_runs() {
    let src = r#"
struct Selection { count: int }
fn select(c: Selection, capacity: int, budget: int) -> int {
    let slot = 0
    while slot < capacity && c.count < budget {
        if slot % 2 == 0 { c.count += 1 }
        slot += 1
    }
    return c.count
}
fn main() {
    print("{select(Selection { count: 0 }, 8, 2)}")
    print("{select(Selection { count: 0 }, 3, 9)}")
    print("{select(Selection { count: 0 }, 0, 9)}")
    print("{select(Selection { count: 0 }, 8, 0)}")
}
"#;
    let lua = emit_module(src);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "compound_while_loop_carried"), "2\n2\n0\n0");
}

#[test]
fn compound_while_preserves_short_circuit_effects_inside_for() {
    let src = r#"
struct Probe { count: int, checks: int }
fn below(p: Probe, budget: int) -> bool {
    p.checks += 1
    return p.count < budget
}
fn scan(p: Probe, capacity: int, budget: int) {
    for round in 0..2 {
        let slot = 0
        while slot < capacity && below(p, budget) {
            p.count += 1
            slot += 1
        }
    }
}
fn either(p: Probe, capacity: int, budget: int) {
    let slot = 0
    while slot < capacity || below(p, budget) {
        p.count += 1
        slot += 1
    }
}
fn main() {
    let p = Probe { count: 0, checks: 0 }
    scan(p, 0, 9)
    print("{p.count} {p.checks}")
    scan(p, 4, 2)
    print("{p.count} {p.checks}")
    either(p, 3, 6)
    print("{p.count} {p.checks}")
}
"#;
    let lua = emit_module(src);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "compound_while_effects"), "0 0\n2 4\n6 6");
}
