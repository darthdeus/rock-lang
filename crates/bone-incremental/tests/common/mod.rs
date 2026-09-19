//! Comparator pieces shared by the legacy oracle (`incremental.rs`) and the
//! modules-v1 differential harness (`v1_differential.rs`). Both compare an
//! incrementally updated workspace with a fresh one over the same texts, so
//! the text-anchored reading lives here once.

use bone_semantic::prelude::*;

/// `ModuleId` for a file checked through a harness. The id is an opaque dense
/// index assigned by the workspace's `module_ids` interner, so it is recovered
/// by looking the file path up in `ws` (fixtures are parsed with the path as
/// their file id).
pub fn mid_for(ws: &Workspace, path: &str) -> ModuleId {
    ws.module_ids[&Ustr::from(path)]
}

/// Rendered type at every `stride`-th byte offset of `path` (None where
/// `type_at` has no node). Text-anchored so it survives arena-index
/// divergence between an incrementally updated workspace and a fresh batch
/// one. Stride 1 = every offset; the real-project fuzz uses a coarser stride
/// to keep wall time sane (files are tens of KB).
pub fn type_profile(ws: &Workspace, path: &str, text: &str, stride: usize) -> Vec<Option<String>> {
    let mid = mid_for(ws, path);
    let Some(inf) = ws.inference(mid) else {
        return vec![None; text.len().div_ceil(stride)];
    };
    (0..text.len())
        .step_by(stride)
        .map(|offset| {
            bone_semantic::query::type_at(ws, mid, inf, offset).map(|ta| ws.render(ta.body, ta.ty))
        })
        .collect()
}
