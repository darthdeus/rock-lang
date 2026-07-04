use rust_lapper::Interval;

use crate::{
    hir::{NodeRef, TyId},
    prelude::*,
};

#[derive(Clone, Debug)]
pub struct TypeAt<'a> {
    pub ty: TyId,
    pub span: Span,
    pub body: Option<&'a BodyInference>,
}

pub fn type_at<'a>(
    ws: &Workspace,
    mid: ModuleId,
    inf: &'a InferenceResult,
    offset: usize,
) -> Option<TypeAt<'a>> {
    let module = ws.modules.get(mid.index())?;

    let mut best: Option<(BodyOwner, &Body, &Interval<usize, NodeRef>)> = None;

    for item in module.item_index().find(offset, offset + 1) {
        let owner = item.val;

        let Some(body) = module.bodies.get(&owner) else {
            continue;
        };

        if let Some(iv) = body
            .node_index()
            .find(offset, offset + 1)
            .min_by_key(|x| x.stop - x.start)
        {
            if best
                .as_ref()
                .map_or(true, |(_, _, b)| iv.stop - iv.start < b.stop - b.start)
            {
                best = Some((owner, body, iv));
            }
        }
    }

    let (_, _, interval) = best?;

    let interval_span = Span {
        file: module.file,
        line_range: (0, 0),
        col_range: (0, 0),
        offset_range: (interval.start, interval.stop),
    };

    match interval.val {
        hir::NodeRef::StaticTy { mid, idx } => Some(TypeAt {
            ty: TyId::Static(mid, idx),
            span: interval_span,
            body: None,
        }),

        hir::NodeRef::Expr(idx) => {
            let owner = module.body_at(offset)?;
            let bi = inf.per_fn.get(&owner)?;
            let body = module.bodies.get(&owner)?;

            debug_assert!(
                bi.body_generation == body.generation,
                "stale inference for {owner:?}: body relowered (gen {}) since inference (gen {})",
                body.generation,
                bi.body_generation
            );

            let ty = *bi.expr_types.get(idx)?;
            let span = *body.expr_spans.get(idx)?;

            Some(TypeAt {
                ty,
                span,
                body: Some(bi),
            })
        }

        hir::NodeRef::Statement(_) => todo!(),
        hir::NodeRef::Binding(idx) => {
            let owner = module.body_at(offset)?;
            let bi = inf.per_fn.get(&owner)?;
            let body = module.bodies.get(&owner)?;

            debug_assert!(
                bi.body_generation == body.generation,
                "stale inference for {owner:?}: body relowered (gen {}) since inference (gen {})",
                body.generation,
                bi.body_generation
            );

            let ty = *bi.binding_types.get(idx)?;
            let span = *body.binding_spans.get(idx)?;

            Some(TypeAt {
                ty,
                span,
                body: Some(bi),
            })
        }

        hir::NodeRef::LocalTy(idx) => {
            let owner = module.body_at(offset)?;

            Some(TypeAt {
                ty: TyId::Local(idx),
                span: interval_span,
                body: inf.per_fn.get(&owner),
            })
        }
    }
}
