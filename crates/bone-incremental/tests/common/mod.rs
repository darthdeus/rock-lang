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

/// The package every single-package modules-v1 fixture belongs to.
pub const PACKAGE: &str = "livev1";

pub fn package_tag(id: u32, name: &str) -> skull_parser::PackageTag {
    skull_parser::PackageTag {
        id,
        self_name: name.into(),
        identity_storage: Default::default(),
        root: String::new(),
    }
}

pub fn in_package(
    dialect: skull_parser::Dialect,
    package: u32,
    name: &str,
    path: &str,
    text: &str,
) -> bone_incremental::session::SessionFile {
    bone_incremental::session::SessionFile {
        path: path.into(),
        text: text.into(),
        dialect,
        package: Some(package_tag(package, name)),
        global_policy: skull_parser::GlobalPolicy::Any,
    }
}

pub fn v1(path: &str, text: &str) -> bone_incremental::session::SessionFile {
    in_package(skull_parser::Dialect::V1, 1, PACKAGE, path, text)
}
