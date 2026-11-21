//! Graph-search invariants of the type-reachability index behind type-directed completion.

use bone_query::reachability::{ReachResult, ReachabilityIndex};
use bone_semantic::prelude::*;
use bone_semantic::semantic_passes::semantic_pass;

fn index_of(src: &str) -> ReachabilityIndex {
    let module = skull_parser::parse_module_clean(src, Ustr::from("<reach-test>")).expect("parse");
    let ws = semantic_pass(&mut [module]).expect("semantic_pass");
    ReachabilityIndex::build(&ws)
}

fn fixture_paths(
    index: &ReachabilityIndex,
    root: &str,
    expected: &[String],
    cap: usize,
) -> ReachResult {
    const PREFIX: &str = "@/<reach-test>::";
    let root = format!("{PREFIX}{root}");
    let expected: Vec<_> = expected
        .iter()
        .map(|name| format!("{PREFIX}{name}"))
        .collect();
    let mut result = index.paths_to(&root, &expected, cap, None);
    for path in &mut result.paths {
        path.suffix = path.suffix.replace(PREFIX, "");
    }
    result
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
            let result = fixture_paths(&index, "Root?", &wanted, cap);
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
        let result = fixture_paths(&index, "Root", &["Goal".to_string()], cap);
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
