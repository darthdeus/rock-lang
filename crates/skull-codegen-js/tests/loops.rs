//! Loop recovery tests (docs/backends.md). The emitter
//! recovers single-entry natural loops from MIR back-edges: a range-`for`
//! (`loop_binding` slot + pure-increment latch) becomes a real JS
//! `for (; slot < bound; slot = slot + 1)`, every other natural loop a
//! `while (true) { <cond>; if (!c) break; <body> }`, modeled on the GLSL
//! relooper. These pin the emitted structure exactly (the `emit.rs` style); the
//! behavioral oracle is the `--backend js` suite. Reducible-but-unstructured
//! shared-tail branches (a `match` range pattern) keep rejecting.
//!
//! The load-bearing property is T3 point 3: the ordered-temp value model stays
//! correct inside a loop body -- a cell written in iteration N and read in N+1
//! re-materializes each pass (`let v<k> = c<cell>` INSIDE the loop), never
//! folding across the back-edge. Every accumulator `let v = c<acc>` below sits
//! inside the loop braces.
use crate::common::{emit_user, ws_with};

// General `while` with a manual counter (NOT a range-for). The accumulator c1
// and counter c2 are read+written across the back-edge; each iteration
// re-reads them via fresh `let v = c1;` / `let v = c2;` inside the loop.
#[test]
fn while_accumulator() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            let acc = 0
            let i = 0
            while i < n {
                acc = acc + i
                i = i + 1
            }
            acc
        }",
    );
    assert_eq!(
        emit_user(&ws, "f"),
        "\
function f(c0) {
  let c1, c2;
  c1 = 0;
  c2 = 0;
  while (true) {
    let v4 = c2;
    let v5 = c0;
    if (!((v4 < v5))) { break; }
    let v7 = c1;
    let v8 = c2;
    c1 = (v7 + v8);
    let v11 = c2;
    c2 = (v11 + 1);
  }
  let v15 = c1;
  return v15;
}
"
    );
}

// Range-`for` -> a real `for (; slot < bound; slot = slot + 1)`. The bound c3 is
// the once-evaluated preheader cell (Rock range bounds are loop-invariant).
#[test]
fn for_range_counted() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            let acc = 0
            for i in 0..n {
                acc = acc + i
            }
            acc
        }",
    );
    assert_eq!(
        emit_user(&ws, "f"),
        "\
function f(c0) {
  let c1, c2, c3;
  c1 = 0;
  c2 = 0;
  let v4 = c0;
  c3 = v4;
  for (; c2 < c3; c2 = c2 + 1) {
    let v9 = c1;
    let v10 = c2;
    c1 = (v9 + v10);
  }
  let v17 = c1;
  return v17;
}
"
    );
}

// Early `break` carrying a value: the found index is stored before `break`.
#[test]
fn early_break_with_value() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            let found = -1
            for i in 0..n {
                if i == 3 {
                    found = i
                    break
                }
            }
            found
        }",
    );
    assert_eq!(
        emit_user(&ws, "f"),
        "\
function f(c0) {
  let c1, c2, c3;
  c1 = -1;
  c2 = 0;
  let v4 = c0;
  c3 = v4;
  for (; c2 < c3; c2 = c2 + 1) {
    let v9 = c2;
    if ((v9 === 3)) {
      let v12 = c2;
      c1 = v12;
      break;
    }
  }
  let v18 = c1;
  return v18;
}
"
    );
}

// `continue` skipping the accumulator write. The `for` increment still runs on
// `continue` (that is why a range-for uses a real `for`, not `while (true)`).
#[test]
fn continue_skips_write() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            let acc = 0
            for i in 0..n {
                if i == 2 {
                    continue
                }
                acc = acc + i
            }
            acc
        }",
    );
    assert_eq!(
        emit_user(&ws, "f"),
        "\
function f(c0) {
  let c1, c2, c3;
  c1 = 0;
  c2 = 0;
  let v4 = c0;
  c3 = v4;
  for (; c2 < c3; c2 = c2 + 1) {
    let v9 = c2;
    if ((v9 === 2)) {
      continue;
    }
    let v12 = c1;
    let v13 = c2;
    c1 = (v12 + v13);
  }
  let v20 = c1;
  return v20;
}
"
    );
}

// Nested loops with independent (shadowed) counters c2 (outer) and c4 (inner).
// Bare JS `break`/`continue` target the innermost `for`, so no labels are
// needed (Rock has no labeled loop control).
#[test]
fn nested_loops_shadowed_counters() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            let total = 0
            for i in 0..n {
                for j in 0..n {
                    total = total + j
                }
            }
            total
        }",
    );
    assert_eq!(
        emit_user(&ws, "f"),
        "\
function f(c0) {
  let c1, c2, c3, c4, c5;
  c1 = 0;
  c2 = 0;
  let v4 = c0;
  c3 = v4;
  for (; c2 < c3; c2 = c2 + 1) {
    c4 = 0;
    let v11 = c0;
    c5 = v11;
    for (; c4 < c5; c4 = c4 + 1) {
      let v16 = c1;
      let v17 = c4;
      c1 = (v16 + v17);
    }
  }
  let v28 = c1;
  return v28;
}
"
    );
}

// A loop whose condition CALLS a fn: `abs(n)` must re-evaluate every iteration
// (a materialized `let v6 = Math.abs(v5)` inside the loop), never hoisted.
#[test]
fn condition_calls_fn() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            let acc = 0
            let i = 0
            while i < abs(n) {
                acc = acc + i
                i = i + 1
            }
            acc
        }",
    );
    assert_eq!(
        emit_user(&ws, "f"),
        "\
function f(c0) {
  let c1, c2;
  c1 = 0;
  c2 = 0;
  while (true) {
    let v4 = c2;
    let v5 = c0;
    let v6 = Math.abs(v5);
    if (!((v4 < v6))) { break; }
    let v8 = c1;
    let v9 = c2;
    c1 = (v8 + v9);
    let v12 = c2;
    c2 = (v12 + 1);
  }
  let v16 = c1;
  return v16;
}
"
    );
}

// A `match` range pattern lowers to a control-flow short-circuit whose two
// failure branches share a tail block -- reducible but not a natural loop. The
// short-circuit fold recovers it into one `&&` diamond (see tests/match_fold.rs
// for the behavioral coverage); it no longer rejects.
#[test]
fn range_match_folds_to_and_diamond() {
    let ws = ws_with(
        "fn f(n: int) -> int {
            return match n {
                0..10 => 1,
                _ => 9,
            }
        }",
    );
    let js = emit_user(&ws, "f");
    assert!(js.contains("&&"), "range match folds to an && diamond:\n{js}");
    assert!(
        !js.contains("shared branch tail"),
        "no longer rejects:\n{js}"
    );
}



