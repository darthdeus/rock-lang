//! P2 step 2 of `docs/closure-env-records-plan.md`: the scope ownership analysis
//! (`skull_transform::scope`). Each snippet lowers ONE fn to raw MIR (no
//! cleanup, so `var`/`let` stores survive) and asserts the recovered scope tree,
//! cell owners, and per-lambda captures. Covers the two shapes that break a naive
//! "a Store in a lambda block is a lambda-local" reading: a nested lambda reading
//! an enclosing-fn cell (captured by BOTH lambdas) and a cell mutated from inside
//! a lambda (owned above, captured below).

use skull_transform::scope::{ScopeTree, ROOT};
use skull_mir::mir::{Inst, MirFunc};

/// Lower `fn_name` from `src` to raw MIR (no stdlib -- avoid arithmetic, which
/// routes through a stdlib op fn). Captures are about cell access shape, not
/// operations, so `() => x` / `n = 5` suffice.
fn lower(src: &str, fn_name: &str) -> MirFunc {
    let ast = skull_parser::parse_module_clean(src, ustr::Ustr::from("<test>")).expect("parse");
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut vec![ast]).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == fn_name)
        .expect("function by name");
    let scx = skull_mir::lower::ScalarCtx::new(&ws);
    skull_mir::lower::lower_function(&ws, &scx, mid, fid)
        .unwrap_or_else(|e| panic!("lower failed: {}", e.message))
}

/// Number of scopes created by lambdas (total scopes minus the root).
fn lambda_scope_ids(tree: &ScopeTree) -> Vec<usize> {
    (0..tree.scopes.len()).filter(|&s| tree.scopes[s].lambda.is_some()).collect()
}

/// A capturing lambda over a fn param: `x` is owned by root, captured by the one
/// lambda scope; the lambda's parent is root.
#[test]
fn simple_capture_of_fn_param() {
    let func = lower("fn f(x: int) -> fn() -> int { return () => x }", "f");
    let tree = ScopeTree::build(&func);
    let lambdas = lambda_scope_ids(&tree);
    assert_eq!(lambdas.len(), 1, "one lambda -> one lambda scope");
    let lam = lambdas[0];
    assert_eq!(tree.scopes[lam].parent, Some(ROOT), "the lambda nests directly under root");

    let x = func.params[0];
    let own = tree.analyze(&func);
    assert_eq!(own.owner[&x], ROOT, "a fn param is owned by root");
    assert!(own.captures[lam].contains(&x), "the lambda must capture `x`");
    assert!(own.captures[ROOT].is_empty(), "root captures nothing");
}

/// A lambda that only reads/writes its OWN local captures nothing.
#[test]
fn lambda_local_is_not_captured() {
    let func = lower("fn f() -> fn() -> int { return () => { let y = 5 return y } }", "f");
    let tree = ScopeTree::build(&func);
    let lambdas = lambda_scope_ids(&tree);
    assert_eq!(lambdas.len(), 1);
    let lam = lambdas[0];
    let own = tree.analyze(&func);
    assert!(
        own.captures[lam].is_empty(),
        "a lambda touching only its own local captures nothing; got {:?}",
        own.captures[lam]
    );
}

/// Nested lambda reading an enclosing-FN cell: `x` (a root param) is read only by
/// the INNER lambda. It is owned by root and captured by BOTH lambdas -- the
/// outer threads it to the inner record it constructs.
#[test]
fn nested_lambda_capturing_root_cell_captured_by_both() {
    let func = lower(
        "fn f(x: int) -> fn() -> fn() -> int { return () => { return () => x } }",
        "f",
    );
    let tree = ScopeTree::build(&func);
    let lambdas = lambda_scope_ids(&tree);
    assert_eq!(lambdas.len(), 2, "two nested lambdas -> two lambda scopes");

    // Identify inner vs outer by parent: outer's parent is root, inner's is outer.
    let outer = *lambdas.iter().find(|&&s| tree.scopes[s].parent == Some(ROOT)).expect("an outer");
    let inner = *lambdas.iter().find(|&&s| tree.scopes[s].parent == Some(outer)).expect("an inner under outer");

    let x = func.params[0];
    let own = tree.analyze(&func);
    assert_eq!(own.owner[&x], ROOT, "the enclosing-fn param is owned by root");
    assert!(own.captures[inner].contains(&x), "the inner lambda captures `x`");
    assert!(own.captures[outer].contains(&x), "the outer lambda also captures `x` (threads it inward)");
}

/// A local mutated from inside a lambda (`let` is Rock's reassignable binding --
/// there is no `var` keyword): owned by root (root's `let n = 0` store + the
/// lambda's `n = 5` store share it), captured by the lambda. This is the shape
/// step 5 boxes; step 2 only pins the ownership.
#[test]
fn cell_mutated_from_lambda_owned_above_captured_below() {
    let func = lower("fn f() -> fn() { let n = 0 return () => { n = 5 } }", "f");
    let tree = ScopeTree::build(&func);
    let lambdas = lambda_scope_ids(&tree);
    assert_eq!(lambdas.len(), 1);
    let lam = lambdas[0];
    let own = tree.analyze(&func);

    assert_eq!(own.captures[lam].len(), 1, "the lambda captures exactly the mutated `n`");
    let n = *own.captures[lam].iter().next().unwrap();
    assert_eq!(own.owner[&n], ROOT, "a cell stored in both root and the lambda is owned by root");

    // Sanity: `n` really is stored in the lambda's region (a non-owner store --
    // the box trigger for step 5), not merely read.
    let stored_in_lambda = tree.scopes[lam].region.iter().any(|&b| {
        func.blocks[b].insts.iter().any(|&i| matches!(&func.insts[i], Inst::Store { cell, .. } if *cell == n))
    });
    assert!(stored_in_lambda, "`n` must be Stored inside the lambda (mutation from a non-owner scope)");
}

