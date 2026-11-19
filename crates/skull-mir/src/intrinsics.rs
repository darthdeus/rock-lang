//! A fn has one body, and it is the LuaJIT story: an `inline(lua)` body is the
//! mapping and a Rock body is the portable fallback. GLSL and JS own an
//! [`IntrinsicTable`]. Emit resolves backend mapping, then portable fallback,
//! then a spanned error; `@native_required` skips the fallback so a
//! perf-critical primitive never silently degrades to a slow portable loop.
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
    /// A callable backend global. Variadic calls pass every supplied argument;
    /// function values refer to the same global.
    Global(&'static str),
    Body,
    Expand(ExpandKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpandKind {
    SceneTypeKey,
    /// Makes a value aggregate's runtime box explicit at this source position.
    Materialize,
    StructPairs,
    /// The pipeline key const `"<vert>+<frag>"`; the driver emits one top-level
    /// `__rock_shader_register(key, glsl)` per distinct pair.
    ShaderPipeline,
    /// The stride and the per-attribute component counts both come from one
    /// `vertex_plan`, so the two cannot drift.
    VertexLayout,
    /// One direct typed store per primitive leaf at the std140 offsets
    /// `gpu_layout::uniform_block` computes, never a per-leaf helper call.
    UniformBlock,
    /// Undeclared `[build.vars]` names reject.
    BuildVar,
    /// The path is resolved against `Workspace.comptime_inputs.embed_root`.
    EmbedString,
    UniformValues,
    /// A bool const: an `if` on it lowers only the selected arm, so the other
    /// arm's callees are never emitted.
    SortKeyWide,
    /// Compares `key_fn(a) < key_fn(b)` leaf by leaf, first leaf first.
    SortKeyLt,
}

pub fn is_intrinsic(f: &Function) -> bool {
    has_anno(f, "intrinsic") || expand_kind(f).is_some()
}

/// The one recognizer of expansion intrinsics; `lower_call_repr` routes
/// through it, so there is no second registry.
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

/// Every backend must map `f` natively; the portable fallback is skipped even
/// when a Rock body exists.
pub fn is_native_required(f: &Function) -> bool {
    has_anno(f, "native_required")
}

/// Maps only on GPU backends, so a direct call from CPU (Lua) code is
/// rejected at emit.
pub(crate) fn is_gpu_only(f: &Function) -> bool {
    has_anno(f, "gpu_only")
}

/// Shader bodies pass through the Lua backend too, so this marker exempts them
/// from the `@gpu_only` rejection.
pub(crate) fn is_gpu_context(f: &Function) -> bool {
    is_gpu_only(f) || has_anno(f, "shader")
}

/// The luajit mapping is the `inline(lua)` body; a Rock body is the fallback.
fn has_luajit_mapping(f: &Function) -> bool {
    f.inline_body.is_some()
}

fn has_anno(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.is_builtin(name))
}

/// [`entries`](IntrinsicTable::entries) must enumerate the same (key, arity)
/// space as [`declared_intrinsics`], so [`unbacked_entries`] catches drift.
pub trait IntrinsicTable {
    fn backend(&self) -> &'static str;
    fn lookup(&self, f: &Function) -> Option<Emission>;
    fn entries(&self) -> Vec<IntrinsicEntry>;
}

/// How a backend supplies a callee itself, in the backend's own precedence.
/// Its call rendering and callee discovery must branch on exactly this, so the
/// artifact and the emitter cannot drift.
pub trait BackendRealization {
    fn backend(&self) -> &'static str;
    /// True iff this backend renders or serves `f` without emitting any declared
    /// body for it.
    fn supplies(&self, ws: &Workspace, f: &Function) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicEntry {
    pub name: &'static str,
    pub arity: usize,
}

/// The (key, arity) set a backend table's keys must be a subset of.
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

/// Fail-closed key check: every `table` entry must match an `@intrinsic`
/// declaration in `declared`; a stray key means the declaration drifted.
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

/// Rejects `@native_required` or `@gpu_only` without `@intrinsic`, and a
/// `@native_required @intrinsic` fn with no luajit mapping: native-required
/// skips the fallback, so luajit has nothing to fall back to.
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

/// A block ends at such a call, so lowering emits nothing after it.
pub fn never_returns(f: &bone_core::hir::Function) -> bool {
    matches!(f.name.as_str(), "error" | "__unwrap_failed")
        && f.annotations.iter().any(|a| a.is_builtin("intrinsic"))
}
