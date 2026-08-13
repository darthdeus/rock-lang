//! Patch absolute spans/indices after an in-place body edit.
//!
//! Per-body re-lowering keeps the unchanged bodies' HIR but the text under them
//! moved, so every stored byte offset / line number / interval index is stale.
//! `shift_module_after_edit` is the one place that corrects them, so `type_at`,
//! diagnostics, and navigation stay right without re-lowering the kept bodies.

use bone_semantic::{
    hir::{AnnoValue, Callee, Expr, Module, Pattern},
    prelude::*,
};

/// A text edit's offset/line delta, applied to stored spans at/after `at`.
#[derive(Copy, Clone, Debug)]
struct Shift {
    at: usize,
    byte_delta: isize,
    line_delta: isize,
}

impl Shift {
    /// Shift one stored offset iff it sits at/after the edit point.
    fn off(&self, o: usize) -> usize {
        if o >= self.at { (o as isize + self.byte_delta) as usize } else { o }
    }

    /// Move a span by the byte/line delta iff it starts at/after `at`.
    /// `col_range` is intentionally left alone: every kept item after a body
    /// edit begins on a later physical line (items are newline-separated), so
    /// its column is unchanged; only a node sharing the edit's final line would
    /// need it, which doesn't occur for top-level items.
    fn span(&self, s: &mut Span) {
        if s.offset_range.0 >= self.at {
            s.offset_range = (self.off(s.offset_range.0), self.off(s.offset_range.1));
            s.line_range = (
                (s.line_range.0 as isize + self.line_delta) as usize,
                (s.line_range.1 as isize + self.line_delta) as usize,
            );
        }
    }

    /// Shift every span value in an arena map (expr / binding / statement / ty).
    fn map<T>(&self, m: &mut ArenaMap<Idx<T>, Span>) {
        for s in m.values_mut() {
            self.span(s);
        }
    }
}