// --- box classification (step 5) --------------------------------------------

/// A captured cell mutated from inside a lambda BOXES (it must observe the write
/// by reference).
#[test]
fn mutated_capture_boxes() {
    let func = lower("fn f() -> fn() { let n = 0 return () => { n = 5 } }", "f");
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let boxed = tree.boxed_cells(&own);
    let lam = lambda_scope_ids(&tree)[0];
    let n = *own.captures[lam].iter().next().unwrap();
    assert!(boxed.contains(&n), "a cell mutated from the lambda must box; boxed={boxed:?}");
}

/// A captured cell that the ENCLOSING code mutates after capture (multi-store)
/// BOXES so the closure observes later writes.
#[test]
fn multistore_capture_boxes() {
    // n stored twice in root (let n = 0; n = 5), read by the lambda.
    let func = lower("fn f() -> fn() -> int { let n = 0 let g = () => { return n } n = 5 return g }", "f");
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let boxed = tree.boxed_cells(&own);
    let lam = lambda_scope_ids(&tree)[0];
    let n = *own.captures[lam].iter().next().unwrap();
    assert!(boxed.contains(&n), "a multi-store captured cell must box; boxed={boxed:?}");
}

/// Rewrite `func` so the LAST `Inst::Store` of `cell` becomes a `CallMulti`
/// dest write instead: drop that Store from its block and re-point dests[0] of
/// the arena-latest `CallMulti` at `cell`. Raw lowering always materializes
/// call results into fresh temps + a copy Store, so a dest-written USER cell
/// only exists post-transform (the inliner rebinds dests) -- this builds that
/// shape directly.
fn rewrite_last_store_to_dest_write(func: &mut MirFunc, cell: comfy_la_arena::Idx<skull_mir::mir::Cell>) {
    let last_store = func
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::Store { cell: c, .. } if *c == cell))
        .map(|(id, _)| id)
        .last()
        .expect("a Store of the cell");
    for (_, b) in func.blocks.iter_mut() {
        b.insts.retain(|&i| i != last_store);
    }
    let call = func
        .insts
        .iter()
        .filter(|(_, i)| matches!(i, Inst::CallMulti { .. }))
        .map(|(id, _)| id)
        .last()
        .expect("a CallMulti");
    let Inst::CallMulti { dests, .. } = &mut func.insts[call] else { unreachable!() };
    dests[0] = cell;
}

/// A captured cell whose reassignment is a `CallMulti` DEST write (no second
/// `Inst::Store`): write counting must see dests or the cell wrongly
/// classifies single-assignment by-value.
#[test]
fn dest_written_capture_boxes() {
    let mut func = lower(
        "value V { a: int, b: int }\n\
         fn g() -> V { return V { a: 1, b: 2 } }\n\
         fn f() -> fn() -> int { let v = g() let h = () => v.a v = g() return h }",
        "f",
    );
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let lam = lambda_scope_ids(&tree)[0];
    assert_eq!(own.captures[lam].len(), 1, "the lambda captures `v.a`'s scalar cell");
    let va = *own.captures[lam].iter().next().unwrap();

    rewrite_last_store_to_dest_write(&mut func, va);
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let boxed = tree.boxed_cells(&own);
    assert!(own.captures[lam].contains(&va), "still captured (the lambda Load is untouched)");
    assert!(boxed.contains(&va), "a dest-reassigned captured cell must box; boxed={boxed:?}");
}

/// A captured cell dest-written from INSIDE the lambda: the writing scope must
/// be visible through dests (a non-owner write is the box trigger even at one
/// effective store).
#[test]
fn dest_written_inside_lambda_boxes() {
    let mut func = lower(
        "value V { a: int, b: int }\n\
         fn g() -> V { return V { a: 1, b: 2 } }\n\
         fn f() -> fn() -> int { let v = g() let h = () => { v = g() return v.a } return h }",
        "f",
    );
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let lam = lambda_scope_ids(&tree)[0];
    let va = *own.captures[lam].iter().next().expect("captured scalar cell");

    // The lambda's `v = g()` is the LAST Store of va in program order (arena
    // order tracks lowering order); rewriting it moves the write into the
    // lambda's CallMulti dests.
    rewrite_last_store_to_dest_write(&mut func, va);
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let boxed = tree.boxed_cells(&own);
    assert!(own.captures[lam].contains(&va), "still captured (the lambda Load is untouched)");
    assert!(boxed.contains(&va), "a cell dest-written from a non-owner scope must box; boxed={boxed:?}");
}

/// A single-assignment captured cell (bound once, never mutated) stays BY-VALUE
/// -- it must NOT box, else a per-iteration capture would collapse to the final
/// value.
#[test]
fn single_assignment_capture_does_not_box() {
    let func = lower("fn f(x: int) -> fn() -> int { return () => x }", "f");
    let tree = ScopeTree::build(&func);
    let own = tree.analyze(&func);
    let boxed = tree.boxed_cells(&own);
    let x = func.params[0]; // a 0-store param -> effective 1 store -> by value
    assert!(!boxed.contains(&x), "a single-assignment (0-store) param capture must not box");
}
