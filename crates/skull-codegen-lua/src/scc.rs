use super::*;

/// Each function's strongly connected component number over `edges`.
/// Components are numbered callee-first, so the binding pass emits in
/// ascending rank and every captured callee is already its final closure.
pub fn call_sccs(edges: impl IntoIterator<Item = (FuncRef, FuncRef)>) -> HashMap<FuncRef, u32> {
    fn intern(
        f: FuncRef,
        ids: &mut HashMap<FuncRef, usize>,
        nodes: &mut Vec<FuncRef>,
        succ: &mut Vec<Vec<u32>>,
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
    let mut succ: Vec<Vec<u32>> = Vec::new();
    for (from, to) in edges {
        let from = intern(from, &mut ids, &mut nodes, &mut succ);
        let to = intern(to, &mut ids, &mut nodes, &mut succ);
        succ[from].push(to as u32);
    }

    let mut component = vec![u32::MAX; nodes.len()];
    for (rank, members) in skull_mir::reach::strongly_connected_components(&succ)
        .into_iter()
        .enumerate()
    {
        for member in members {
            component[member as usize] = rank as u32;
        }
    }
    nodes.into_iter().zip(component).collect()
}
