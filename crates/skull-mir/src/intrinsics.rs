use bone_core::hir::Function;
use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_parser::CompilerError;
use std::collections::HashSet;
use ustr::Ustr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emission {
    /// `$1..$n` are argument placeholders (`sin($1)`, `Math.atan2($1, $2)`).
    Template(&'static str),
    Global(&'static str),
    Body,
    Expand(ExpandKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpandKind {
    SceneTypeKey,
    Materialize,
    StructPairs,
    ShaderPipeline,
    VertexLayout,
    UniformBlock,
    BuildVar,
    EmbedString,
    UniformValues,
    SortKeyWide,
    SortKeyLt,
}

pub fn is_intrinsic(f: &Function) -> bool {
    has_anno(f, "intrinsic") || expand_kind(f).is_some()
}

pub fn expand_kind(f: &Function) -> Option<ExpandKind> {
    if has_anno(f, "scene_type_key") {
        return Some(ExpandKind::SceneTypeKey);
    }
    if has_anno(f, "materialize") {
        return Some(ExpandKind::Materialize);
    }
    if has_anno(f, "struct_expand") {
        return Some(ExpandKind::StructPairs);
    }
    if has_anno(f, "shader_expand") {
        return Some(ExpandKind::ShaderPipeline);
    }
    if has_anno(f, "vertex_expand") {
        return Some(ExpandKind::VertexLayout);
    }
    if has_anno(f, "uniform_expand") {
        return Some(ExpandKind::UniformBlock);
    }
    if has_anno(f, "build_var_expand") {
        return Some(ExpandKind::BuildVar);
    }
    if has_anno(f, "embed_expand") {
        return Some(ExpandKind::EmbedString);
    }
    if has_anno(f, "uniform_values") {
        return Some(ExpandKind::UniformValues);
    }
    if has_anno(f, "sort_key_wide") {
        return Some(ExpandKind::SortKeyWide);
    }
    if has_anno(f, "sort_key_lt") {
        return Some(ExpandKind::SortKeyLt);
    }
    None
}

pub fn is_native_required(f: &Function) -> bool {
    has_anno(f, "native_required")
}

pub(crate) fn is_gpu_only(f: &Function) -> bool {
    has_anno(f, "gpu_only")
}

pub(crate) fn is_gpu_context(f: &Function) -> bool {
    is_gpu_only(f) || has_anno(f, "shader")
}

fn has_luajit_mapping(f: &Function) -> bool {
    f.inline_body.is_some()
}

fn has_anno(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.is_builtin(name))
}

pub trait IntrinsicTable {
    fn backend(&self) -> &'static str;
    fn lookup(&self, f: &Function) -> Option<Emission>;
    fn entries(&self) -> Vec<IntrinsicEntry>;
}

pub trait BackendRealization {
    fn backend(&self) -> &'static str;
    fn supplies(&self, ws: &Workspace, f: &Function) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicEntry {
    pub name: &'static str,
    pub arity: usize,
}

pub fn declared_intrinsics(ws: &Workspace) -> HashSet<(Ustr, usize)> {
    let mut out = HashSet::new();
    for module in ws.modules.iter() {
        for (_, f) in module.functions.iter() {
            if is_intrinsic(f) {
                out.insert((f.name, f.param_decls.len()));
                out.insert((
                    Ustr::from(crate::mangle::declared_name(f.name.as_str())),
                    f.param_decls.len(),
                ));
                out.insert((
                    Ustr::from(&crate::mangle::mangle_fn(ws, f)),
                    f.param_decls.len(),
                ));
            }
        }
    }
    out
}

pub fn unbacked_entries(
    table: &dyn IntrinsicTable,
    declared: &HashSet<(Ustr, usize)>,
) -> Vec<IntrinsicEntry> {
    table
        .entries()
        .into_iter()
        .filter(|e| !declared.contains(&(Ustr::from(e.name), e.arity)))
        .collect()
}

pub fn intrinsic_rejection(ws: &Workspace, mid: ModuleId) -> Option<CompilerError> {
    let module = ws.module(mid);
    for (_, f) in module.functions.iter() {
        if is_gpu_only(f) && !has_anno(f, "intrinsic") {
            return Some(CompilerError::new_with_label(
                f.name_span,
                format!(
                    "`@gpu_only` marks an `@intrinsic` fn that only GPU backends map; \
                     `{}` is `@gpu_only` but not `@intrinsic`",
                    f.name
                ),
            ));
        }
        if !is_native_required(f) {
            continue;
        }
        if !has_anno(f, "intrinsic") {
            return Some(CompilerError::new_with_label(
                f.name_span,
                format!(
                    "`@native_required` marks an `@intrinsic` fn a backend must map natively; \
                     `{}` is `@native_required` but not `@intrinsic`",
                    f.name
                ),
            ));
        }
        if !has_luajit_mapping(f) {
            return Some(CompilerError::new_with_label(
                f.name_span,
                format!(
                    "`@native_required` intrinsic `{}` has no luajit mapping: its only body is a \
                     Rock body (a portable fallback), and native-required skips the fallback rung. \
                     Give it an `inline(lua)` body (the luajit mapping).",
                    f.name
                ),
            ));
        }
    }
    None
}

pub fn never_returns(f: &bone_core::hir::Function) -> bool {
    matches!(f.name.as_str(), "error" | "__unwrap_failed")
        && f.annotations.iter().any(|a| a.is_builtin("intrinsic"))
}
