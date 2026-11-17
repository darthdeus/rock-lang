use skull_mir::mir::{Inst, MirFunc};
use skull_transform::scope::ScopeTree;

use crate::support::lower;

fn rewrite_last_store_to_dest_write(
    func: &mut MirFunc,
    cell: comfy_la_arena::Idx<skull_mir::mir::Cell>,
) {
    let last_store = func
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::Store { cell: c, .. } if *c == cell))
        .map(|(id, _)| id)
        .next_back()
        .expect("a Store of the cell");
    for (_, b) in func.blocks.iter_mut() {
        b.insts.retain(|&i| i != last_store);
    }
    let call = func
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::CallMulti { .. }))
        .map(|(id, _)| id)
        .next_back()
        .expect("a CallMulti");
    let Inst::CallMulti { dests, .. } = &mut func.insts[call] else {
        unreachable!()
    };
    dests[0] = cell;
}

/// A captured cell whose reassignment is a `CallMulti` DEST write (no second
/// `Inst::Store`): write counting must see dests or the cell wrongly
/// classifies single-assignment by-value.
#[test]
fn dest_written_captures_box_from_either_scope() {
    for (name, source) in [
        (
            "after capture",
            "value V { a: int, b: int }\n\
             fn g() -> V { return V { a: 1, b: 2 } }\n\
             fn f() -> fn() -> int { let v = g() let h = () => v.a v = g() return h }",
        ),
        (
            "inside lambda",
            "value V { a: int, b: int }\n\
             fn g() -> V { return V { a: 1, b: 2 } }\n\
             fn f() -> fn() -> int { let v = g() let h = () => { v = g() return v.a } return h }",
        ),
    ] {
        let mut func = lower(source, "f");
        let tree = ScopeTree::build(&func);
        let own = tree.analyze(&func);
        let lam = tree
            .scopes
            .iter()
            .position(|scope| scope.lambda.is_some())
            .unwrap();
        assert_eq!(own.captures[lam].len(), 1, "{name}: one scalar capture");
        let va = *own.captures[lam]
            .iter()
            .next()
            .expect("captured scalar cell");
        rewrite_last_store_to_dest_write(&mut func, va);

        let tree = ScopeTree::build(&func);
        let own = tree.analyze(&func);
        let boxed = tree.boxed_cells(&own);
        assert!(own.captures[lam].contains(&va), "{name}: capture remains");
        assert!(
            boxed.contains(&va),
            "{name}: dest write must box; {boxed:?}"
        );
    }
}
