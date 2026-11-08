//! Strongly connected components of the call graph.

use super::*;

/// The strongly connected components of a `(caller, callee)` call-edge stream,
/// the Tarjan run behind [`call_sccs`].
///
/// Components are numbered in Tarjan COMPLETION order, which is a reverse
/// topological order of the condensation: a component is closed only after
/// every component it can reach, so `component[callee] < component[caller]`
/// whenever the two differ. [`call_sccs`] hands that number out as the
/// callee-binding rank, and the binding pass emits in ascending rank so every
/// captured callee is already its final closure.
struct CallComponents {
    nodes: Vec<FuncRef>,
    /// Node index -> component number.
    component: Vec<u32>,
}

fn tarjan_components(edges: impl IntoIterator<Item = (FuncRef, FuncRef)>) -> CallComponents {
    fn intern(
        f: FuncRef,
        ids: &mut HashMap<FuncRef, usize>,
        nodes: &mut Vec<FuncRef>,
        succ: &mut Vec<Vec<usize>>,
    ) -> usize {
        if let Some(&i) = ids.get(&f) {
            return i;
        }
        let i = nodes.len();
        ids.insert(f, i);
        nodes.push(f);
        succ.push(Vec::new());
        i
    }

    let mut ids: HashMap<FuncRef, usize> = HashMap::default();
    let mut nodes: Vec<FuncRef> = Vec::new();
    let mut succ: Vec<Vec<usize>> = Vec::new();
    for (from, to) in edges {
        let from = intern(from, &mut ids, &mut nodes, &mut succ);
        let to = intern(to, &mut ids, &mut nodes, &mut succ);
        succ[from].push(to);
    }

    // Iterative Tarjan: a program-sized call graph has chains deep enough to
    // overflow a recursive DFS.
    const UNVISITED: usize = usize::MAX;
    let n = nodes.len();
    let mut index = vec![UNVISITED; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut component = vec![u32::MAX; n];
    let mut next_component = 0u32;
    let mut members: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut work: Vec<(usize, usize)> = Vec::new();
    let mut next_index = 0usize;
    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        work.push((root, 0));
        while let Some((v, resume)) = work.pop() {
            if resume == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            let mut descended = false;
            let mut i = resume;
            while i < succ[v].len() {
                let w = succ[v][i];
                i += 1;
                if index[w] == UNVISITED {
                    work.push((v, i));
                    work.push((w, 0));
                    descended = true;
                    break;
                }
                if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            }
            if descended {
                continue;
            }
            if low[v] == index[v] {
                members.clear();
                loop {
                    let w = stack.pop().expect("tarjan component stack");
                    on_stack[w] = false;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                for &w in &members {
                    component[w] = next_component;
                }
                next_component += 1;
            }
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
        }
    }
    CallComponents { nodes, component }
}

/// Each function's strongly connected component number over `edges`, the input
/// callee binding reads through [`CodegenCtx::set_call_sccs`]. Two functions share a number exactly when
/// they are mutually reachable, and a callee's number is strictly lower than
/// its caller's whenever they differ -- see [`CallComponents`].
pub fn call_sccs(edges: impl IntoIterator<Item = (FuncRef, FuncRef)>) -> HashMap<FuncRef, u32> {
    let c = tarjan_components(edges);
    c.nodes
        .iter()
        .copied()
        .zip(c.component.iter().copied())
        .collect()
}
