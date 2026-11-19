use bone_core::hir::{Body, Expr};
use bone_core::hir_walk::HirChild;
use bone_core::prelude::{FxHashSet, Idx};

/// Every expression in `root`'s subtree, `root` included.
pub fn expr_subtree(body: &Body, root: Idx<Expr>) -> FxHashSet<Idx<Expr>> {
    let mut out = FxHashSet::default();
    walk_expr(body, root, &mut out);
    out
}

fn walk_expr(body: &Body, e: Idx<Expr>, out: &mut FxHashSet<Idx<Expr>>) {
    if !out.insert(e) {
        return;
    }
    body.walk_descendants(HirChild::Expr(e), &mut |child| match child {
        HirChild::Expr(x) => out.insert(x),
        HirChild::Statement(_) | HirChild::Pattern(_) => true,
    });
}
