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

#[test]
fn path_search_cases() {
    // (case, source, root, target, sorted (suffix, is_method, takes_args))
    let cases: &[(&str, &str, &str, &str, &[(&str, bool, bool)])] = &[
        // Memoizing the reachable set must not collapse the fan-out.
        (
            "diamond yields both paths",
            "struct E { v: int }\nstruct A { e: E }\nstruct B { e: E }\nstruct T { a: A, b: B }\n",
            "T",
            "E",
            &[("a.e", false, false), ("b.e", false, false)],
        ),
        // No type repeats on a path, so never `next.leaf`.
        (
            "self cycle terminates",
            "struct Leaf { v: int }\nstruct Node { next: Node?, leaf: Leaf }\n",
            "Node",
            "Leaf",
            &[("leaf", false, false)],
        ),
        (
            "mutual cycle terminates",
            "struct Tag { v: int }\nstruct A { b: B?, tag: Tag }\nstruct B { a: A?, tag: Tag }\n",
            "A",
            "Tag",
            &[("b.tag", false, false), ("tag", false, false)],
        ),
        // A method needing more args is offered as a last hop (filled with
        // `($0)`) but never traversed: a snippet hole cannot sit mid-path.
        (
            "method with args is terminal",
            "struct V { x: int }\nstruct W { v: V }\nfn build(w: W, scale: int) -> V { w.v }\n",
            "W",
            "V",
            &[("build", true, true), ("v", false, false)],
        ),
    ];
    for (case, src, root, target, expected) in cases {
        let mut got: Vec<(String, bool, bool)> = index_of(src)
            .paths_to(root, &[target.to_string()], 64, None)
            .paths
            .into_iter()
            .map(|p| (p.suffix, p.is_method, p.takes_args))
            .collect();
        got.sort();
        let expected: Vec<(String, bool, bool)> = expected
            .iter()
            .map(|(s, m, a)| (s.to_string(), *m, *a))
            .collect();
        assert_eq!(got, expected, "{case}");
    }
}

#[test]
fn capped_paths_preserve_distance_spelling_and_terminal_metadata() {
    let index = index_of(
        "struct Goal { value: int }
        struct X { g: Goal }
        struct Root { z: Goal, a: X, b: X }
        fn bridge(root: Root) -> X { root.a }
        fn finish(root: Root, n: int) -> Goal { root.z }
        fn mk(x: X) -> Goal { x.g }
",
    );
    // Expected suffixes in rank order, space-separated.
    for (wanted, expected) in [
        (
            vec!["Goal".to_string()],
            "finish z a.g a.mk b.g b.mk bridge().g bridge().mk",
        ),
        (
            vec!["X".to_string(), "Goal".to_string()],
            "a b bridge finish z a.g a.mk b.g b.mk bridge().g bridge().mk",
        ),
    ] {
        let expected: Vec<&str> = expected.split(' ').collect();
        for cap in 0..=expected.len() + 1 {
            let result = index.paths_to("Root?", &wanted, cap, None);
            assert_eq!(
                result
                    .paths
                    .iter()
                    .map(|p| p.suffix.as_str())
                    .collect::<Vec<_>>(),
                expected[..cap.min(expected.len())],
                "cap {cap}, targets {wanted:?}"
            );
            assert_eq!(result.truncated, cap < expected.len());
            for path in result.paths {
                assert_eq!(
                    path.is_method,
                    path.suffix.ends_with("mk")
                        || path.suffix == "finish"
                        || path.suffix == "bridge"
                );
                assert_eq!(path.takes_args, path.suffix == "finish");
                assert_eq!(
                    path.via_call,
                    path.is_method || path.suffix.starts_with("bridge()")
                );
            }
        }
    }
}

#[test]
fn field_and_method_with_same_name_rank_by_complete_suffix() {
    let index = index_of(
        "struct Goal { value: int }
        struct X { goal: Goal }
        struct Root { a: X }
        fn a(root: Root) -> X { root.a }
",
    );
    for cap in 0..=3 {
        let result = index.paths_to("Root", &["Goal".to_string()], cap, None);
        let expected = ["a().goal", "a.goal"];
        assert_eq!(
            result
                .paths
                .iter()
                .map(|p| p.suffix.as_str())
                .collect::<Vec<_>>(),
            expected[..cap.min(2)]
        );
        assert_eq!(result.truncated, cap < 2);
    }
}
