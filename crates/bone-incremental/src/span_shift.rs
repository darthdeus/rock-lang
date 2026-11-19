//! Patch absolute spans and indices after an in-place body edit.

use bone_core::text_edit::TextEditDelta;
use bone_semantic::{
    hir::{AnnoValue, Module, Pattern},
    prelude::*,
};

struct Shift(TextEditDelta);

impl Shift {
    fn off(&self, o: usize) -> usize {
        self.0.shift_offset(o)
    }

    fn span(&self, s: &mut Span) {
        self.0.shift_span(s);
    }

    fn map<T>(&self, m: &mut ArenaMap<Idx<T>, Span>) {
        for s in m.values_mut() {
            self.span(s);
        }
    }
}

pub fn shift_module_after_edit(
    module: &mut Module,
    edited: BodyOwner,
    at: usize,
    byte_delta: isize,
    line_delta: isize,
) {
    let sh = Shift(TextEditDelta {
        file: module.file,
        at,
        byte_delta,
        line_delta,
    });

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

        // Spans stored inline in HIR nodes, not in the side tables above.
        for (_, b) in body.binding_arena.iter_mut() {
            sh.span(&mut b.span);
        }
        // `Expr` holds no spans: the names written inside expressions keep theirs in
        // `name_spans`.
        for s in body.name_spans.values_mut() {
            sh.span(s);
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
                Pattern::Newtype {
                    name: _,
                    name_span,
                    sub: _,
                } => sh.span(name_span),
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
        for seg in body.path_segments.iter_mut() {
            sh.span(&mut seg.span);
        }
    }

    for seg in module.path_segments.iter_mut() {
        sh.span(&mut seg.span);
    }

    sh.map(&mut module.ty_spans);

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
                match &mut arg.value {
                    AnnoValue::Ident(id) => sh.span(&mut id.span),
                    AnnoValue::IdentList(ids) => {
                        for id in ids {
                            sh.span(&mut id.span);
                        }
                    }
                    _ => {}
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
