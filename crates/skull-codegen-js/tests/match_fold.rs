//! Short-circuit fold + exhaustive-match structuring (T6). A `match` range/tuple
//! pattern lowers to nested branches whose failure arms share one tail block; the
//! fold collapses the chain into one `&&`/`||` diamond (a folded inner block's
//! pure projections hoist before the `if`). An EXHAUSTIVE match (enum, or a
//! literal set with a proven-dead default) ends its impossible arm in
//! `Term::Unreachable`, which -- unlike a `Return` -- does NOT connect to the
//! post-dom virtual exit, so the real return block stays the join instead of the
//! chain double-walking it. The node-executed checks are the correctness oracle.

use std::process::Command;
use crate::common::{emit_user, ws_with};

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn node_eval_int(defs: &str, call: &str) -> i64 {
    let program = format!("{defs}\nconsole.log({call});");
    let out = Command::new("node").arg("-e").arg(&program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<i64>().unwrap_or_else(|_| panic!("node did not print an int: {s:?}\n{program}"))
}

/// Emit `f` and evaluate `f(arg)` under node (skips if node is absent).
fn eval1(src: &str, arg: i64, expect: i64) {
    if !node_available() {
        eprintln!("SKIP: node not found");
        return;
    }
    let ws = ws_with(src);
    assert_eq!(node_eval_int(&emit_user(&ws, "f"), &format!("f({arg})")), expect);
}

#[test]
fn range_match_two_arm() {
    let src = "fn f(x: int) -> int { match x { 1..5 => 10, _ => 20 } }";
    eval1(src, 3, 10);
    eval1(src, 9, 20);
    eval1(src, 5, 20); // exclusive upper bound
    eval1(src, 1, 10); // inclusive lower bound
}

#[test]
fn range_match_three_arm() {
    let src = "fn f(x: int) -> int { match x { 1..5 => 10, 6..10 => 20, _ => 30 } }";
    eval1(src, 3, 10);
    eval1(src, 7, 20);
    eval1(src, 100, 30);
    eval1(src, 5, 30); // between the two ranges (5 and also not 6..10)
}

#[test]
fn tuple_pattern_folds() {
    // The inner `tuple_index` projection is hoisted before the folded `&&`.
    let src = "fn f(a: int) -> int { match (a, a + 1) { (1, 2) => 100, _ => 200 } }";
    eval1(src, 1, 100);
    eval1(src, 5, 200);
}

#[test]
fn exhaustive_enum_chain() {
    // No `_` arm: the impossible fourth case is `Term::Unreachable`. Without the
    // post-dom fix the return block double-walks and rejects.
    let src = "enum E { A, B, C }\n\
               fn f(x: int) -> int {\n\
                   let e = if x == 0 { E::A } else { if x == 1 { E::B } else { E::C } }\n\
                   match e { E::A => 1, E::B => 2, E::C => 3 }\n\
               }";
    eval1(src, 0, 1);
    eval1(src, 1, 2);
    eval1(src, 2, 3);
}

#[test]
fn literal_set_match() {
    let src = "fn f(x: int) -> int { match x { 1 => 100, 2 => 200, 3 => 300, _ => 0 } }";
    eval1(src, 1, 100);
    eval1(src, 2, 200);
    eval1(src, 3, 300);
    eval1(src, 9, 0);
}
