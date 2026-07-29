//! The `@intrinsic` mechanism: the stdlib DECLARES intrinsics, backends
//! IMPLEMENT them (docs/intrinsics-plan.md, docs/stdlib-multibackend-plan.md
//! "Intrinsics design"). The portable artifact is the contract, not the mapping.
//!
//! **Body-slot rule.** A fn has one body, and that slot is the luajit story: an
//! `inline(lua)` body IS the luajit mapping; a Rock body IS the portable
//! fallback (and the differential-test reference). There is no Rust-side lua
//! table -- luajit's "table" is the set of in-source `inline(lua)` bodies.
//! Other backends (GLSL, JS) own a Rust [`IntrinsicTable`].
//!
//! **Resolution order at emit:** backend mapping > portable fallback > spanned
//! `CompilerError`. `@native_required` opts out of the fallback rung: a missing
//! backend mapping is fatal even when a fallback body exists, so a perf-critical
//! primitive can never silently degrade to a slow portable loop.
//!
//! `@struct_expand` (docs/shaders.md) is an intrinsic whose
//! emission is a compile-time [`ExpandKind`] on every backend, recognized at MIR
//! lowering rather than any per-backend table.

use bone_semantic::hir::Function;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use std::collections::HashSet;
use skull_parser::CompilerError;
use ustr::Ustr;

/// How a backend renders a call to an `@intrinsic` fn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emission {
    /// Inline expression template; `$1..$n` are argument placeholders
    /// (`sin($1)`, `Math.atan2($1, $2)`). The shipped GLSL table syntax.
    Template(&'static str),
    /// Emit the fn body once as a backend fn definition and call it.
    Body,
    /// Compile-time expansion at MIR lowering (backend-agnostic).
    Expand(ExpandKind),
}

/// A compile-time expansion kind -- the emission of an expansion intrinsic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpandKind {
    /// `materialize<T>(value: T) -> T` makes a value aggregate's runtime box
    /// explicit at this source position.
    Materialize,
    /// `@struct_expand fn f<T>(u: T) -> [(string, E)]` -> the literal pairs list
    /// (docs/shaders.md, Layer 2).
    StructPairs,
    /// `@shader_expand fn rock_shader<V, F>(vert: V, frag: F) -> string` -> the
    /// pipeline key const `"<vert>+<frag>"`; the driver emits one top-level
    /// `__rock_shader_register(key, glsl)` per distinct pair
    /// (docs/shaders.md).
    ShaderPipeline,
    /// `@vertex_expand fn f<T>(v: FlatVec<T>) -> int | [int]` -> the element's
    /// vertex layout as a literal: the stride for an `int` return, the
    /// per-attribute component counts for `[int]`. Both from one
    /// `vertex_plan`, so the two cannot drift
    /// (docs/shader-binding-layout-plan.md, P1 wiring).
    VertexLayout,
}

/// True iff `f` is an intrinsic in the mechanism's sense: declared `@intrinsic`,
/// or an expansion intrinsic (`@struct_expand`) whose annotation IS the
/// declaration surface.
pub fn is_intrinsic(f: &Function) -> bool {
    has_anno(f, "intrinsic") || expand_kind(f).is_some()
}

/// The compile-time expansion kind for `f`, or `None` if `f` is not an expansion
/// intrinsic. THE ONE recognizer -- `lower_call_repr` routes through it instead
/// of a bespoke annotation check, so there is no second ad-hoc registry.
pub fn expand_kind(f: &Function) -> Option<ExpandKind> {
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
    None
}

/// True iff `f` is `@native_required`: every backend must map it natively; the
/// portable fallback rung is skipped even when a Rock body exists.
pub fn is_native_required(f: &Function) -> bool {
    has_anno(f, "native_required")
}

/// True iff `f` is `@gpu_only`: it maps only on GPU backends, so a direct call
/// from CPU (Lua) code is rejected at emit ([`crate::lower::gpu_only_call_rejection`]).
/// The one in-tree consumer is `texture()` (stdlib/shader.rock).
pub fn is_gpu_only(f: &Function) -> bool {
    has_anno(f, "gpu_only")
}

/// True iff `f` is GPU-side code that may reach a `@gpu_only` intrinsic: a
/// `@shader(...)` entry or another `@gpu_only` fn. Shader bodies pass through the
/// Lua backend (bitgun's `heat_shimmer_frag`), so the marker exempts them.
pub fn is_gpu_context(f: &Function) -> bool {
    is_gpu_only(f) || has_anno(f, "shader")
}

/// The luajit mapping is the `inline(lua)` body; a Rock body is the fallback.
fn has_luajit_mapping(f: &Function) -> bool {
    f.inline_body.is_some()
}

fn has_anno(f: &Function, name: &str) -> bool {
    f.annotations.iter().any(|a| a.name.name.as_str() == name)
}

/// A backend's intrinsic mapping table (GLSL, JS, a future C runtime-lib table).
/// luajit has none -- its mapping is the set of in-source `inline(lua)` bodies.
/// Keyed by callee identity; the concrete key scheme is the impl's business, but
/// [`entries`](IntrinsicTable::entries) MUST enumerate the same (name, arity)
/// space so the fail-closed check ([`unbacked_entries`]) can catch signature
/// drift.
pub trait IntrinsicTable {
    /// Backend identity, for diagnostics.
    fn backend(&self) -> &'static str;
    /// The emission for callee `f`, or `None` if this backend does not map it.
    fn lookup(&self, f: &Function) -> Option<Emission>;
    /// Every entry's (name, arity) -- the fail-closed key surface.
    fn entries(&self) -> Vec<IntrinsicEntry>;
}

/// One backend-table entry's identity, for the fail-closed key check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicEntry {
    pub name: &'static str,
    pub arity: usize,
}

/// The (name, arity) of every `@intrinsic`-declared fn across the workspace --
/// the set a backend table's keys must be a subset of.
pub fn declared_intrinsics(ws: &Workspace) -> HashSet<(Ustr, usize)> {
    let mut out = HashSet::new();
    for module in ws.modules.iter() {
        for (_, f) in module.functions.iter() {
            if is_intrinsic(f) {
                out.insert((f.name, f.param_decls.len()));
            }
        }
    }
    out
}

/// Fail-closed key check: every entry of `table` MUST correspond to an
/// `@intrinsic` declaration in `declared` (its (name, arity) set from
/// [`declared_intrinsics`]). A key matching no declaration means the stdlib
/// signature drifted out from under the mapping -- returned here so a test fails
/// loudly rather than the mapping silently missing at a call site. Pure data in,
/// pure data out (no `Workspace`) so the algorithm is unit-testable on its own.
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

/// Emit-time `@intrinsic` validation over the user module `mid` (the luajit
/// coverage rung; GLSL/JS coverage lives in their own emit paths). Rejects, with
/// a span:
///
/// - a `@native_required` fn that is not also `@intrinsic` -- the marker only
///   means "backends must map this intrinsic natively", meaningless alone;
/// - a `@gpu_only` fn that is not also `@intrinsic` -- likewise meaningless
///   alone: the marker only names an intrinsic that CPU backends do not map;
/// - a `@native_required @intrinsic` fn with no luajit mapping (its only body is
///   a Rock body): native-required skips the fallback rung, so there is nothing
///   for luajit to fall back to.
///
/// On luajit every other `@intrinsic` fn is trivially covered (its body is
/// either the mapping or the fallback); the coverage rung has real teeth for
/// GLSL now and JS later, in those backends' emit paths.
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
