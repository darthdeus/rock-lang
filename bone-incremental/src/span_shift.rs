//! Patch absolute spans/indices after an in-place body edit.
//!
//! Per-body re-lowering keeps the unchanged bodies' HIR but the text under them
//! moved, so every stored byte offset / line number / interval index is stale.
//! `shift_module_after_edit` is the one place that corrects them, so `type_at`,
//! diagnostics, and navigation stay right without re-lowering the kept bodies.

use bone_semantic::{hir::Module, prelude::*};

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
    }

    // Interface type spans (params / returns / fields) of every later signature.
    sh.map(&mut module.ty_spans);

    // Item header / name / annotation / field / variant spans.
    for (_, f) in module.functions.iter_mut() {
        sh.span(&mut f.span);
        sh.span(&mut f.name_span);
        for a in f.annotations.iter_mut() {
            sh.span(&mut a.span);
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
