//! **Which expressions belong to one HIR expression's subtree.**
//!
//! A struct's `BodyOwner::StructDefaults` body holds every field's default
//! initializer in one arena, and a struct literal lowers only the defaults of
//! the fields it OMITS. Attributing that body's calls per field therefore needs
//! the subtree partition.
//!
//! The child enumeration is bone's `Body::for_each_child`, which is total over
//! the HIR by construction. [`defaults_body_residue`] is the runtime half: the
//! union of a defaults body's field subtrees must be its whole arena, and the
//! census reports any residue.

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

/// Expressions of a struct-defaults `body` that NO field's default subtree
/// reaches.
///
/// A defaults body holds nothing but field initializers, so this is 0 by
/// construction -- and it is measured rather than assumed, because a missed
/// child link would show up here and nowhere else: the walk would simply
/// attribute fewer calls and every count would still balance.
pub fn defaults_body_residue(body: &Body) -> usize {
    let mut covered = FxHashSet::default();
    for root in body.field_defaults.values() {
        walk_expr(body, *root, &mut covered);
    }
    body.expr_arena
        .iter()
        .filter(|(e, _)| !covered.contains(e))
        .count()
}
