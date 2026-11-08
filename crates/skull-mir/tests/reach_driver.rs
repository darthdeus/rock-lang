//! Direct tests of the shared reachability driver.
//!
//! The backend corpora cannot pin this: the existing shaders have no function with
//! two distinct emittable callees, so flipping child-visit order leaves every
//! golden AND `shader-validate` green. Post-order fidelity is load-bearing for
//! GLSL (it emits straight from the order and has no forward declarations), so
//! it gets tested here against a call graph shaped to expose it.

use bone_core::prelude::CompilerError;
use skull_mir::reach::{ReachPolicy, walk};

/// A call graph over integer nodes, with the traversal recorded.
struct Graph {
    edges: Vec<(u32, Vec<u32>)>,
    roots: Vec<u32>,
    leaves: Vec<u32>,
    in_progress: Vec<u32>,
    reject_cycles: bool,
    visited: Vec<u32>,
}

impl Graph {
    fn new(edges: &[(u32, &[u32])], roots: &[u32]) -> Self {
        Graph {
            edges: edges.iter().map(|(n, s)| (*n, s.to_vec())).collect(),
            roots: roots.to_vec(),
            leaves: Vec::new(),
            in_progress: Vec::new(),
            reject_cycles: false,
            visited: Vec::new(),
        }
    }
}

impl ReachPolicy for Graph {
    type Node = u32;

    fn roots(&mut self) -> Result<Vec<u32>, CompilerError> {
        Ok(self.roots.clone())
    }

    fn initially_in_progress(&mut self) -> Vec<u32> {
        self.in_progress.clone()
    }

    fn is_leaf(&mut self, node: u32) -> Result<bool, CompilerError> {
        Ok(self.leaves.contains(&node))
    }

    fn on_cycle(&mut self, node: u32) -> Result<(), CompilerError> {
        if self.reject_cycles {
            return Err(CompilerError::new_no_location(format!("cycle at {node}")));
        }
        Ok(())
    }

    fn visit(&mut self, node: u32) -> Result<Vec<u32>, CompilerError> {
        self.visited.push(node);
        Ok(self
            .edges
            .iter()
            .find(|(n, _)| *n == node)
            .map(|(_, s)| s.clone())
            .unwrap_or_default())
    }
}

/// A diamond: children must be expanded left-to-right and each node must land
/// after all of its successors. Goes RED if children are pushed without being
/// reversed (yields `[D, C, B, A]`).
#[test]
fn post_order_matches_recursive_dfs() {
    let mut g = Graph::new(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])], &[1]);
    let order = walk(&mut g).unwrap();
    assert_eq!(order, vec![4, 2, 3, 1]);
}

/// Every node is expanded at most once even when several callers reach it.
#[test]
fn shared_successor_expands_once() {
    let mut g = Graph::new(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])], &[1]);
    walk(&mut g).unwrap();
    assert_eq!(g.visited.iter().filter(|&&n| n == 4).count(), 1);
}

/// Multiple roots share one traversal: a node reached from the first root is
/// not re-expanded under the second, and each root still lands after its own
/// successors.
#[test]
fn roots_share_one_traversal() {
    let mut g = Graph::new(&[(1, &[3]), (2, &[3]), (3, &[])], &[1, 2]);
    let order = walk(&mut g).unwrap();
    assert_eq!(order, vec![3, 1, 2]);
    assert_eq!(g.visited.iter().filter(|&&n| n == 3).count(), 1);
}

/// A leaf is marked but never expanded and never enters the post-order.
#[test]
fn leaf_is_marked_but_never_expanded_or_ordered() {
    let mut g = Graph::new(&[(1, &[2]), (2, &[])], &[1]);
    g.leaves = vec![2];
    let order = walk(&mut g).unwrap();
    assert_eq!(order, vec![1]);
    assert!(!g.visited.contains(&2));
}

/// The leaf test runs BEFORE the cycle test, so a policy that rejects an
/// unemittable node from `is_leaf` reports that rather than a recursion error.
#[test]
fn leaf_test_precedes_cycle_test() {
    let mut g = Graph::new(&[(1, &[2]), (2, &[2])], &[1]);
    g.leaves = vec![2];
    g.reject_cycles = true;
    assert!(walk(&mut g).is_ok());
}

/// Recursion is a cycle the policy may allow (JS: ordinary Rock) ...
#[test]
fn cycle_allowed_terminates_without_reexpanding() {
    let mut g = Graph::new(&[(1, &[2]), (2, &[1])], &[1]);
    let order = walk(&mut g).unwrap();
    assert_eq!(order, vec![2, 1]);
    assert_eq!(g.visited, vec![1, 2]);
}

/// ... or reject (GLSL: no call stack for it).
#[test]
fn cycle_rejected_surfaces_policy_error() {
    let mut g = Graph::new(&[(1, &[2]), (2, &[1])], &[1]);
    g.reject_cycles = true;
    let e = walk(&mut g).unwrap_err();
    assert!(e.message.contains("cycle at 1"), "{}", e.message);
}

/// GLSL's shape: the shader entry is expanded by its caller, which seeds the
/// driver with the entry's callees. A callee calling back into the entry is
/// recursion, and only `initially_in_progress` lets the driver see it.
#[test]
fn initially_in_progress_node_is_a_cycle() {
    let mut g = Graph::new(&[(2, &[1])], &[2]);
    g.in_progress = vec![1];
    g.reject_cycles = true;
    let e = walk(&mut g).unwrap_err();
    assert!(e.message.contains("cycle at 1"), "{}", e.message);
}

/// Unreached nodes are not expanded -- the point of the whole search.
#[test]
fn unreachable_node_is_never_expanded() {
    let mut g = Graph::new(&[(1, &[2]), (2, &[]), (99, &[])], &[1]);
    let order = walk(&mut g).unwrap();
    assert_eq!(order, vec![2, 1]);
    assert!(!g.visited.contains(&99));
}
