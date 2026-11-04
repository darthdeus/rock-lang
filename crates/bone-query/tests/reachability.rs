//! Unit tests for the type-reachability index that backs type-directed
//! completion paths (`bone_query::reachability`). The integration behaviour
//! (what completion offers) is in `completion_contexts.rs`; these pin the
//! graph-search invariants directly: diamond fan-out, cycle termination, and
//! the methods-with-args terminal-only rule. See
//! `crates/bone-query/src/reachability.rs`.

use bone_query::reachability::ReachabilityIndex;
use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::semantic_pass;

fn index_of(src: &str) -> ReachabilityIndex {
    let module = skull_parser::parse_module_clean(src, Ustr::from("<reach-test>")).expect("parse");
    let ws = semantic_pass(&mut [module]).expect("semantic_pass");
    ReachabilityIndex::build(&ws)
}

/// Sorted `suffix` strings of the paths from `root` to `expected`.
fn suffixes(idx: &ReachabilityIndex, root: &str, expected: &[&str]) -> Vec<String> {
    let exp: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    let mut s: Vec<String> = idx
        .paths_to(root, &exp, 64)
        .paths
        .into_iter()
        .map(|p| p.suffix)
        .collect();
    s.sort();
    s
}

#[test]
fn diamond_yields_both_paths() {
    // T reaches E two independent ways (T.a.e and T.b.e). Memoizing the
    // reachable SET does not collapse this fan-out -- both paths must surface.
    let src = "struct E { v: int }\n\
               struct A { e: E }\n\
               struct B { e: E }\n\
               struct T { a: A, b: B }\n";
    let idx = index_of(src);
    assert_eq!(
        suffixes(&idx, "T", &["E"]),
        vec!["a.e".to_string(), "b.e".to_string()]
    );
}

#[test]
fn self_cycle_terminates_and_stays_simple() {
    // A self-referential type must not loop forever. The simple-path cycle guard
    // (no type repeats on a path) yields the direct field once and refuses to
    // re-enter the type -- so `leaf`, never `next.leaf` / `next.next.leaf`.
    let src = "struct Leaf { v: int }\n\
               struct Node { next: Node?, leaf: Leaf }\n";
    let idx = index_of(src);
    assert_eq!(suffixes(&idx, "Node", &["Leaf"]), vec!["leaf".to_string()]);
}

#[test]
fn mutual_cycle_terminates() {
    // Mutually-recursive types (A <-> B) must also terminate. From A you can
    // reach Tag via `tag` directly or `b.tag`; re-entering A from B is blocked.
    let src = "struct Tag { v: int }\n\
               struct A { b: B?, tag: Tag }\n\
               struct B { a: A?, tag: Tag }\n";
    let idx = index_of(src);
    assert_eq!(
        suffixes(&idx, "A", &["Tag"]),
        vec!["b.tag".to_string(), "tag".to_string()]
    );
}

#[test]
fn method_with_args_is_terminal_not_traversed() {
    // `build(w: W, scale: int) -> V` is a UFCS method on W needing an extra arg.
    // It is offered as a LAST hop (you'd fill `($0)`), but never traversed
    // through -- you cannot park a snippet hole mid-path. The plain field `v`
    // also reaches V.
    let src = "struct V { x: int }\n\
               struct W { v: V }\n\
               fn build(w: W, scale: int) -> V { w.v }\n";
    let idx = index_of(src);
    let res = idx.paths_to("W", &["V".to_string()], 64);
    let mut by_suffix: Vec<(String, bool, bool)> = res
        .paths
        .iter()
        .map(|p| (p.suffix.clone(), p.is_method, p.takes_args))
        .collect();
    by_suffix.sort();
    assert_eq!(
        by_suffix,
        vec![
            ("build".to_string(), true, true),
            ("v".to_string(), false, false)
        ],
    );
}