/// After a body edit re-lowered `edited` in place, every OTHER body keeps its
/// reused HIR but its absolute offsets are stale -- the text under it moved by
/// `byte_delta` bytes / `line_delta` lines starting at byte `at`. Patch every
/// offset carrier in the module so `type_at` / diagnostics / navigation stay
/// correct without re-lowering the unchanged bodies.
pub fn shift_module_after_edit(
    module: &mut Module,
    edited: BodyOwner,
    at: usize,
    byte_delta: isize,
    line_delta: isize,
) {
    let sh = Shift { at, byte_delta, line_delta };

    // Kept bodies: reused HIR with stale absolute spans + node index. The edited
    // body is re-lowered fresh, so skip it.
    for (owner, body) in module.bodies.iter_mut() {
        if *owner == edited {
            continue;
        }
        sh.map(&mut body.binding_spans);
        sh.map(&mut body.statement_spans);
        sh.map(&mut body.expr_spans);
        sh.map(&mut body.lowered_ty_spans);
        for iv in body.node_intervals.iter_mut() {
            iv.start = sh.off(iv.start);
            iv.stop = sh.off(iv.stop);
        }
        body.node_index_cell = Default::default();

        // Spans stored INLINE in HIR nodes, not in the side tables above:
        // callee names, enum-variant heads, struct-literal / record-pattern
        // field names, binding decls. references/rename read these directly.
        for (_, b) in body.binding_arena.iter_mut() {
            sh.span(&mut b.span);
        }
        // No catch-all arm: exhaustiveness is the tripwire. A new Expr/Pattern
        // variant, or a new field on a span-carrying one (fields are named, not
        // `..`), fails to compile until someone decides whether it holds a span
        // that needs shifting. The behavioral net is the reference oracle in
        // bone-query/tests/stale_spans_after_body_edit.rs.
        for (_, e) in body.expr_arena.iter_mut() {
            match e {
                Expr::EnumVariant {
                    enum_name: _,
                    enum_name_span,
                    variant: _,
                    variant_span,
                    args: _,
                } => {
                    sh.span(enum_name_span);
                    sh.span(variant_span);
                }
                Expr::Call { callee, args: _, .. } => match callee {
                    Callee::Unresolved { name: _, name_span } => sh.span(name_span),
                    Callee::Local(_) | Callee::Value(_) => {}
                },
                Expr::Unit
                | Expr::Null
                | Expr::Hole(_)
                | Expr::Int(_)
                | Expr::Float(_)
                | Expr::Bool(_)
                | Expr::String(_)
                | Expr::Var(_)
                | Expr::Tuple(_)
                | Expr::InterpolatedString(_)
                | Expr::Not(_)
                | Expr::BitNot(_)
                | Expr::UnaryMinus(_)
                | Expr::IncDec { .. }
                | Expr::Return(_)
                | Expr::Break
                | Expr::Continue
                | Expr::Yield(_)
                | Expr::YieldDelegate(_)
                | Expr::Spread(_)
                | Expr::As { .. }
                | Expr::Index { .. }
                | Expr::Lambda { .. }
                | Expr::SafeNav { .. }
                | Expr::SafeNavLet { .. }
                | Expr::SafeNavCall { .. }
                | Expr::TupleIndex { .. }
                | Expr::BinOp { .. }
                | Expr::Block { .. }
                | Expr::If { .. }
                | Expr::IfLet { .. }
                | Expr::Array(_)
                | Expr::StructLiteral { .. }
                | Expr::FieldAccess { .. }
                | Expr::Global(_)
                | Expr::SiblingRef(_)
                | Expr::Range { .. }
                | Expr::Assign { .. }
                | Expr::CompoundAssign { .. }
                | Expr::Match { .. } => {}
            }
        }
        for (_, p) in body.pattern_arena.iter_mut() {
            match p {
                Pattern::EnumVariant {
                    enum_name: _,
                    enum_name_span,
                    variant: _,
                    variant_span,
                    sub: _,
                } => {
                    if let Some(s) = enum_name_span {
                        sh.span(s);
                    }
                    sh.span(variant_span);
                }
                Pattern::Record {
                    enum_name: _,
                    enum_name_span,
                    name: _,
                    name_span,
                    fields: _,
                    rest: _,
                } => {
                    if let Some(s) = enum_name_span {
                        sh.span(s);
                    }
                    if let Some(s) = name_span {
                        sh.span(s);
                    }
                }
                Pattern::Wildcard
                | Pattern::Bind(_)
                | Pattern::Literal(_)
                | Pattern::Range { .. }
                | Pattern::Tuple { .. }
                | Pattern::Or { .. } => {}
            }
        }
        for (_, f) in body.struct_lit_field_arena.iter_mut() {
            sh.span(&mut f.name_span);
        }
        for (_, f) in body.record_pat_field_arena.iter_mut() {
            sh.span(&mut f.name_span);
        }
    }

    // Interface type spans (params / returns / fields) of every later signature.
    sh.map(&mut module.ty_spans);

    // Item header / name / annotation / field / variant spans.
    for (_, f) in module.functions.iter_mut() {
        sh.span(&mut f.span);
        sh.span(&mut f.name_span);
        for a in f.annotations.iter_mut() {
            sh.span(&mut a.span);
            sh.span(&mut a.name.span);
            for arg in a.args.iter_mut() {
                if let Some(n) = &mut arg.name {
                    sh.span(&mut n.span);
                }
                if let AnnoValue::Ident(id) = &mut arg.value {
                    sh.span(&mut id.span);
                }
            }
        }
    }
    for (_, st) in module.structs.iter_mut() {
        sh.span(&mut st.span);
        sh.span(&mut st.name_span);
        for fld in st.fields.iter_mut() {
            sh.span(&mut fld.span);
        }
    }
    for (_, en) in module.enums.iter_mut() {
        sh.span(&mut en.span);
        sh.span(&mut en.name_span);
        for v in en.variants.iter_mut() {
            sh.span(&mut v.name_span);
        }
    }
    for (_, nt) in module.newtypes.iter_mut() {
        sh.span(&mut nt.span);
        sh.span(&mut nt.name_span);
    }
    for (_, g) in module.globals.iter_mut() {
        sh.span(&mut g.span);
        sh.span(&mut g.name_span);
    }
    for (_, d) in module.decorators.iter_mut() {
        sh.span(&mut d.span);
    }

    // item_index straddles the edit, so shift its intervals endpoint-wise, then
    // drop the lazily-built index so it rebuilds from the shifted source-of-truth.
    for iv in module.item_intervals.iter_mut() {
        iv.start = sh.off(iv.start);
        iv.stop = sh.off(iv.stop);
    }
    module.item_index_cell = Default::default();
}
