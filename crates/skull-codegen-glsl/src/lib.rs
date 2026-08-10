//! skull-codegen-glsl -- GLSL function-library emission (route B, `docs/shaders.md`).
//!
//! A second codegen backend over the shared `skull-mir` substrate (sibling of
//! `skull-codegen-lua`). Given a typechecked [`Workspace`] and a set of root fn
//! names, [`emit_shader_library`] emits GLSL function definitions for the roots
//! plus their reachable callees, topologically ordered (callees before callers).
//!
//! Two things make this a RESTRICTED profile, not a general backend:
//!   - Value structs stay first-class `vecN` (the `ScalarCtx` values-first-class
//!     flag), never the luajit scalar-leaf ABI.
//!   - A strict profile pass rejects everything above tier 0 (recursion, strings,
//!     `Vec`/`Map`/`Arena`, optionals, closures, `NativeBuffer`, coroutines, and
//!     any call with neither a mapping-table entry nor an emittable body). Never
//!     skip-and-continue; never emit a guess.
//!
//! The tier-0 intrinsic mapping table ([`intrinsic_template`]) is the hand-written
//! spec from `docs/shaders.md`, keyed by the callee's mangled signature (name +
//! GLSL param types) -- the future `@intrinsic` GLSL column.

mod structure;

pub use skull_mir::shader_ir::GlslProfile;
use bone_semantic::hir::{
    AnnoValue, BodyOwner, BuiltinTy, Expr, Function, StructDef, TyId, TypeCtor, TypeExpr,
};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{FnList, ModuleId, Span};
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_codegen_lua::mangle_fn;
use skull_graph::instance_graph::{InstanceGraph, SiteOutcome};
pub use skull_graph::shader::ShaderLibraryView;
use skull_graph::shader::{ShaderProgram, ShaderProgramView, ShaderStageView};
use skull_mir::cfg::reverse_postorder;
use skull_mir::gpu_layout::{
    ArrayMemberDecl, AttrKind, SamplerKind, UniformArrayBlockPlan, UniformBlockPlan, WireKind,
    texture_slots, uniform_array_block, vertex_attrs,
};
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};
use skull_mir::concrete::ConcretePlans;
use skull_mir::lower::{lower_instance_with_plan, ScalarCtx, SpecSubst};
use skull_mir::instance::Instance;
use skull_mir::monoty::MonoInterner;
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_mir::shader_ir::{
    FragmentOutput, LogicalSampler, ShaderIR, ShaderStageIr,
    LogicalArrayGroup, LogicalTextureGroup, LogicalUniformGroup, LogicalVarying,
    LogicalVertexInput, ShaderInstance, ShaderInterfacePlan, ShaderStage,
    ShaderStage as IrStage, UniformRole,
    VertexRate,
};
use skull_mir::mir::*;
use skull_parser::CompilerError;
use std::collections::BTreeSet;
use std::sync::Arc;
use structure::{LoopRegion, Structure};
use ustr::Ustr;

/// A tier-0 GLSL type. Everything else is rejected by the profile pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GlslTy {
    Float,
    Int,
    Bool,
    Vec2,
    Vec3,
    Vec4,
    /// Bool-vector; LOCAL/TEMP only (GLSL forbids bool interpolants, and the tier
    /// has no bvec upload path -- rejected in attr/varying/uniform position).
    Bvec3,
    Mat4,
    /// Opaque; param/uniform position only (GLSL forbids sampler locals/returns).
    Sampler2D,
    /// Integer sampler (the R32I entity-id tier). Same opacity rules as
    /// Sampler2D; read via texture_id().
    ISampler2D,
}

impl GlslTy {
    fn name(self) -> &'static str {
        match self {
            GlslTy::Float => "float",
            GlslTy::Int => "int",
            GlslTy::Bool => "bool",
            GlslTy::Vec2 => "vec2",
            GlslTy::Vec3 => "vec3",
            GlslTy::Vec4 => "vec4",
            GlslTy::Bvec3 => "bvec3",
            GlslTy::Mat4 => "mat4",
            GlslTy::Sampler2D => "sampler2D",
            GlslTy::ISampler2D => "isampler2D",
        }
    }

    fn is_sampler(self) -> bool {
        matches!(self, GlslTy::Sampler2D | GlslTy::ISampler2D)
    }

    /// Uniform-declaration spelling. isampler2D carries `highp`: GLSL ES 3.00
    /// gives integer samplers NO default precision (an unqualified declaration
    /// is a compile error there); desktop GLSL accepts and ignores it.
    fn uniform_decl(self) -> &'static str {
        match self {
            GlslTy::ISampler2D => "highp isampler2D",
            other => other.name(),
        }
    }
}

fn err(span: Span, msg: impl Into<String>) -> CompilerError {
    CompilerError::new_with_label(span, msg.into())
}

/// The `TypeExpr` behind a static TyId (declared signature/field types are static).
fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

/// The tier-0 GLSL type of a nominal, or `None`.
fn nominal_glsl_ty(n: &str) -> Option<GlslTy> {
    match n {
        "Vec2" => Some(GlslTy::Vec2),
        "Vec3" => Some(GlslTy::Vec3),
        "Vec4" => Some(GlslTy::Vec4),
        "Bvec3" => Some(GlslTy::Bvec3),
        "Mat4" => Some(GlslTy::Mat4),
        "Sampler2D" => Some(GlslTy::Sampler2D),
        "ISampler2D" => Some(GlslTy::ISampler2D),
        _ => None,
    }
}

/// The tier-0 GLSL type of a concrete logical type -- the shape a generic is
/// BOUND to at the instance being emitted. Same tier-0 admission as the
/// declared-type path, asked of `MonoTy` instead of `TypeExpr`.
fn mono_glsl_ty(t: &MonoTy) -> Option<GlslTy> {
    match t {
        MonoTy::Float => Some(GlslTy::Float),
        MonoTy::Int => Some(GlslTy::Int),
        MonoTy::Bool => Some(GlslTy::Bool),
        MonoTy::App { ctor: MonoCtor::Nominal(n), .. } => nominal_glsl_ty(n.as_str()),
        _ => None,
    }
}

/// Render a Rock type to its tier-0 GLSL type UNDER `subst`, or reject.
///
/// A declared `T` has no GLSL type; the instance's binding for `T` does. This
/// is why the same helper is tier 0 at `T = float` and rejected at `T = string`
/// -- a verdict about the declaration is a verdict about neither.
fn glsl_ty(
    ws: &Workspace,
    subst: &SpecSubst,
    ty: TyId,
    span: Span,
    what: &str,
) -> Result<GlslTy, CompilerError> {
    let reject = || {
        err(span, format!("shader profile: {what} type is not tier 0 (only float/int/bool/Vec2/Vec3/Vec4)"))
    };
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Ok(GlslTy::Float),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Ok(GlslTy::Int),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Ok(GlslTy::Bool),
        Some(TypeExpr::Generic(var, name)) => match subst.get(&var) {
            Some(bound) => mono_glsl_ty(bound).ok_or_else(|| {
                err(span, format!(
                    "shader profile: {what} type `{name}` is bound to `{}`, which is not tier 0 \
                     (only float/int/bool/Vec2/Vec3/Vec4)",
                    bound.render()
                ))
            }),
            None => Err(reject()),
        },
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => nominal_glsl_ty(n.as_str())
            .ok_or_else(|| err(span, format!(
                "shader profile: {what} type `{n}` is not tier 0 (only float/int/bool/Vec2/Vec3/Vec4)"
            ))),
        _ => Err(reject()),
    }
}

/// Format an `f64` literal as GLSL, always keeping a decimal point (GLSL 330 is
/// strict: `float x = 1;` is an error, so `1.0` must never render as `1`).
fn fmt_float(f: f64) -> String {
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') || s.contains('E') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{s}.0")
    }
}

// --- const globals ----------------------------------------------------------

/// Depth cap on `const A = B` alias chains. A cycle is impossible in a
/// well-formed program (bone rejects one), so this only bounds a pathological
/// chain rather than implementing cycle detection.
const CONST_CHAIN_DEPTH: usize = 16;

/// Fold a `const` global to its literal value, or `None` if it is not a `const`
/// or its initializer is not a constant expression.
///
/// GLSL has no module-level mutable state, so a `global` read can never lower.
/// A `const` is different in kind: bone enforces it read-only
/// (`ws.const_globals`, rejected at any rebind), so baking its value into the
/// shader cannot drift from the CPU-side value. It folds at the use site rather
/// than emitting a GLSL `const` declaration -- a declaration would have to join
/// the one cross-stage namespace shared by uniforms, varyings, attributes and
/// samplers, and collide-check against it, for no gain the folded literal
/// doesn't already give.
///
/// Only literals and alias chains fold today. Arithmetic (`const TAU = PI * 2`)
/// lowers to an operator CALL in HIR, not a `BinOp`, so folding it means
/// evaluating calls -- deliberately out of scope; such a const rejects with a
/// message naming the real cause.
fn const_lit(ws: &Workspace, name: Ustr) -> Option<Lit> {
    const_lit_depth(ws, name, 0)
}

fn const_lit_depth(ws: &Workspace, name: Ustr, depth: usize) -> Option<Lit> {
    if depth > CONST_CHAIN_DEPTH || !ws.const_globals.contains(&name) {
        return None;
    }
    let (module, gid) = ws.modules.iter().find_map(|m| {
        m.globals
            .iter()
            .find(|(_, g)| g.name == name && g.is_const)
            .map(|(gid, _)| (m, gid))
    })?;
    let body = module.bodies.get(&BodyOwner::Global(gid))?;
    fold_const_expr(ws, module, body, body.root?, depth)
}

fn fold_const_expr(
    ws: &Workspace,
    module: &bone_semantic::hir::Module,
    body: &bone_semantic::hir::Body,
    e: Idx<Expr>,
    depth: usize,
) -> Option<Lit> {
    match body.expr_arena[e] {
        Expr::Int(i) => Some(Lit::Int(i)),
        Expr::Float(f) => Some(Lit::Float(f.into_inner())),
        Expr::Bool(b) => Some(Lit::Bool(b)),
        Expr::UnaryMinus(inner) => match fold_const_expr(ws, module, body, inner, depth)? {
            Lit::Int(i) => Some(Lit::Int(-i)),
            Lit::Float(f) => Some(Lit::Float(-f)),
            _ => None,
        },
        Expr::Global(g) => const_lit_depth(ws, g, depth + 1),
        _ => None,
    }
}

/// The GLSL type a folded const literal renders at.
fn const_lit_ty(lit: &Lit) -> GlslTy {
    match lit {
        Lit::Int(_) => GlslTy::Int,
        Lit::Bool(_) => GlslTy::Bool,
        _ => GlslTy::Float,
    }
}

/// Render a folded const literal as a GLSL expression.
fn render_const_lit(lit: &Lit) -> Option<String> {
    match lit {
        Lit::Float(f) => Some(fmt_float(*f)),
        Lit::Int(i) => Some(i.to_string()),
        Lit::Bool(b) => Some(b.to_string()),
        Lit::Nil | Lit::Str(_) => None,
    }
}

// --- profile pass ------------------------------------------------------------

/// The GLSL name of a shader instance. The symbol is target-neutral
/// ([`ShaderInstance::symbol`]); this is the seam where a backend that needed a
/// different spelling would rename, and GLSL does not.
fn glsl_name(ws: &Workspace, interner: &MonoInterner, inst: &ShaderInstance) -> String {
    inst.symbol(ws, interner)
}

/// Reject a signature whose params/return aren't tier 0, as DECLARED.
///
/// Public because the mono SHADER ARTIFACT VIEW asks it of what a target
/// packages: the same function the emission walk runs, so a view cannot admit
/// a signature emission refuses. A generic declaration is answered under its
/// instantiation instead -- see [`check_tier0_signature_spec`].
pub fn check_tier0_signature(ws: &Workspace, f: &Function) -> Result<(), CompilerError> {
    check_tier0_signature_spec(ws, &SpecSubst::default(), f)
}

fn check_tier0_signature_spec(
    ws: &Workspace,
    subst: &SpecSubst,
    f: &Function,
) -> Result<(), CompilerError> {
    for pd in &f.param_decls {
        glsl_ty(ws, subst, pd.ty, f.name_span, &format!("parameter `{}`", pd.name))?;
    }
    if let Some(rt) = f.return_ty {
        if glsl_ty(ws, subst, rt, f.name_span, "return")?.is_sampler() {
            return Err(err(f.name_span, "shader profile: a sampler cannot be returned (GLSL forbids sampler locals/returns)"));
        }
    }
    Ok(())
}

/// Walk a fn's MIR, rejecting any non-tier-0 instruction and collecting the
/// INSTANCES it calls directly.
///
/// The callee's instantiation is the one lowering already recorded at the call
/// (`MirFunc::spec_bindings`) -- the same record the Lua and JS backends read,
/// so GLSL cannot disagree with them about which body a call names.
fn profile_body(
    ws: &Workspace,
    f: &Function,
    mf: &MirFunc,
    callees: &mut Vec<ShaderInstance>,
) -> Result<(), CompilerError> {
    let span = f.name_span;
    for (iid, inst) in mf.insts.iter() {
        match inst {
            Inst::Call { func, .. } => {
                callees.push(callee_inst_at(ws, mf, iid, *func, span)?)
            }
            // A `const` global folds to its literal at the use site; a mutable
            // `global` is runtime state with no GLSL equivalent.
            Inst::Global(name) => {
                if const_lit(ws, *name).is_none() {
                    if ws.const_globals.contains(name) {
                        return Err(err(span, format!(
                            "shader profile: const `{name}` has an initializer that is not a constant expression -- only literals and const aliases fold for GLSL"
                        )));
                    }
                    return Err(err(span, format!(
                        "shader profile: `{}` uses a non-tier-0 construct (global read) that has no GLSL lowering -- a mutable `global` has no GLSL equivalent; a `const` folds to its literal",
                        f.name
                    )));
                }
            }
            Inst::Const(Lit::Str(_)) => {
                return Err(err(span, format!("shader profile: `{}` uses a non-tier-0 construct (string literal) that has no GLSL lowering", f.name)));
            }
            // A bare `Nil` const may be a dead placeholder (`return` in expression
            // position emits an unused nil, `lower.rs`); a genuinely-used null /
            // optional is rejected when it renders (`render_val`). Rejecting every
            // nil here would refuse `if a { return .. } else { .. }`.
            Inst::Const(Lit::Nil) => {}
            // Allowed tier-0 instructions.
            Inst::Const(_)
            | Inst::Load(_)
            | Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..)
            | Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..)
            | Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..)
            | Inst::Neg(_) | Inst::Not(_) | Inst::FloatToInt(_) | Inst::IntToFloat(_)
            | Inst::MakeValueStruct { .. }
            | Inst::MaterializeValueStruct { .. }
            | Inst::Field { .. }
            | Inst::Store { .. } => {}
            // Everything else is above tier 0 (strings, containers, closures,
            // coroutines, enums, native buffers, bit ops, globals, ...).
            other => {
                return Err(err(
                    span,
                    format!("shader profile: `{}` uses a non-tier-0 construct ({}) that has no GLSL lowering", f.name, inst_kind(other)),
                ));
            }
        }
    }
    Ok(())
}

/// A short human name for a rejected instruction kind (for the error message).
fn inst_kind(inst: &Inst) -> &'static str {
    match inst {
        Inst::Global(_) => "global read",
        Inst::StringConcat(..) | Inst::ToString(_) | Inst::Format { .. } | Inst::FormatDyn { .. } => "string",
        Inst::BitAnd(..) | Inst::BitOr(..) | Inst::BitXor(..) | Inst::Shl(..) | Inst::Shr(..) | Inst::BitNot(_) => "bitwise op",
        Inst::CallMulti { .. } | Inst::CallValue { .. } | Inst::CallValueMulti { .. } | Inst::CallBoundMulti { .. } => "indirect/multi-value call",
        Inst::Lambda { .. } => "closure",
        Inst::MakeTuple(_) => "tuple",
        Inst::MakeVec(_) => "Vec",
        Inst::MakeStruct { .. } => "heap struct",
        Inst::MakeEnum { .. } | Inst::MakeEnumDyn { .. } => "enum",
        Inst::Index { .. } | Inst::Len { .. } | Inst::ArrayPush { .. } | Inst::StoreIndex { .. } => "container op",
        Inst::StoreField { .. } => "field mutation",
        Inst::StoreGlobal { .. } => "global write",
        Inst::CoroSetInner(_) | Inst::CoroHook { .. } | Inst::MapNext { .. } => "coroutine/iteration",
        _ => "unsupported instruction",
    }
}

// --- intrinsic mapping table ------------------------------------------------

/// The GLSL tier-0 intrinsic table (`docs/shaders.md`): one row per
/// (name, arity) -> `$1..$n` expression template. Single source for both
/// [`GlslIntrinsics::lookup`] (call-site rendering) and
/// [`GlslIntrinsics::entries`] (the fail-closed key surface), so the two can
/// never drift. Keyed by base name + arity (a user fn that merely shares a name
/// still resolves here -- acceptable for the restricted profile; the stdlib owns
/// these names). Vector operators (`__op_*` reaching codegen are non-scalar, so
/// Vec) are handled at the call site, not here.
const GLSL_INTRINSICS: &[(&str, usize, &str)] = &[
    ("sin", 1, "sin($1)"),
    ("cos", 1, "cos($1)"),
    ("tan", 1, "tan($1)"),
    ("sqrt", 1, "sqrt($1)"),
    ("exp", 1, "exp($1)"),
    ("abs", 1, "abs($1)"),
    ("floor", 1, "floor($1)"),
    ("ceil", 1, "ceil($1)"),
    ("fract", 1, "fract($1)"),
    ("sign", 1, "sign($1)"),
    ("pow", 2, "pow($1, $2)"),
    ("atan2", 2, "atan($1, $2)"),
    // GLSL round() half-way rounding is implementation-defined; pin floor(x+0.5).
    ("round", 1, "floor($1 + 0.5)"),
    ("min", 2, "min($1, $2)"),
    ("max", 2, "max($1, $2)"),
    ("clamp", 3, "clamp($1, $2, $3)"),
    ("lerp", 3, "mix($1, $2, $3)"),
    ("smoothstep", 3, "smoothstep($1, $2, $3)"),
    // int<->float identity casts.
    ("int_to_float", 1, "float($1)"),
    ("float", 1, "float($1)"),
    ("to_float", 1, "float($1)"),
    // Vec method helpers (the field-math bodies never emit). Keyed by the base
    // method name, which mangling strips the receiver-type prefix from
    // (`Vec2.dot` -> `dot`); GLSL `dot`/`length`/`distance` are overloaded across
    // vecN, so one entry covers Vec2/Vec3/Vec4.
    ("dot", 2, "dot($1, $2)"),
    ("length", 1, "length($1)"),
    ("distance", 2, "distance($1, $2)"),
    // NO normalize row: Rock's Vec2.normalize is zero-safe (returns zero()),
    // GLSL's native normalize is undefined at 0 -- the Rock body emits as a
    // helper fn instead (pinned by vec2_normalize_body).
    ("v", 4, "vec4($1, $2, $3, $4)"),
    ("v", 3, "vec3($1, $2, $3)"),
    ("v", 2, "vec2($1, $2)"),
    ("v", 1, "vec2($1)"),
    ("v", 0, "vec2(0.0)"),
    ("zero", 0, "vec2(0.0)"),
    ("texture", 2, "texture($1, $2)"),
    // Integer-sampler id read (R32I tier): texture() on isampler2D returns
    // ivec4; the id is the only channel anyone reads.
    ("texture_id", 2, "texture($1, $2).r"),
    // textureSize returns ivec2; wrap in vec2(...) so the result is a float Vec2
    // (matches Rock's `-> Vec2`, and the gl_FragCoord/size divides it feeds).
    ("texture_size", 1, "vec2(textureSize($1, 0))"),
    // GPU builtins (docs/shaders.md). frag_coord/discard take no
    // args -> bare builtins (discard renders as the statement `discard;`, see
    // emit_block_effects). Fragment-stage gating lives in the pipeline emitter.
    ("frag_coord", 0, "gl_FragCoord"),
    ("fwidth", 1, "fwidth($1)"),
    ("discard", 0, "discard"),
    ("vertex_id", 0, "gl_VertexID"),
    // Uniform-array indexing (decision 3): `at(arr, i)` on an UniformArray<T>
    // param renders as plain GLSL indexing of the declared uniform array.
    ("at", 2, "$1[$2]"),
    // Bool-vector tier (docs/shaders.md). isnan/isinf are GLES 3.00 builtins.
    // `any` here is arity-1 (Bvec3 reduction) -- distinct from the iterator
    // `any` (arity-2, not a shader intrinsic). `greater_than`/`to_vec3` carry
    // portable Rock bodies for luajit/JS; these templates render the native
    // GLSL inline so the bodies never emit on the shader path.
    ("isnan", 1, "isnan($1)"),
    ("isinf", 1, "isinf($1)"),
    // The native bvec3 ctor: inline, since a helper fn named `bvec3` would
    // collide with the GLSL builtin type constructor.
    ("bvec3", 3, "bvec3($1, $2, $3)"),
    ("any", 1, "any($1)"),
    ("greater_than", 2, "greaterThan($1, $2)"),
    ("to_vec3", 1, "vec3($1)"),
];

/// The GLSL intrinsic template for a callee, or `None` if it must be emitted as
/// a GLSL fn definition. Backs [`GlslIntrinsics::lookup`].
fn intrinsic_template(f: &Function) -> Option<&'static str> {
    let n = f.name.as_str();
    let arity = f.param_decls.iter().count();
    GLSL_INTRINSICS
        .iter()
        .find(|(name, ar, _)| *name == n && *ar == arity)
        .map(|(_, _, tmpl)| *tmpl)
}

/// The GLSL backend's [`IntrinsicTable`] -- the registry surface over
/// [`GLSL_INTRINSICS`]. `lookup` is the tier-0 template; `entries` is the
/// fail-closed key set (`unbacked_entries` checks each against the stdlib's
/// `@intrinsic` declarations).
pub struct GlslIntrinsics;

impl IntrinsicTable for GlslIntrinsics {
    fn backend(&self) -> &'static str {
        "glsl"
    }
    fn lookup(&self, f: &Function) -> Option<Emission> {
        intrinsic_template(f).map(Emission::Template)
    }
    fn entries(&self) -> Vec<IntrinsicEntry> {
        GLSL_INTRINSICS
            .iter()
            .map(|(name, arity, _)| IntrinsicEntry { name, arity: *arity })
            .collect()
    }
}

/// Whether a callee that survived MIR lowering is rendered inline at call sites
/// rather than emitted as a GLSL fn definition. Scalar operators do not reach
/// this predicate: lowering turns them into native MIR instructions.
pub fn renders_inline(f: &Function) -> bool {
    GlslIntrinsics.lookup(f).is_some()
        || (vec_op_symbol(f.name.as_str()).is_some() && f.param_decls.iter().count() == 2)
}

/// The graph sees HIR call obligations before scalar operators are lowered away,
/// so it needs the concrete argument types to distinguish a vector infix from a
/// scalar MIR instruction.
pub fn renders_inline_instance(
    ws: &Workspace,
    f: &Function,
    inst: &Instance,
    interner: &MonoInterner,
) -> bool {
    if GlslIntrinsics.lookup(f).is_some() {
        return true;
    }
    if vec_op_symbol(f.name.as_str()).is_none() || f.param_decls.iter().count() != 2 {
        return false;
    }
    let subst = inst.subst(ws, interner);
    f.param_decls.iter().any(|param| {
        matches!(
            glsl_ty(ws, &subst, param.ty, f.name_span, "operator").ok(),
            Some(GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4 | GlslTy::Mat4)
        ) || matches!(
            ws.render(None, param.ty).as_str(),
            "Vec2" | "Vec3" | "Vec4" | "Mat4"
        )
    })
}

/// A HIR arithmetic call that lowering removes into an `Inst::*` operation. It
/// is a graph leaf, but it is not a GLSL helper/builtin population.
pub fn is_lowered_native_operator(
    ws: &Workspace,
    f: &Function,
    inst: &Instance,
    interner: &MonoInterner,
) -> bool {
    if !matches!(
        f.name.as_str(),
        "__op_add" | "__op_sub" | "__op_mul" | "__op_div" | "__op_mod"
    ) || f.param_decls.iter().count() != 2
        || renders_inline_instance(ws, f, inst, interner)
    {
        return false;
    }
    let subst = inst.subst(ws, interner);
    f.param_decls.iter().all(|param| {
        matches!(
            glsl_ty(ws, &subst, param.ty, f.name_span, "operator").ok(),
            Some(GlslTy::Float | GlslTy::Int)
        ) || matches!(ws.render(None, param.ty).as_str(), "float" | "int")
    })
}

/// The GLSL infix operator symbol for a non-scalar `__op_*` (Vec operator) call.
fn vec_op_symbol(name: &str) -> Option<&'static str> {
    match name {
        "__op_add" => Some("+"),
        "__op_sub" => Some("-"),
        "__op_mul" => Some("*"),
        "__op_div" => Some("/"),
        _ => None,
    }
}

// --- per-function emission ---------------------------------------------------

/// A cosmetic-tier fold of a lowered `&&`/`||`/expression-`if` diamond back into
/// one GLSL expression (`docs/shaders.md`). The diamond stored its
/// result into a single-use cell across two single-store arms; instead of
/// emitting the diamond we render this expression at the cell's one use.
#[derive(Clone, Copy)]
struct FoldedDiamond {
    cond: Idx<Inst>,
    then_val: Idx<Inst>,
    else_val: Idx<Inst>,
    kind: FoldKind,
    /// The result cell's GLSL type -- ternary arms render at this type so an
    /// int-in-float-context arm keeps the coercion the diamond's store applied.
    result_ty: GlslTy,
}

#[derive(Clone, Copy, PartialEq)]
enum FoldKind {
    /// `if a then b else false` == `(a && b)`.
    And,
    /// `if a then true else b` == `(a || b)`.
    Or,
    /// `if a then b else c` == `(a ? b : c)`.
    Ternary,
}

struct FnEmit<'a> {
    ws: &'a Workspace,
    /// The build's one arena: a callee instance's arguments are ids into it, so
    /// the name a call renders and the substitution its result types resolve
    /// through both read it.
    interner: &'a MonoInterner,
    f: &'a MirFunc,
    /// GLSL return type (None = void); floats coerce int-literal returns.
    ret_ty: Option<GlslTy>,
    /// Cell -> emitted GLSL identifier.
    cell_name: HashMap<Idx<Cell>, String>,
    /// Cell -> GLSL type (params from signature, others from first store).
    cell_ty: HashMap<Idx<Cell>, GlslTy>,
    /// Loop headers matching the range-`for` shape -> their counted-loop info.
    counted: HashMap<Idx<Block>, LoopInfo>,
    /// Other natural-loop headers (general `while`) -> their region.
    general: HashMap<Idx<Block>, LoopRegion>,
    /// Dominators / post-dominators / natural loops.
    structure: Structure,
    /// Loop headers currently being emitted (so the header region walk doesn't
    /// re-wrap them as a fresh loop).
    active: HashSet<Idx<Block>>,
    out: String,
    indent: usize,
    /// Guard against unstructured control flow / infinite walks.
    visited: HashSet<Idx<Block>>,
    /// Entry mode: the cells of the "global-base" struct params -- attribute,
    /// uniform, and varyings structs, whose field reads render as bare GLSL
    /// globals (attributes/uniforms/varyings are globals, not struct members).
    global_cells: HashSet<Idx<Cell>>,
    /// Entry mode: global field name -> GLSL type (for `ty_of` on those reads).
    /// Names are collision-checked unique, so one combined map covers all bases.
    uniform_fields: HashMap<Ustr, GlslTy>,
    /// Cosmetic folds (`docs/shaders.md`, cosmetic tier). A diamond's
    /// single-use result cell -> the expression it renders as; the diamond itself
    /// is skipped, its declaration + stores suppressed.
    folded_cond: HashMap<Idx<Cell>, FoldedDiamond>,
    /// The branch block a `folded_cond` diamond starts at -> its result cell
    /// (so `emit_seq` skips the diamond and jumps to its join).
    folded_block: HashMap<Idx<Block>, Idx<Cell>>,
    /// A counted-loop bound temp folded into the for-header -> its const value.
    folded_bound: HashMap<Idx<Cell>, Idx<Inst>>,
    /// `void main()` mode: the fn is emitted as `void main()` and its terminal
    /// `return <MakeValueStruct>` destructures into a named global assignment per
    /// field (vertex varyings, or MRT fragment outputs).
    main_out: Option<MainOut>,
    /// The fn's name span -- body-level emit errors anchor here (MIR insts carry
    /// no spans, so per-expression precision is not available).
    fn_span: Span,
    /// The INSTANCE being emitted: this body's own generic bindings. Every
    /// declared type it renders resolves through this, so `T` is `float` here
    /// and `int` in the sibling instance of the same declaration.
    subst: SpecSubst,
    /// Every direct call in this body, resolved to its callee INSTANCE by
    /// [`callee_inst_at`]. Built once so the name a call renders and the type
    /// its result takes come from one answer.
    callee_insts: HashMap<Idx<Inst>, ShaderInstance>,
}

impl FnEmit<'_> {
    /// The instantiation the call at `iid` reaches its callee at.
    ///
    /// A LOOKUP, not a second derivation: [`callee_inst_at`] resolved and
    /// validated every call site before this body could reach emission, and
    /// `emit_fn_def_with` recorded the answers. A miss is compiler state --
    /// the map is built from the same `mf.insts` walk this renders from.
    fn callee_inst(&self, iid: Idx<Inst>) -> &ShaderInstance {
        self.callee_insts.get(&iid).expect(
            "every Call in this body was resolved by `callee_inst_at` before emission",
        )
    }
}

/// **The one resolver of a call site's callee instance, and it FAILS CLOSED.**
///
/// `MirFunc::spec_bindings` holds the instantiation lowering recorded at each
/// call. Reading an absent entry as "no instantiation" is only correct for a
/// callee that HAS no generics; for a generic one it silently substitutes the
/// declaration's shape, which on GLSL means emitting or typing a body no
/// instance has. So an absent or PARTIAL key for an emitted generic callee is
/// a rejection with the caller's span, never a fallback.
///
/// A callee that renders inline is exempt and is the reason the rule is not
/// simply "generic implies key": `min<T>`, `max<T>`, `clamp` and `v(..)` are
/// generic declarations that GLSL spells as native builtins at the call site,
/// so they are never emitted as definitions and their key is never consulted.
fn callee_inst_at(
    ws: &Workspace,
    mf: &MirFunc,
    iid: Idx<Inst>,
    func: FuncRef,
    span: Span,
) -> Result<ShaderInstance, CompilerError> {
    let cf = &ws.module(func.module).functions[func.func];
    // Absent is not "the declaration": a callee with no generics HAS no
    // instantiation to record, and one that is generic and recorded nothing is
    // the rejection below.
    let recorded = mf.spec_bindings.get(&iid).cloned();
    if renders_inline(cf) || cf.generics.is_empty() {
        return Ok(ShaderInstance::new(recorded.unwrap_or_else(|| Instance::base(func))));
    }
    let key = recorded.as_ref().map(|i| i.args.clone()).unwrap_or_default();
    // Covers both shapes: a call that recorded nothing, and one whose key the
    // producer declined to build because a type parameter did not reify. Those
    // used to be two arms, the second scanning for a `None` slot; a key is
    // total now, so that state is unrepresentable rather than merely unreached,
    // and its message is folded in here.
    if key.is_empty() {
        return Err(err(
            span,
            format!(
                "shader profile: the call to generic `{}` has no recorded instantiation -- either \
                 none was recorded, or a type parameter was left uninstantiated -- so GLSL has no \
                 concrete shape to emit it at. A GPU has no boxed representation for an unreified \
                 generic, so this cannot fall back to the declaration",
                cf.name
            ),
        ));
    }
    // The key is POSITIONAL against the declared generics, so a different
    // length names another function's instantiation. `spec_subst_from_key` zips
    // the two and would silently drop the tail or substitute a prefix.
    if key.len() != cf.generics.len() {
        return Err(err(
            span,
            format!(
                "shader profile: the call to generic `{}` recorded {} type argument(s) for {} \
                 declared generic(s) -- the instantiation is positional, so a key of another \
                 length is not this callee's",
                cf.name,
                key.len(),
                cf.generics.len()
            ),
        ));
    }
    Ok(ShaderInstance::new(recorded.expect("a non-empty key implies a recorded instance")))
}

/// A `void main()` entry that destructures its terminal `return <struct
/// literal>` into named global assignments: vertex varyings (+ gl_Position) or
/// MRT fragment outputs (`layout(location=N) out vec4`).
#[derive(Clone)]
struct MainOut {
    /// Field name -> GLSL type (the assignment target's type).
    fields: HashMap<Ustr, GlslTy>,
    /// Vertex only: a `position` field becomes `gl_Position` (suppressed as a
    /// named output). Fragment MRT has no such remap -- every field is an out.
    position_is_gl: bool,
}

#[derive(Clone, Copy)]
struct LoopInfo {
    slot: Idx<Cell>,
    bound: Idx<Inst>,
    inclusive: bool,
    body_entry: Idx<Block>,
    latch: Idx<Block>,
    exit: Idx<Block>,
}

/// The enclosing loop's jump targets, mirroring skull-mir's `loop_targets`:
/// `Goto(cont)` -> `continue`, `Goto(brk)` -> `break`.
#[derive(Clone, Copy, Default)]
struct LoopCtx {
    cont: Option<Idx<Block>>,
    brk: Option<Idx<Block>>,
}

/// Emit `<ret> <name>(<params>) { <body> }` for one lowered INSTANCE.
fn emit_fn_def(
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &ShaderInstance,
    mf: &MirFunc,
) -> Result<String, CompilerError> {
    emit_fn_def_with(ws, interner, inst, mf, None)
}

/// [`emit_fn_def`], optionally in ENTRY mode: the uniform-struct and sampler
/// params are dropped from the GLSL signature (their reads render as bare
/// uniform globals) and the fn keeps its bare Rock name.
fn emit_fn_def_with(
    ws: &Workspace,
    interner: &MonoInterner,
    inst: &ShaderInstance,
    mf: &MirFunc,
    entry: Option<&EntryShape>,
) -> Result<String, CompilerError> {
    let fref = inst.fref();
    let subst = inst.subst(ws, interner);
    let f = &ws.module(fref.module).functions[fref.func];
    let emits_main = entry.is_some_and(|s| s.main_out.is_some());
    let name = if emits_main {
        "main".to_string()
    } else if entry.is_some() {
        f.name.as_str().to_string()
    } else {
        glsl_name(ws, interner, inst)
    };
    // A `void main()` entry destructures its returned struct into globals
    // (gl_Position + varyings, or MRT outs) -- no signature return type.
    let ret_ty = if emits_main {
        None
    } else {
        match f.return_ty {
            Some(rt) => Some(glsl_ty(ws, &subst, rt, f.name_span, "return")?),
            None => None,
        }
    };
    let ret = ret_ty.map_or("void", |t| t.name());

    // Resolve and VALIDATE every call site before anything renders, so the
    // emitter reads one answer rather than re-deriving it per use.
    let mut callee_insts: HashMap<Idx<Inst>, ShaderInstance> = HashMap::default();
    for (iid, inst) in mf.insts.iter() {
        if let Inst::Call { func, .. } = inst {
            callee_insts.insert(iid, callee_inst_at(ws, mf, iid, *func, f.name_span)?);
        }
    }

    let st = structure::analyze(mf);
    let mut counted = HashMap::default();
    let mut general = HashMap::default();
    for (&h, region) in &st.loops {
        // The range-`for` fast path is valid only when the header's own `Branch`
        // IS the loop test (its else is the real exit). A `while i < N && ..`
        // header also looks like `i < N` with an `i + 1` latch, but its exit lies
        // inside the loop -- that one takes the general path.
        match match_counted_loop(mf, h) {
            Some(li) if li.exit == region.exit && region.body.contains(&li.latch) => {
                counted.insert(h, li);
            }
            _ => {
                general.insert(h, region.clone());
            }
        }
    }

    let mut e = FnEmit {
        ws,
        interner,
        f: mf,
        ret_ty,
        cell_name: HashMap::default(),
        cell_ty: HashMap::default(),
        counted,
        general,
        structure: st,
        active: HashSet::default(),
        out: String::new(),
        indent: 1,
        visited: HashSet::default(),
        global_cells: entry
            .map(|s| s.global_params.iter().map(|&i| mf.params[i]).collect())
            .unwrap_or_default(),
        uniform_fields: entry.map(|s| s.uniform_fields.iter().copied().collect()).unwrap_or_default(),
        folded_cond: HashMap::default(),
        folded_block: HashMap::default(),
        folded_bound: HashMap::default(),
        main_out: entry.and_then(|s| s.main_out.clone()),
        fn_span: f.name_span,
        subst: subst.clone(),
        callee_insts,
    };
    e.assign_cell_names();
    // Global-base struct params + samplers + uniform arrays are dropped from the
    // GLSL signature.
    let dropped: HashSet<usize> = entry
        .map(|s| {
            s.global_params
                .iter()
                .copied()
                .chain(s.sampler_params.iter().copied())
                .chain(s.arrays.iter().copied())
                .collect()
        })
        .unwrap_or_default();
    // A global-base struct param is not a tier-0 GLSL leaf (glsl_ty would reject
    // it); its reads render as bare globals. Samplers DO map (`sampler2D`), so
    // they still get a cell type. Uniform-array params have no tier-0 type
    // either -- their only legal use is as `at()`'s first arg, which renders the
    // cell name (= the declared uniform array).
    let struct_params: HashSet<usize> = entry
        .map(|s| s.global_params.iter().copied().chain(s.arrays.iter().copied()).collect())
        .unwrap_or_default();
    for (i, &c) in mf.params.iter().enumerate() {
        if struct_params.contains(&i) {
            continue;
        }
        let ty = glsl_ty(ws, &subst, f.param_decls[i].ty, f.name_span, "parameter")?;
        e.cell_ty.insert(c, ty);
    }
    e.compute_cell_types();
    e.compute_folds();

    // Parameter list (entry mode drops uniform-struct + sampler params).
    let params: Vec<String> = mf
        .params
        .iter()
        .enumerate()
        .filter(|(i, _)| !dropped.contains(i))
        .map(|(_, &c)| {
            let ty = e.cell_ty[&c].name();
            format!("{ty} {}", e.cell_name[&c])
        })
        .collect();

    // Declare every non-param cell up front (hoisted -- avoids GLSL block-scope
    // leaks and lets a counter/accumulator span nested loops).
    let param_set: HashSet<Idx<Cell>> = mf.params.iter().copied().collect();
    let mut decls = String::new();
    for (cid, _) in mf.cells.iter() {
        if param_set.contains(&cid) {
            continue;
        }
        // A folded diamond result / bound temp has no storage -- it renders inline.
        if e.folded_cond.contains_key(&cid) || e.folded_bound.contains_key(&cid) {
            continue;
        }
        if let Some(ty) = e.cell_ty.get(&cid) {
            decls.push_str(&format!("  {} {};\n", ty.name(), e.cell_name[&cid]));
        }
    }

    e.emit_seq(mf.entry, None, LoopCtx::default())?;
    let body = std::mem::take(&mut e.out);

    Ok(format!(
        "{ret} {name}({}) {{\n{decls}{body}}}\n",
        params.join(", ")
    ))
}

impl FnEmit<'_> {
    fn push(&mut self, line: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    /// Assign a unique GLSL identifier to every cell (source name, deduped).
    fn assign_cell_names(&mut self) {
        let mut used: HashSet<String> = HashSet::default();
        for (cid, cell) in self.f.cells.iter() {
            let base = cell.name.map(|n| n.as_str().to_string()).unwrap_or_else(|| format!("t{}", cid.into_raw().into_u32()));
            let mut name = base.clone();
            let mut k = 0;
            while used.contains(&name) {
                k += 1;
                name = format!("{base}_{k}");
            }
            used.insert(name.clone());
            self.cell_name.insert(cid, name);
        }
    }

    /// Whether `iid` is a `Load` of an entry-mode global-base struct param cell
    /// (attribute/uniform/varyings) -- a field read off it renders as a bare
    /// global, not a struct member.
    fn is_uniform_base(&self, iid: Idx<Inst>) -> bool {
        matches!(self.f.insts[iid], Inst::Load(c) if self.global_cells.contains(&c))
    }

    /// Type every non-param cell: prefer bone's binding type ([`Cell::ty`]) where
    /// it maps to a tier-0 GLSL type (fixes `let y: float = 0`, whose first store's
    /// rep is int); fall back to first-store inference (RPO) for compiler-minted
    /// temps (if-result, loop counter) that carry no binding type.
    fn compute_cell_types(&mut self) {
        for (cid, cell) in self.f.cells.iter() {
            if self.cell_ty.contains_key(&cid) {
                continue;
            }
            if let Some(ty) = cell.ty {
                if let Ok(g) = glsl_ty(self.ws, &self.subst, ty, Span::default(), "local") {
                    self.cell_ty.insert(cid, g);
                }
            }
        }
        let rpo = reverse_postorder(self.f, self.f.entry);
        for &b in &rpo {
            for &iid in &self.f.blocks[b].insts {
                if let Inst::Store { cell, value } = self.f.insts[iid] {
                    if !self.cell_ty.contains_key(&cell) {
                        let ty = self.ty_of(value);
                        self.cell_ty.insert(cell, ty);
                    }
                }
            }
        }
    }

    /// Detect the cosmetic-tier folds (`docs/shaders.md`): `&&`/`||`
    /// short-circuit diamonds collapse back to infix boolean expressions.
    fn compute_folds(&mut self) {
        let reach: HashSet<Idx<Block>> =
            reverse_postorder(self.f, self.f.entry).into_iter().collect();
        // Cell load-counts over reachable code -- a fold is only sound when the
        // diamond's result cell has exactly one use.
        let mut load_count: HashMap<Idx<Cell>, usize> = HashMap::default();
        // Stores too: the fold renders the diamond at the cell's use, so the
        // diamond's two arms must be the cell's ONLY writers. A nested ladder
        // (`match n { 1 => .., 2 => .., _ => .. }`) folds its inner diamond while
        // an OUTER arm also stores the same single-use cell -- suppressing that
        // arm's store and silently dropping the case.
        let mut store_count: HashMap<Idx<Cell>, usize> = HashMap::default();
        for &b in &reach {
            for &iid in &self.f.blocks[b].insts {
                match self.f.insts[iid] {
                    Inst::Load(c) => *load_count.entry(c).or_default() += 1,
                    Inst::Store { cell, .. } => *store_count.entry(cell).or_default() += 1,
                    _ => {}
                }
            }
        }

        // Short-circuit diamonds: a `Branch` whose two arms each single-store the
        // same single-use bool cell, one arm storing the `&&`/`||` const identity.
        // `a && b` lowers to `if a then b else false`; `a || b` to
        // `if a then true else b` (`lower_and`/`lower_or`, no And/Or inst).
        for b in reach.iter().copied() {
            let Term::Branch { cond, then_bb, else_bb } = self.f.blocks[b].term else {
                continue;
            };
            let Some(join) = self.structure.ipdom(b) else { continue };
            let Some((tcell, tval)) = self.single_store_arm(then_bb, join) else { continue };
            let Some((ecell, eval)) = self.single_store_arm(else_bb, join) else { continue };
            if tcell != ecell {
                continue;
            }
            let rcell = tcell;
            if load_count.get(&rcell).copied().unwrap_or(0) != 1 {
                continue;
            }
            if store_count.get(&rcell).copied().unwrap_or(0) != 2 {
                continue;
            }
            let Some(&result_ty) = self.cell_ty.get(&rcell) else { continue };
            // Soundness: the fold moves the expression to the cell's use site, so
            // its inputs must be unchanged there. The arms go straight to `join`
            // and store only `rcell`, so the only place an input could be mutated
            // between the diamond and the use is `join` before the use -- require
            // the use to be in `join` with no store preceding it.
            if !self.use_hoistable_to(rcell, join) {
                continue;
            }
            let kind = if result_ty == GlslTy::Bool && self.is_const_bool(eval, false) {
                FoldKind::And
            } else if result_ty == GlslTy::Bool && self.is_const_bool(tval, true) {
                FoldKind::Or
            } else {
                FoldKind::Ternary
            };
            self.folded_cond.insert(
                rcell,
                FoldedDiamond { cond, then_val: tval, else_val: eval, kind, result_ty },
            );
            self.folded_block.insert(b, rcell);
        }

        // Bound temps (item 3): a counted loop's bound `t = <expr>; for (; i < t;
        // ..)` folds to `for (; i < <expr>; ..)`, dropping the single-use temp.
        // The bound temp is a range-for's `hi`, evaluated once in the preheader
        // (match_counted_loop only matches range-fors), so inlining is sound when
        // `<expr>` is loop-invariant -- a const, or pure int arithmetic over cells
        // that are stored exactly once (their store dominates the preheader, so
        // re-reading in the folded condition yields the same value). Folding it
        // is not just a cleanup: a GLES/GL driver miscompiles `t = radius + 1; for
        // (; y < t; ..)` for a nested runtime-bounded kernel (the outline_detect
        // classify loop found no neighbors), while the inlined `y < (radius + 1)`
        // is correct -- pinned by bitgun `outline_classify_sim`.
        let bounds: Vec<Idx<Inst>> = self.counted.values().map(|li| li.bound).collect();
        for bound in bounds {
            let Inst::Load(bcell) = self.f.insts[bound] else { continue };
            if load_count.get(&bcell).copied().unwrap_or(0) != 1 {
                continue;
            }
            let Some(v) = self.single_store_value(bcell) else { continue };
            if self.bound_value_is_invariant(v) {
                self.folded_bound.insert(bcell, v);
            }
        }
    }

    /// Whether a counted-loop bound value is loop-invariant enough to inline into
    /// the `for` header (see the fold in `precompute`). Conservative: a const, or
    /// pure int arithmetic / int<->float casts over cells that are stored exactly
    /// once in the fn (so re-reading in the folded condition can't change). The
    /// loop counter is multi-store, so it can never sneak in. Anything else (a
    /// call like `min(n, 16)`, a texture read) stays a temp -- the safe default.
    fn bound_value_is_invariant(&self, v: Idx<Inst>) -> bool {
        match self.f.insts[v] {
            Inst::Const(_) => true,
            Inst::Load(c) => !self.global_cells.contains(&c) && self.single_store_value(c).is_some(),
            Inst::IntAdd(a, b) | Inst::IntSub(a, b) | Inst::IntMul(a, b) => {
                self.bound_value_is_invariant(a) && self.bound_value_is_invariant(b)
            }
            Inst::IntToFloat(x) | Inst::FloatToInt(x) => self.bound_value_is_invariant(x),
            _ => false,
        }
    }

    /// The single value stored to `cell`, or `None` if it is stored zero or more
    /// than once anywhere in the fn.
    fn single_store_value(&self, cell: Idx<Cell>) -> Option<Idx<Inst>> {
        let mut found = None;
        for (_, blk) in self.f.blocks.iter() {
            for &iid in &blk.insts {
                if let Inst::Store { cell: c, value } = self.f.insts[iid] {
                    if c == cell {
                        if found.is_some() {
                            return None;
                        }
                        found = Some(value);
                    }
                }
            }
        }
        found
    }

    /// Whether `rcell`'s single use is in `join` with no `Store` preceding it
    /// there -- the condition that makes hoisting the folded expression to the
    /// use site sound (no input can be mutated between the diamond and the use).
    fn use_hoistable_to(&self, rcell: Idx<Cell>, join: Idx<Block>) -> bool {
        for &iid in &self.f.blocks[join].insts {
            match self.f.insts[iid] {
                Inst::Load(c) if c == rcell => return true,
                Inst::Store { .. } => return false,
                _ => {}
            }
        }
        false
    }

    /// If `arm` is a single-store block (exactly one `Store`, any number of pure
    /// value insts) ending `Goto(join)`, return its `(cell, stored value)`.
    fn single_store_arm(&self, arm: Idx<Block>, join: Idx<Block>) -> Option<(Idx<Cell>, Idx<Inst>)> {
        if self.f.blocks[arm].term != Term::Goto(join) {
            return None;
        }
        let mut store = None;
        for &iid in &self.f.blocks[arm].insts {
            if let Inst::Store { cell, value } = self.f.insts[iid] {
                if store.is_some() {
                    return None;
                }
                store = Some((cell, value));
            }
        }
        store
    }

    fn is_const_bool(&self, iid: Idx<Inst>, want: bool) -> bool {
        matches!(self.f.insts[iid], Inst::Const(Lit::Bool(b)) if b == want)
    }

    /// Render a folded diamond as one GLSL expression.
    fn render_folded(&self, fd: &FoldedDiamond) -> Result<String, CompilerError> {
        let cond = self.render_val(fd.cond)?;
        Ok(match fd.kind {
            FoldKind::And => format!("({} && {})", cond, self.render_val(fd.then_val)?),
            FoldKind::Or => format!("({} || {})", cond, self.render_val(fd.else_val)?),
            FoldKind::Ternary => format!(
                "({} ? {} : {})",
                cond,
                self.render_val_t(fd.then_val, fd.result_ty)?,
                self.render_val_t(fd.else_val, fd.result_ty)?
            ),
        })
    }

    /// The GLSL type of a value instruction.
    fn ty_of(&self, iid: Idx<Inst>) -> GlslTy {
        match &self.f.insts[iid] {
            Inst::Const(Lit::Float(_)) => GlslTy::Float,
            Inst::Const(Lit::Int(_)) => GlslTy::Int,
            Inst::Const(Lit::Bool(_)) => GlslTy::Bool,
            Inst::Const(Lit::Nil) | Inst::Const(Lit::Str(_)) => GlslTy::Float, // rejected earlier
            Inst::FloatAdd(..) | Inst::FloatSub(..) | Inst::FloatMul(..) | Inst::FloatDiv(..) | Inst::FloatMod(..) => GlslTy::Float,
            Inst::IntAdd(..) | Inst::IntSub(..) | Inst::IntMul(..) | Inst::IntDiv(..) | Inst::IntMod(..) => GlslTy::Int,
            Inst::IntLt(..) | Inst::IntLeq(..) | Inst::IntGt(..) | Inst::IntGeq(..)
            | Inst::FloatLt(..) | Inst::FloatLeq(..) | Inst::FloatGt(..) | Inst::FloatGeq(..)
            | Inst::Eq(..) | Inst::NotEq(..) | Inst::Not(_) => GlslTy::Bool,
            Inst::Neg(x) => self.ty_of(*x),
            Inst::FloatToInt(_) => GlslTy::Int,
            Inst::IntToFloat(_) => GlslTy::Float,
            Inst::Load(c) => self.cell_ty.get(c).copied().unwrap_or(GlslTy::Float),
            Inst::Global(name) => {
                const_lit(self.ws, *name).as_ref().map_or(GlslTy::Float, const_lit_ty)
            }
            Inst::Field { base, field } => {
                if self.is_uniform_base(*base) {
                    self.uniform_fields.get(field).copied().unwrap_or(GlslTy::Float)
                } else if self.ty_of(*base) == GlslTy::Bvec3 {
                    GlslTy::Bool // bvec3 component read
                } else {
                    GlslTy::Float // Vec component
                }
            }
            Inst::MakeValueStruct { name, .. } | Inst::MaterializeValueStruct { name, .. } => {
                match name.as_str() {
                "Vec2" => GlslTy::Vec2,
                "Vec3" => GlslTy::Vec3,
                "Vec4" => GlslTy::Vec4,
                "Bvec3" => GlslTy::Bvec3,
                "Mat4" => GlslTy::Mat4,
                _ => GlslTy::Float,
                }
            }
            Inst::Call { func, args } => {
                let cf = &self.ws.module(func.module).functions[func.func];
                let csub = self.callee_inst(iid).subst(self.ws, self.interner);
                match cf.return_ty.and_then(|rt| glsl_ty(self.ws, &csub, rt, cf.name_span, "return").ok()) {
                    Some(t) => t,
                    // Generic-return genType builtins (min<T>/max<T>/clamp/abs):
                    // GLSL's result type is the argument type. Load-bearing under
                    // ES300, which has no implicit int->float on assignment.
                    None if matches!(cf.name.as_str(), "min" | "max" | "clamp" | "abs")
                        && !args.is_empty() =>
                    {
                        self.ty_of(args[0])
                    }
                    None => GlslTy::Float,
                }
            }
            _ => GlslTy::Float,
        }
    }

    /// Render at a float context slot. int->float subsumption is materialized at
    /// lowering (`Inst::IntToFloat`, which `render_val` folds/wraps), so an
    /// int-typed value reaching here that is NOT already coerced is a MISSED
    /// lowering sink -- a hard emit error, never silent driver-dependent GLSL.
    /// (GLSL ES has no implicit int->float; desktop drivers vary on the corner.)
    fn render_val_t(&self, iid: Idx<Inst>, want: GlslTy) -> Result<String, CompilerError> {
        if want == GlslTy::Float && self.ty_of(iid) == GlslTy::Int {
            return Err(err(
                self.span(),
                format!(
                    "shader profile: int-typed value ({}) in a float context without an IntToFloat coercion -- a missed lowering sink (docs/shaders.md)",
                    inst_kind(&self.f.insts[iid])
                ),
            ));
        }
        self.render_val(iid)
    }

    /// Render a value instruction as a GLSL expression (inlined; MIR values are
    /// single-use, so no CSE duplication).
    fn render_val(&self, iid: Idx<Inst>) -> Result<String, CompilerError> {
        let bin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!("({} {} {})", s.render_val(*a)?, op, s.render_val(*b)?))
        };
        let fbin = |a: &Idx<Inst>, op: &str, b: &Idx<Inst>, s: &Self| -> Result<String, CompilerError> {
            Ok(format!(
                "({} {} {})",
                s.render_val_t(*a, GlslTy::Float)?,
                op,
                s.render_val_t(*b, GlslTy::Float)?
            ))
        };
        Ok(match &self.f.insts[iid] {
            Inst::Const(Lit::Float(f)) => fmt_float(*f),
            Inst::Const(Lit::Int(i)) => i.to_string(),
            Inst::Const(Lit::Bool(b)) => b.to_string(),
            Inst::Const(Lit::Nil) => return Err(err(self.span(), "shader profile: null literal is not tier 0")),
            Inst::Const(Lit::Str(_)) => return Err(err(self.span(), "shader profile: string literal is not tier 0")),
            // Admitted by `profile_body` only when it folds, so a miss here is a
            // profile/emit divergence, not user error.
            Inst::Global(name) => const_lit(self.ws, *name)
                .as_ref()
                .and_then(render_const_lit)
                .ok_or_else(|| err(self.span(), format!(
                    "shader profile: const `{name}` passed the profile pass but did not fold at emit -- a compiler bug"
                )))?,
            Inst::Load(c) => {
                // A global-base struct param has no GLSL identity -- its fields
                // are bare globals, read via `Field` (which never renders the
                // base). A `Load` reaching expression position is a copy/alias
                // (`let v = vary`) that would reference the dropped param name.
                if self.global_cells.contains(c) {
                    return Err(err(self.span(), format!(
                        "shader profile: the struct param `{}` can only be read field-by-field (its fields are GLSL globals) -- it cannot be copied or aliased",
                        self.cell_name[c]
                    )));
                }
                if let Some(fd) = self.folded_cond.get(c) {
                    self.render_folded(fd)?
                } else if let Some(&v) = self.folded_bound.get(c) {
                    self.render_val(v)?
                } else {
                    self.cell_name[c].clone()
                }
            }
            Inst::IntAdd(a, b) => bin(a, "+", b, self)?,
            Inst::FloatAdd(a, b) => fbin(a, "+", b, self)?,
            Inst::IntSub(a, b) => bin(a, "-", b, self)?,
            Inst::FloatSub(a, b) => fbin(a, "-", b, self)?,
            Inst::IntMul(a, b) => bin(a, "*", b, self)?,
            Inst::FloatMul(a, b) => fbin(a, "*", b, self)?,
            Inst::FloatDiv(a, b) => fbin(a, "/", b, self)?,
            Inst::FloatMod(a, b) => format!(
                "mod({}, {})",
                self.render_val_t(*a, GlslTy::Float)?,
                self.render_val_t(*b, GlslTy::Float)?
            ),
            // GLSL int / and % are undefined on negatives -> floored helpers.
            Inst::IntDiv(a, b) => format!("__rock_idiv({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntMod(a, b) => format!("__rock_imod({}, {})", self.render_val(*a)?, self.render_val(*b)?),
            Inst::IntLt(a, b) => bin(a, "<", b, self)?,
            Inst::FloatLt(a, b) => fbin(a, "<", b, self)?,
            Inst::IntLeq(a, b) => bin(a, "<=", b, self)?,
            Inst::FloatLeq(a, b) => fbin(a, "<=", b, self)?,
            Inst::IntGt(a, b) => bin(a, ">", b, self)?,
            Inst::FloatGt(a, b) => fbin(a, ">", b, self)?,
            Inst::IntGeq(a, b) => bin(a, ">=", b, self)?,
            Inst::FloatGeq(a, b) => fbin(a, ">=", b, self)?,
            // Untyped Eq/NotEq: operands arrive same-typed (a mixed `float == int`
            // was int->float-coerced at lowering), so render a plain compare.
            Inst::Eq(a, b) => bin(a, "==", b, self)?,
            Inst::NotEq(a, b) => bin(a, "!=", b, self)?,
            Inst::Neg(x) => format!("(-{})", self.render_val(*x)?),
            Inst::Not(x) => format!("(!{})", self.render_val(*x)?),
            Inst::FloatToInt(x) => format!("int({})", self.render_val(*x)?),
            // int->float subsumption (inserted at lowering): fold an int literal
            // to float text, else materialize the `float(...)` conversion.
            Inst::IntToFloat(x) => match &self.f.insts[*x] {
                Inst::Const(Lit::Int(i)) => fmt_float(*i as f64),
                _ => format!("float({})", self.render_val(*x)?),
            },
            Inst::Field { base, field } => {
                if self.is_uniform_base(*base) {
                    field.as_str().to_string() // uniform global, not a struct member
                } else {
                    format!("{}.{}", self.render_val(*base)?, field)
                }
            }
            // `MaterializeValueStruct` renders identically: on the GPU an
            // authorized box is just a `vec2(..)` ctor -- the authorization is a
            // Lua-side concept (`authorize_gpu_boundary_boxes`).
            Inst::MakeValueStruct { name, fields } | Inst::MaterializeValueStruct { name, fields } => {
                // GLSL must never see the post-normalization LEAF form (dotted
                // field names, `flatten_nobox_nested_ctors`). It cannot today --
                // this crate never calls `skull_transform`, so it never runs
                // `cleanup` -- and it must not start: GLSL has real nested ctors
                // where the nested form is correct and free, so flattening would
                // be actively wrong here. Fail loud if a pipeline change ever
                // routes normalized MIR to this emitter.
                debug_assert!(
                    !fields.iter().any(|(f, _)| f.contains('.')),
                    "GLSL received the nobox leaf form for `{name}`: this emitter \
                     must not run on `cleanup`-normalized MIR"
                );
                // Bvec3 components render at a bool context; the vecN ctors take
                // float components (int operands materialize an IntToFloat).
                let (ctor, comp_ty) = match name.as_str() {
                    "Vec2" => ("vec2", GlslTy::Float),
                    "Vec3" => ("vec3", GlslTy::Float),
                    "Vec4" => ("vec4", GlslTy::Float),
                    "Bvec3" => ("bvec3", GlslTy::Bool),
                    other => return Err(err(self.span(), format!("shader profile: value struct `{other}` is not tier 0"))),
                };
                let args: Result<Vec<String>, CompilerError> =
                    fields.iter().map(|(_, v)| self.render_val_t(*v, comp_ty)).collect();
                format!("{ctor}({})", args?.join(", "))
            }
            Inst::Call { func, args } => self.render_call(iid, *func, args)?,
            other => return Err(err(self.span(), format!("shader profile: cannot render {} as a GLSL expression", inst_kind(other)))),
        })
    }

    /// Render a direct call. `iid` names the call SITE, which is where the
    /// callee's instantiation is recorded -- both the emitted name and every
    /// declared param/return type resolve through it, so a specialized callee
    /// and its definition cannot disagree.
    fn render_call(
        &self,
        iid: Idx<Inst>,
        func: FuncRef,
        args: &[Idx<Inst>],
    ) -> Result<String, CompilerError> {
        let cf = &self.ws.module(func.module).functions[func.func];
        let callee = self.callee_inst(iid);
        let csub = callee.subst(self.ws, self.interner);
        let name = cf.name.as_str();
        // A non-scalar `__op_*` call is a Vec operator -> native infix. An int
        // operand (`*(Vec2, int)`) is cast to float (GLSL has no vec*int).
        if let Some(sym) = vec_op_symbol(name) {
            if args.len() == 2 {
                let render_operand = |i: usize, e: &Self| -> Result<String, CompilerError> {
                    let pt = glsl_ty(e.ws, &csub, cf.param_decls[i].ty, cf.name_span, "operand")?;
                    let s = e.render_val(args[i])?;
                    Ok(if pt == GlslTy::Int { format!("float({s})") } else { s })
                };
                return Ok(format!("({} {} {})", render_operand(0, self)?, sym, render_operand(1, self)?));
            }
        }
        // Args render against the declared param type where it maps to GLSL
        // (a generic param, e.g. `min<T>`, doesn't -- those render plain).
        let arg_t = |i: usize, a: Idx<Inst>, e: &Self| -> Result<String, CompilerError> {
            match cf.param_decls.get(i).and_then(|p| glsl_ty(e.ws, &csub, p.ty, cf.name_span, "arg").ok()) {
                Some(want) => e.render_val_t(a, want),
                None => e.render_val(a),
            }
        };
        // A `v(...)` ctor call with a vector param (the mixed ctors in
        // stdlib/vec.rock, `v(rgb, 1.0)`) must not hit the arity-keyed scalar
        // rows (`("v", 2)` -> vec2) -- the (name, arity) same-arity type-drift
        // hazard. GLSL constructors take mixed components natively, so emit
        // vecN(...) with N = the summed component count.
        if name == "v"
            && cf.param_decls.iter().any(|p| {
                matches!(
                    glsl_ty(self.ws, &csub, p.ty, cf.name_span, "ctor param").ok(),
                    Some(GlslTy::Vec2 | GlslTy::Vec3 | GlslTy::Vec4)
                )
            })
        {
            let n: usize = cf
                .param_decls
                .iter()
                .map(|p| match glsl_ty(self.ws, &csub, p.ty, cf.name_span, "ctor param").ok() {
                    Some(GlslTy::Vec2) => 2,
                    Some(GlslTy::Vec3) => 3,
                    Some(GlslTy::Vec4) => 4,
                    _ => 1,
                })
                .sum();
            let ctor = match n {
                2 => "vec2",
                3 => "vec3",
                4 => "vec4",
                _ => {
                    return Err(err(
                        self.span(),
                        format!("shader profile: v(...) ctor with {n} total components"),
                    ))
                }
            };
            let rargs: Result<Vec<String>, CompilerError> =
                args.iter().enumerate().map(|(i, a)| arg_t(i, *a, self)).collect();
            return Ok(format!("{ctor}({})", rargs?.join(", ")));
        }
        if let Some(Emission::Template(tmpl)) = GlslIntrinsics.lookup(cf) {
            let mut s = tmpl.to_string();
            for (i, a) in args.iter().enumerate() {
                s = s.replace(&format!("${}", i + 1), &arg_t(i, *a, self)?);
            }
            return Ok(s);
        }
        // An emittable user fn -> a plain call to THIS INSTANCE's GLSL name.
        let gname = glsl_name(self.ws, self.interner, callee);
        let rargs: Result<Vec<String>, CompilerError> =
            args.iter().enumerate().map(|(i, a)| arg_t(i, *a, self)).collect();
        Ok(format!("{gname}({})", rargs?.join(", ")))
    }

    fn span(&self) -> Span {
        self.fn_span
    }

    /// Emit a structured sequence of blocks starting at `start`, stopping before
    /// `stop` (its continuation). Handles straight-line code, if-diamonds,
    /// counted `for` loops, and general `while` loops; `ctx` carries the
    /// enclosing loop's break/continue targets.
    fn emit_seq(&mut self, start: Idx<Block>, stop: Option<Idx<Block>>, ctx: LoopCtx) -> Result<(), CompilerError> {
        let mut cur = start;
        loop {
            if Some(cur) == stop {
                return Ok(());
            }
            // Loop headers are dispatched structurally (before the visited guard):
            // `emit_general` re-enters `emit_seq` on the header for its condition
            // region, so the header must not already be marked visited.
            if !self.active.contains(&cur) {
                if let Some(li) = self.counted.get(&cur).copied() {
                    self.emit_counted(li)?;
                    cur = li.exit;
                    continue;
                }
                if let Some(region) = self.general.get(&cur).cloned() {
                    self.emit_general(&region, ctx)?;
                    cur = region.exit;
                    continue;
                }
            }
            if !self.visited.insert(cur) {
                return Err(err(self.span(), "shader profile: unstructured control flow (unsupported loop/branch shape)"));
            }
            self.emit_block_effects(cur)?;
            match self.f.blocks[cur].term.clone() {
                Term::Return(vs) => {
                    self.emit_return(&vs)?;
                    return Ok(());
                }
                Term::Unreachable => return Ok(()),
                Term::Goto(t) => {
                    if Some(t) == ctx.cont {
                        self.push("continue;");
                        return Ok(());
                    }
                    if Some(t) == ctx.brk {
                        self.push("break;");
                        return Ok(());
                    }
                    if Some(t) == stop {
                        return Ok(());
                    }
                    cur = t;
                }
                Term::Branch { cond, then_bb, else_bb } => {
                    // A folded short-circuit / ternary diamond emits nothing here --
                    // its expression renders at the result cell's one use. Skip to
                    // the join (ipdom); both arms are absorbed.
                    if self.folded_block.contains_key(&cur) {
                        match self.structure.ipdom(cur) {
                            Some(join) => cur = join,
                            None => return Ok(()),
                        }
                    } else {
                        match self.emit_branch(cur, cond, then_bb, else_bb, stop, ctx)? {
                            Some(m) => cur = m,
                            None => return Ok(()),
                        }
                    }
                }
                Term::Switch { .. } => {
                    return Err(err(self.span(), "shader profile: `match` is not tier 0"));
                }
                Term::Yield { .. } => {
                    return Err(err(self.span(), "shader profile: `yield` (coroutine) is not tier 0"));
                }
            }
        }
    }

    /// Whether `b` produces no statement and just forwards to another block
    /// (`Some(target)`): the empty else/then arm of a statement `if`, or a bare
    /// `break`/`continue` block.
    fn empty_forward(&self, b: Idx<Block>) -> Option<Idx<Block>> {
        if let Term::Goto(t) = self.f.blocks[b].term {
            let has_store = self.f.blocks[b]
                .insts
                .iter()
                .any(|&i| matches!(self.f.insts[i], Inst::Store { .. }));
            if !has_store {
                return Some(t);
            }
        }
        None
    }

    /// Emit an `if`-diamond for a `Branch`. Returns the block to continue at
    /// (the merge / join), or `None` when both arms diverge and the arms have
    /// absorbed the continuation.
    fn emit_branch(
        &mut self,
        blk: Idx<Block>,
        cond: Idx<Inst>,
        then_bb: Idx<Block>,
        else_bb: Idx<Block>,
        stop: Option<Idx<Block>>,
        ctx: LoopCtx,
    ) -> Result<Option<Idx<Block>>, CompilerError> {
        let cond_s = self.render_val(cond)?;
        // Empty else -> then-only `if`; continue at the else target.
        if let Some(j) = self.empty_forward(else_bb) {
            self.push(&format!("if ({cond_s}) {{"));
            self.indent += 1;
            self.emit_seq(then_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Empty then -> invert and emit the else arm only.
        if let Some(j) = self.empty_forward(then_bb) {
            self.push(&format!("if (!({cond_s})) {{"));
            self.indent += 1;
            self.emit_seq(else_bb, Some(j), ctx)?;
            self.indent -= 1;
            self.push("}");
            return Ok(Some(j));
        }
        // Full diamond. The merge is ipdom, unless an arm diverges (ipdom
        // collapses to the virtual exit or a loop target) -- then the arms absorb
        // the continuation up to the enclosing stop and there is no fall-through.
        let merge = self
            .structure
            .ipdom(blk)
            .filter(|&m| Some(m) != ctx.cont && Some(m) != ctx.brk);
        let arm_stop = merge.or(stop);
        self.push(&format!("if ({cond_s}) {{"));
        self.indent += 1;
        self.emit_seq(then_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("} else {");
        self.indent += 1;
        self.emit_seq(else_bb, arm_stop, ctx)?;
        self.indent -= 1;
        self.push("}");
        Ok(merge)
    }

    /// Emit the straight-line effect statements of a block: Stores (assignments)
    /// and void (effect-only) calls like `discard()`. Value-returning insts are
    /// inlined at their use, so they produce no statement here.
    fn emit_block_effects(&mut self, b: Idx<Block>) -> Result<(), CompilerError> {
        for iid in self.f.blocks[b].insts.clone() {
            let line: Option<String> = match &self.f.insts[iid] {
                Inst::Store { cell, value } => {
                    let (cell, value) = (*cell, *value);
                    // Folded diamond result / bound temp: no storage, inlined.
                    if self.folded_cond.contains_key(&cell) || self.folded_bound.contains_key(&cell) {
                        None
                    } else {
                        let rhs = match self.cell_ty.get(&cell) {
                            Some(&want) => self.render_val_t(value, want)?,
                            None => self.render_val(value)?,
                        };
                        Some(format!("{} = {};", self.cell_name[&cell], rhs))
                    }
                }
                // A void call in statement position (discard()): its result is
                // never consumed, so it never gets inlined at a use -- emit it here.
                Inst::Call { func, args } => {
                    let cf = &self.ws.module(func.module).functions[func.func];
                    if cf.return_ty.is_none() {
                        Some(format!("{};", self.render_call(iid, *func, args)?))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(line) = line {
                self.push(&line);
            }
        }
        Ok(())
    }

    fn emit_return(&mut self, vs: &[Idx<Inst>]) -> Result<(), CompilerError> {
        // `void main()` mode: `return <Struct { field, ... }>` destructures into a
        // named global assignment per field -- vertex varyings (a `position` field
        // -> gl_Position) or MRT fragment outputs.
        if let Some(mo) = self.main_out.clone() {
            let [v] = vs else {
                return Err(err(self.span(), "shader entry: a void-main entry must return its output struct"));
            };
            let fields: Vec<(Ustr, Idx<Inst>)> = match &self.f.insts[*v] {
                Inst::MakeValueStruct { fields, .. }
                | Inst::MaterializeValueStruct { fields, .. } => {
                    fields.iter().map(|&(n, x)| (n, x)).collect()
                }
                _ => {
                    return Err(err(
                        self.span(),
                        "shader entry: a void-main entry must return an output struct literal",
                    ))
                }
            };
            for (name, val) in fields {
                let (target, want) = if mo.position_is_gl && name.as_str() == "position" {
                    ("gl_Position".to_string(), GlslTy::Vec4)
                } else {
                    // bone typechecks the returned struct against the varyings/MRT
                    // struct, so every field is in the map. Reject rather than
                    // defaulting: a classify/emit drift would otherwise silently
                    // emit a Float-typed assignment for a vec/int output.
                    let ty = mo.fields.get(&name).copied().ok_or_else(|| {
                        err(
                            self.span(),
                            format!(
                                "shader entry: returned field `{name}` is not an output of this entry's \
                                 varyings/MRT struct"
                            ),
                        )
                    })?;
                    (name.to_string(), ty)
                };
                let s = self.render_val_t(val, want)?;
                self.push(&format!("{target} = {s};"));
            }
            return Ok(());
        }
        match vs {
            [] => self.push("return;"),
            [v] => {
                let s = match self.ret_ty {
                    Some(want) => self.render_val_t(*v, want)?,
                    None => self.render_val(*v)?,
                };
                self.push(&format!("return {s};"));
            }
            _ => return Err(err(self.span(), "shader profile: multi-value return is not tier 0")),
        }
        Ok(())
    }

    fn emit_counted(&mut self, li: LoopInfo) -> Result<(), CompilerError> {
        let slot = self.cell_name[&li.slot].clone();
        let bound = self.render_val(li.bound)?;
        let cmp = if li.inclusive { "<=" } else { "<" };
        self.push(&format!("for (; {slot} {cmp} {bound}; {slot} = {slot} + 1) {{"));
        self.indent += 1;
        let body_len = self.out.len();
        let ctx = LoopCtx { cont: Some(li.latch), brk: Some(li.exit) };
        self.emit_seq(li.body_entry, None, ctx)?;
        self.trim_trailing_continue(body_len);
        self.indent -= 1;
        self.push("}");
        Ok(())
    }

    /// A general `while`: `while (true) { <cond region>; if (!(cond)) break; <body> }`.
    /// The condition region may be a multi-block diamond (`while a && b`), so it
    /// goes through the structurizer recursively.
    fn emit_general(&mut self, region: &LoopRegion, outer: LoopCtx) -> Result<(), CompilerError> {
        let h = region.header;
        let exit = region.exit;
        let (test, cond, body_start, break_when_true) = self.find_test(region)?;

        self.active.insert(h);
        self.push("while (true) {");
        self.indent += 1;

        self.emit_seq(h, Some(test), outer)?;
        self.emit_block_effects(test)?;
        self.visited.insert(test);

        let cond_s = self.render_val(cond)?;
        if break_when_true {
            self.push(&format!("if ({cond_s}) {{ break; }}"));
        } else {
            self.push(&format!("if (!({cond_s})) {{ break; }}"));
        }

        let ctx = LoopCtx { cont: Some(h), brk: Some(exit) };
        let body_len = self.out.len();
        self.emit_seq(body_start, None, ctx)?;
        self.trim_trailing_continue(body_len);

        self.indent -= 1;
        self.push("}");
        self.active.remove(&h);
        Ok(())
    }

    /// The loop's condition test: the unique `Branch` in the body with a direct
    /// successor to the exit. Returns `(test block, cond, body entry, break-when-cond-true)`.
    fn find_test(
        &self,
        region: &LoopRegion,
    ) -> Result<(Idx<Block>, Idx<Inst>, Idx<Block>, bool), CompilerError> {
        for &b in &region.body {
            if let Term::Branch { cond, then_bb, else_bb } = self.f.blocks[b].term {
                if then_bb == region.exit {
                    return Ok((b, cond, else_bb, true));
                }
                if else_bb == region.exit {
                    return Ok((b, cond, then_bb, false));
                }
            }
        }
        Err(err(self.span(), "shader profile: unstructured loop (no single condition test)"))
    }

    /// Drop a redundant trailing `continue;` that is the loop body's natural
    /// back-edge fall-through (indented exactly at the body level).
    fn trim_trailing_continue(&mut self, start: usize) {
        let line = format!("{}continue;\n", "  ".repeat(self.indent));
        if self.out.len() >= start + line.len() && self.out.ends_with(&line) {
            let cut = self.out.len() - line.len();
            if cut >= start && (cut == 0 || self.out.as_bytes()[cut - 1] == b'\n') {
                self.out.truncate(cut);
            }
        }
    }
}

// --- counted-loop recognition (mirrors skull-codegen-lua's, for the range-for
// shape lower_for_range emits) ------------------------------------------------

/// `Some(LoopInfo)` iff `h` is a `for i in lo..hi` header: a `Branch` on
/// `IntLt/IntLeq(load slot, bound)`, entered by exactly a preheader + a
/// pure-increment latch (`load slot; 1; add; store slot; goto h`). The `slot`
/// cell must be `loop_binding` -- desugar provenance that only `lower_for_range`
/// sets, so a `while i < n { ..; i = i + 1 }` (manual counter) is NOT mistaken
/// for a range-for and takes the general path instead.
fn match_counted_loop(f: &MirFunc, h: Idx<Block>) -> Option<LoopInfo> {
    let Term::Branch { cond, then_bb, else_bb } = f.blocks[h].term else {
        return None;
    };
    let (slot, bound, inclusive) = match &f.insts[cond] {
        Inst::IntLt(a, b) => (load_cell(f, *a)?, *b, false),
        Inst::IntLeq(a, b) => (load_cell(f, *a)?, *b, true),
        _ => return None,
    };
    if !f.cells[slot].loop_binding {
        return None;
    }
    // Find the pure-increment latch among h's goto-preds.
    let mut latch = None;
    for (bid, blk) in f.blocks.iter() {
        if blk.term == Term::Goto(h) && is_incr_latch(f, bid, slot) {
            latch = Some(bid);
        }
    }
    let latch = latch?;
    Some(LoopInfo {
        slot,
        bound,
        inclusive,
        body_entry: then_bb,
        latch,
        exit: else_bb,
    })
}

fn load_cell(f: &MirFunc, iid: Idx<Inst>) -> Option<Idx<Cell>> {
    match f.insts[iid] {
        Inst::Load(c) => Some(c),
        _ => None,
    }
}

/// `p`: `vj = load slot; one = 1; vinc = vj + one; store slot = vinc; goto h`.
fn is_incr_latch(f: &MirFunc, p: Idx<Block>, slot: Idx<Cell>) -> bool {
    let ins = &f.blocks[p].insts;
    // The store to slot must be an increment of slot by 1.
    for &iid in ins {
        if let Inst::Store { cell, value } = f.insts[iid] {
            if cell == slot {
                if let Inst::IntAdd(a, b) = f.insts[value] {
                    let is_one = |x: Idx<Inst>| matches!(f.insts[x], Inst::Const(Lit::Int(1)));
                    let loads_slot = |x: Idx<Inst>| matches!(f.insts[x], Inst::Load(c) if c == slot);
                    return (loads_slot(a) && is_one(b)) || (loads_slot(b) && is_one(a));
                }
                return false;
            }
        }
    }
    false
}

// --- public API --------------------------------------------------------------

/// Resolve a root fn name to a unique [`FuncRef`], preferring the user module.
pub fn resolve_root(ws: &Workspace, user_mid: ModuleId, name: &str) -> Result<FuncRef, CompilerError> {
    let key = Ustr::from(name);
    let Some(list): Option<&FnList> = ws.functions_by_name.get(&key) else {
        return Err(CompilerError::new_no_location(format!("shader root `{name}` not found")));
    };
    let in_user: Vec<_> = list.iter().filter(|(m, _)| *m == user_mid).collect();
    let pick = if in_user.len() == 1 {
        *in_user[0]
    } else if list.len() == 1 {
        list[0]
    } else {
        return Err(CompilerError::new_no_location(format!(
            "shader root `{name}` is overloaded/ambiguous ({} definitions); route B roots must be uniquely named",
            list.len()
        )));
    };
    Ok(FuncRef { module: pick.0, func: pick.1 })
}

/// The GLSL helper functions for tier-0 semantics GLSL's builtins get wrong:
/// floored int division/modulo (GLSL's are undefined on negative operands).
/// Emitted only when reachable code uses `IntDiv`/`IntMod`.
fn helper_prelude(uses_idiv: bool, uses_imod: bool) -> String {
    let mut s = String::new();
    if uses_idiv {
        s.push_str(
            "int __rock_idiv(int a, int b) {\n  return int(floor(float(a) / float(b)));\n}\n",
        );
    }
    if uses_imod {
        s.push_str(
            "int __rock_imod(int a, int b) {\n  return a - b * int(floor(float(a) / float(b)));\n}\n",
        );
    }
    s
}

fn library_root_instance(
    ws: &Workspace,
    graph: &InstanceGraph,
    root: FuncRef,
) -> Result<Instance, CompilerError> {
    let canonical = skull_mir::lower::canonical_variant(ws, root);
    graph
        .root_instances
        .iter()
        .find(|instance| instance.func == canonical && instance.args.is_empty())
        .cloned()
        .ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "shader library graph has no explicit root instance for `{}`",
                ws.module(root.module).functions[root.func].name
            ))
        })
}

fn visit_shader_library(
    ws: &Workspace,
    graph: &InstanceGraph,
    instance: Instance,
    retained: &mut BTreeSet<Instance>,
    builtins: &mut BTreeSet<Instance>,
    active: &mut BTreeSet<Instance>,
    done: &mut BTreeSet<Instance>,
    order: &mut Vec<Instance>,
) -> Result<(), CompilerError> {
    if active.contains(&instance) {
        let f = &ws.module(instance.func.module).functions[instance.func.func];
        return Err(err(
            f.name_span,
            format!("shader profile: recursion through `{}` is not allowed on GLSL", f.name),
        ));
    }
    if !done.insert(instance.clone()) {
        return Ok(());
    }
    if graph.concreteness(&instance) == skull_graph::instance_graph::Concreteness::Missing {
        return Err(CompilerError::new_no_location(format!(
            "shader library graph has no substitution for `{}`",
            graph.symbol(ws, &instance)
        )));
    }
    active.insert(instance.clone());
    retained.insert(instance.clone());
    if let Some(sites) = graph.sites_of(&instance) {
        for record in sites.values() {
            let callee = match &record.outcome {
                SiteOutcome::Resolved(callee) => callee.clone(),
                SiteOutcome::Failed(reason) => {
                    return Err(CompilerError::new_with_label(
                        record.span,
                        format!("shader library graph could not resolve a call: {reason}"),
                    ));
                }
            };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if graph.concreteness(&callee) == skull_graph::instance_graph::Concreteness::Missing {
                return Err(CompilerError::new_with_label(
                    record.span,
                    format!("shader library graph has no substitution for `{}`", f.name),
                ));
            }
            if renders_inline_instance(ws, f, &callee, graph.interner()) {
                builtins.insert(callee);
            } else if is_lowered_native_operator(ws, f, &callee, graph.interner()) {
                continue;
            } else if f.is_extern || f.inline_body.is_some() {
                // The supplied population stops at an opaque boundary. The
                // emitter still sees the call while profiling the caller and
                // reports the boundary after any earlier body-level rejection.
                continue;
            } else {
                visit_shader_library(
                    ws,
                    graph,
                    callee,
                    retained,
                    builtins,
                    active,
                    done,
                    order,
                )?;
            }
        }
    }
    active.remove(&instance);
    order.push(instance);
    Ok(())
}

/// Realize an explicit-root function-library population from the shared graph.
/// This is deliberately separate from registered shader programs: the roots
/// have no stage or interface contract, only concrete definitions and helpers.
pub fn realize_shader_library_view(
    ws: &Workspace,
    graph: &InstanceGraph,
    roots: &[FuncRef],
) -> Result<ShaderLibraryView, CompilerError> {
    let mut root_instances = Vec::new();
    let mut seen_roots = BTreeSet::new();
    for &root in roots {
        let instance = library_root_instance(ws, graph, root)?;
        if seen_roots.insert(instance.clone()) {
            root_instances.push(instance);
        }
    }
    let mut retained = BTreeSet::new();
    let mut builtins = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    let mut order = Vec::new();
    for root in &root_instances {
        visit_shader_library(
            ws,
            graph,
            root.clone(),
            &mut retained,
            &mut builtins,
            &mut active,
            &mut done,
            &mut order,
        )?;
    }
    let mut dependencies = Vec::new();
    let mut seen_dependencies = BTreeSet::new();
    for instance in order.iter().chain(root_instances.iter()) {
        if seen_dependencies.insert((instance.func.module.raw(), instance.func.func.into_raw().into_u32())) {
            dependencies.push(instance.func);
        }
    }
    let concrete_plans = skull_graph::concrete_body::build(ws, graph).plans;
    Ok(ShaderLibraryView {
        roots: root_instances,
        retained,
        builtins,
        order,
        dependencies,
        concrete_plans,
    })
}

/// The graph, lowering context and explicit-root population kept together so
/// every route that emits a function library reads one interner.
pub struct ShaderLibrarySession {
    pub graph: InstanceGraph,
    pub scx: ScalarCtx,
    pub view: ShaderLibraryView,
}

pub fn shader_library_session(
    ws: &Workspace,
    roots: Vec<FuncRef>,
) -> Result<ShaderLibrarySession, CompilerError> {
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(ws, roots.clone(), false, Arc::clone(&interner));
    let scx = ScalarCtx::new_values_first_class(ws).sharing(interner);
    let view = realize_shader_library_view(ws, &graph, &roots)?;
    Ok(ShaderLibrarySession { graph, scx, view })
}

/// The graph, lowering context and registered-program population kept together
/// for routes that do not already own a `skull::shader_artifact` view.
pub struct ShaderProgramSession {
    pub graph: InstanceGraph,
    pub scx: ScalarCtx,
    pub view: ShaderProgramView,
}

fn shader_program_root(
    ws: &Workspace,
    graph: &InstanceGraph,
    fref: FuncRef,
) -> Result<Instance, CompilerError> {
    let canonical = skull_mir::lower::canonical_variant(ws, fref);
    graph
        .instances
        .iter()
        .find(|instance| instance.func == canonical && instance.args.is_empty())
        .cloned()
        .ok_or_else(|| {
            CompilerError::new_no_location(format!(
                "shader graph has no concrete root instance for `{}`",
                ws.module(fref.module).functions[fref.func].name
            ))
        })
}

fn shader_reachable(
    roots: &[Instance],
    edges: &std::collections::BTreeMap<Instance, Vec<Instance>>,
) -> BTreeSet<Instance> {
    let mut seen: BTreeSet<Instance> = roots.iter().cloned().collect();
    let mut queue: std::collections::VecDeque<Instance> = roots.iter().cloned().collect();
    while let Some(instance) = queue.pop_front() {
        for callee in edges.get(&instance).into_iter().flatten() {
            if seen.insert(callee.clone()) {
                queue.push_back(callee.clone());
            }
        }
    }
    seen
}

fn shader_postorder(
    instance: &Instance,
    edges: &std::collections::BTreeMap<Instance, Vec<Instance>>,
    retained: &BTreeSet<Instance>,
    active: &mut BTreeSet<Instance>,
    done: &mut BTreeSet<Instance>,
    order: &mut Vec<Instance>,
) {
    if done.contains(instance) || !active.insert(instance.clone()) {
        return;
    }
    for callee in edges.get(instance).into_iter().flatten() {
        if retained.contains(callee) {
            shader_postorder(callee, edges, retained, active, done, order);
        }
    }
    active.remove(instance);
    done.insert(instance.clone());
    if !order.contains(instance) {
        order.push(instance.clone());
    }
}

fn shader_stage_view(
    ws: &Workspace,
    graph: &InstanceGraph,
    edges: &std::collections::BTreeMap<Instance, Vec<Instance>>,
    entry: Instance,
    stage: ShaderStage,
) -> ShaderStageView {
    let retained = shader_reachable(std::slice::from_ref(&entry), edges);
    let helpers: BTreeSet<Instance> = retained
        .iter()
        .filter(|instance| *instance != &entry)
        .cloned()
        .collect();
    let mut builtins = BTreeSet::new();
    for caller in &retained {
        let Some(sites) = graph.sites_of(caller) else { continue };
        for record in sites.values() {
            let SiteOutcome::Resolved(callee) = &record.outcome else { continue };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if renders_inline_instance(ws, f, callee, graph.interner()) {
                builtins.insert(callee.clone());
            }
        }
    }
    let mut order = Vec::new();
    shader_postorder(
        &entry,
        edges,
        &retained,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
        &mut order,
    );
    order.retain(|instance| instance != &entry);
    let mut dependencies = Vec::new();
    let mut seen = BTreeSet::new();
    for instance in order.iter().chain(std::iter::once(&entry)) {
        let key = (instance.func.module.raw(), instance.func.func.into_raw().into_u32());
        if seen.insert(key) {
            dependencies.push(instance.func);
        }
    }
    ShaderStageView { stage, entry, retained, helpers, builtins, order, dependencies }
}

fn realize_shader_program_view(
    ws: &Workspace,
    graph: &InstanceGraph,
    program: ShaderProgram,
) -> Result<ShaderProgramView, CompilerError> {
    let mut roots = Vec::new();
    if let Some(vertex) = program.vert {
        roots.push(shader_program_root(ws, graph, vertex)?);
    }
    roots.push(shader_program_root(ws, graph, program.frag)?);

    let mut seen = roots.iter().cloned().collect::<BTreeSet<_>>();
    let mut queue = roots.clone();
    let mut edges: std::collections::BTreeMap<Instance, Vec<Instance>> =
        std::collections::BTreeMap::new();
    while let Some(caller) = queue.pop() {
        let Some(sites) = graph.sites_of(&caller) else { continue };
        for record in sites.values() {
            let SiteOutcome::Resolved(callee) = &record.outcome else { continue };
            if graph.concreteness(callee)
                == skull_graph::instance_graph::Concreteness::Missing
            {
                return Err(CompilerError::new_no_location(format!(
                    "shader graph has no substitution for `{}`",
                    ws.module(callee.func.module).functions[callee.func.func].name
                )));
            }
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if renders_inline_instance(ws, f, callee, graph.interner())
                || is_lowered_native_operator(ws, f, callee, graph.interner())
            {
                continue;
            }
            edges.entry(caller.clone()).or_default().push(callee.clone());
            if seen.insert(callee.clone()) {
                queue.push(callee.clone());
            }
        }
    }
    let stages = match program.vert {
        Some(vertex) => vec![
            shader_stage_view(
                ws,
                graph,
                &edges,
                shader_program_root(ws, graph, vertex)?,
                ShaderStage::Vertex,
            ),
            shader_stage_view(
                ws,
                graph,
                &edges,
                shader_program_root(ws, graph, program.frag)?,
                ShaderStage::Fragment,
            ),
        ],
        None => vec![shader_stage_view(
            ws,
            graph,
            &edges,
            shader_program_root(ws, graph, program.frag)?,
            ShaderStage::Fragment,
        )],
    };
    let concrete_plans = skull_graph::concrete_body::build(ws, graph).plans;
    Ok(ShaderProgramView { program, stages, concrete_plans })
}

/// Build a registered-program view from the shared graph. The returned session
/// is the compatibility seam for callers that have roots but no retained
/// `ShaderArtifactView`; emission itself still consumes the supplied view.
pub fn shader_program_session(
    ws: &Workspace,
    program: ShaderProgram,
) -> Result<ShaderProgramSession, CompilerError> {
    let roots: Vec<_> = program
        .vert
        .into_iter()
        .chain(std::iter::once(program.frag))
        .collect();
    let interner = Arc::new(MonoInterner::new());
    let graph = skull_graph::instance_graph::build_in(
        ws,
        roots,
        false,
        Arc::clone(&interner),
    );
    let scx = ScalarCtx::new_values_first_class(ws).sharing(interner);
    let view = realize_shader_program_view(ws, &graph, program)?;
    Ok(ShaderProgramSession { graph, scx, view })
}

fn checked_library_callees(
    ws: &Workspace,
    caller: &ShaderInstance,
    callees: Vec<ShaderInstance>,
    allowed: &BTreeSet<Instance>,
    interner: &MonoInterner,
    builtins: &mut BTreeSet<Instance>,
) -> Result<Vec<ShaderInstance>, CompilerError> {
    let caller_f = &ws.module(caller.fref().module).functions[caller.fref().func];
    let mut retained = Vec::new();
    for callee in callees {
        let f = &ws.module(callee.fref().module).functions[callee.fref().func];
        if renders_inline_instance(ws, f, &callee.inst, interner) {
            builtins.insert(callee.inst.clone());
            continue;
        }
        if is_lowered_native_operator(ws, f, &callee.inst, interner) {
            continue;
        }
        if f.is_extern || f.inline_body.is_some() {
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no GLSL intrinsic mapping",
                    f.name
                ),
            ));
        }
        if !allowed.contains(&callee.inst) {
            return Err(err(
                caller_f.name_span,
                format!(
                    "shader library view omitted the concrete callee `{}` reached by `{}`",
                    f.name, caller_f.name
                ),
            ));
        }
        retained.push(callee);
    }
    Ok(retained)
}

/// Emit a no-`#version` GLSL function library from a supplied graph population.
/// The view owns identity, retention, builtins, order and dependencies; this
/// function only lowers those instances, validates their bodies and renders them.
pub fn emit_shader_library_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderLibraryView,
) -> Result<String, CompilerError> {
    if view.order.iter().cloned().collect::<BTreeSet<_>>() != view.retained
        || view.order.len() != view.retained.len()
        || view.roots.iter().any(|root| !view.retained.contains(root))
    {
        return Err(CompilerError::new_no_location(
            "shader library view order is not the retained population".to_string(),
        ));
    }
    let allowed = &view.retained;
    let mut bodies: HashMap<ShaderInstance, MirFunc> = HashMap::default();
    let mut calls: HashMap<ShaderInstance, Vec<ShaderInstance>> = HashMap::default();
    let mut discovered_builtins = BTreeSet::new();
    let mut uses_idiv = false;
    let mut uses_imod = false;
    for raw in &view.order {
        let instance = ShaderInstance::new(raw.clone());
        let f = &ws.module(instance.fref().module).functions[instance.fref().func];
        let subst = instance.subst(ws, scx.interner());
        check_tier0_signature_spec(ws, &subst, f)?;
        let plan = view.concrete_plans.get(raw).ok_or_else(|| {
            err(f.name_span, format!("shader profile: no concrete plan for `{}`", f.name))
        })?;
        let body = lower_instance_with_plan(ws, scx, plan).map_err(|e| {
            err(
                f.name_span,
                format!("shader profile: `{}` cannot lower for GLSL: {}", f.name, e.message),
            )
        })?;
        let mut direct = Vec::new();
        profile_body(ws, f, &body, &mut direct)?;
        let retained = checked_library_callees(
            ws,
            &instance,
            direct,
            allowed,
            scx.interner(),
            &mut discovered_builtins,
        )?;
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
        calls.insert(instance.clone(), retained);
        bodies.insert(instance, body);
    }
    if discovered_builtins != view.builtins {
        return Err(CompilerError::new_no_location(
            "shader library view builtin population differs from lowered calls".to_string(),
        ));
    }
    reject_population_cycles(ws, &calls)?;
    let mut dependencies = Vec::new();
    let mut seen_dependencies = BTreeSet::new();
    for instance in view.order.iter().chain(view.roots.iter()) {
        if seen_dependencies.insert((instance.func.module.raw(), instance.func.func.into_raw().into_u32())) {
            dependencies.push(instance.func);
        }
    }
    if dependencies != view.dependencies {
        return Err(CompilerError::new_no_location(
            "shader library view dependencies differ from retained order".to_string(),
        ));
    }
    let mut out = helper_prelude(uses_idiv, uses_imod);
    if !out.is_empty() {
        out.push('\n');
    }
    for (index, raw) in view.order.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let instance = ShaderInstance::new(raw.clone());
        let body = bodies.get(&instance).expect("view order body was lowered");
        out.push_str(&emit_fn_def(ws, scx.interner(), &instance, body)?);
    }
    Ok(out)
}

/// Compatibility wrapper for callers that have not yet retained a graph. The
/// route itself is still graph-backed; production and tests that own a session
/// should call [`emit_shader_library_with_view`] directly.
pub fn emit_shader_library(
    ws: &Workspace,
    mid: ModuleId,
    roots: &[String],
) -> Result<String, CompilerError> {
    let root_refs: Vec<_> = roots
        .iter()
        .map(|root| resolve_root(ws, mid, root))
        .collect::<Result<_, _>>()?;
    let session = shader_library_session(ws, root_refs)?;
    emit_shader_library_with_view(ws, &session.scx, &session.view)
}

// --- A1: fragment-entry compilation (docs/shaders.md) --------------------

/// A classified shader entry signature (A1 fragment, or an A2 pipeline stage).
struct EntryShape {
    /// Param indices whose field reads render as bare GLSL globals and which are
    /// dropped from the emitted signature: the A1 uniform struct, or a pipeline
    /// stage's attribute / uniform / varyings structs.
    global_params: Vec<usize>,
    /// Combined global field name -> GLSL type (for `ty_of` on bare-global
    /// reads). Names are collision-checked unique across all global structs.
    uniform_fields: Vec<(Ustr, GlslTy)>,
    /// Sampler param INDICES, dropped from the emitted signature and read as
    /// bare globals. Indices only: which samplers exist and what kind they are
    /// is the interface's answer, and a second copy here is what let the vertex
    /// stage reference a sampler it never declared.
    sampler_params: Vec<usize>,
    /// `UniformArray<T>` param indices. Dropped from the signature and referenced
    /// by name (via `at()` -> indexing); NOT tier-0-typed, so they are skipped
    /// when typing param cells.
    arrays: Vec<usize>,
    /// `void main()` mode: presence switches emission to `void main()` with a
    /// return-struct destructure (vertex varyings, or MRT fragment outputs).
    main_out: Option<MainOut>,
}

/// The default fullscreen vertex stage -- identical to the hand-written
/// vignette/heat_shimmer VERTEX blocks (the loader compiles the one file twice
/// with `#define VERTEX` / `#define PIXEL`, `stone-rs/src/shader.rs`).
const FULLSCREEN_VERTEX: &str = "layout (location = 0) in vec2 aPos;
layout (location = 1) in vec2 aTexCoord;

out vec2 TexCoord;

void main() {
    gl_Position = vec4(aPos, 0.0, 1.0);
    TexCoord = aTexCoord;
}
";

/// The nominal name behind a declared type, if any (`ShimmerU`, `Sampler2D`).
fn nominal_name(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    match ty_expr(ws, ty)? {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => Some(n),
        _ => None,
    }
}

/// A1's own classification: param roles and group names, in the neutral
/// vocabulary. The body-emission view ([`EntryShape`]) is derived from the
/// completed INTERFACE afterwards, exactly as A2's is -- building it here
/// would be a second uniform classification beside `uniform_group_members`.
struct EntryClass {
    global_params: Vec<usize>,
    sampler_params: Vec<usize>,
    samplers: Vec<(usize, Ustr, SamplerKind)>,
    texture_group: Option<Ustr>,
    uniform_group: Option<Ustr>,
}

/// Classify an entry fn's params against the A1 contract: at most one uniform
/// value struct (every field tier 0), any number of `Sampler2D`, exactly one
/// `Vec2` (the fullscreen TexCoord varying); return `Vec4`.
fn classify_entry(ws: &Workspace, f: &Function) -> Result<EntryClass, CompilerError> {
    let span = f.name_span;
    let mut uniform_param = None;
    let mut samplers: Vec<(usize, Ustr, SamplerKind)> = Vec::new();
    let mut tc = None;
    let mut texture_params: Vec<usize> = Vec::new();
    let mut texture_group: Option<Ustr> = None;
    let mut uniform_group: Option<Ustr> = None;
    for (i, pd) in f.param_decls.iter().enumerate() {
        if has_anno(&pd.annotations, "textures") {
            reject_conflicting_texture_role(&pd.annotations, pd.name, span, "entry")?;
            if texture_group.is_some() {
                return Err(err(span, "shader entry: at most one @textures group param is allowed -- one group is the binding set"));
            }
            texture_group = nominal_name(ws, pd.ty);
            samplers.extend(texture_group_samplers(ws, pd.ty, i, span, "entry")?);
            texture_params.push(i);
            continue;
        }
        match glsl_ty(ws, &SpecSubst::default(), pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, sampler_kind_of(t))),
            Ok(GlslTy::Vec2) => {
                if tc.is_some() {
                    return Err(err(span, "shader entry: exactly one Vec2 param (the fullscreen TexCoord varying) is allowed"));
                }
                tc = Some(i);
            }
            Ok(other) => {
                return Err(err(span, format!(
                    "shader entry: param `{}` ({}) is not allowed -- A1 uniforms go in one uniform struct param, plus Sampler2D params and one Vec2",
                    pd.name, other.name()
                )));
            }
            Err(e) => {
                // Not a tier-0 leaf: a value struct here is the uniform struct,
                // which must carry @uniform (one rule everywhere with the A2
                // pipeline -- attributes/uniforms are never silently inferred).
                let Some(sname) = nominal_name(ws, pd.ty) else { return Err(e) };
                let Some(&sid) = ws.struct_by_name.get(&sname) else { return Err(e) };
                if !has_anno(&pd.annotations, "uniform") {
                    return Err(err(span, format!(
                        "shader entry: value-struct param `{}` needs @uniform (the uniform block marker)",
                        pd.name
                    )));
                }
                if uniform_param.is_some() {
                    return Err(err(span, "shader entry: at most one uniform struct param is allowed"));
                }
                // The group's members are classified where they become the
                // interface's (`uniform_group_members`), which is the same
                // oracle the std140 packer reads. A `glsl_ty` walk here could
                // reject BEFORE it and was free to disagree -- it admits
                // Sampler2D and Bvec3, which no uniform block can pack.
                let _ = sid;
                uniform_param = Some(i);
                uniform_group = Some(sname);
            }
        }
    }
    if tc.is_none() {
        return Err(err(span, "shader entry: a Vec2 param (the fullscreen TexCoord varying) is required"));
    }
    match f.return_ty.map(|rt| glsl_ty(ws, &SpecSubst::default(), rt, span, "return")) {
        Some(Ok(GlslTy::Vec4)) => {}
        _ => return Err(err(span, "shader entry: the return type must be Vec4 (FragColor)")),
    }
    Ok(EntryClass {
        global_params: uniform_param.into_iter().chain(texture_params).collect(),
        sampler_params: samplers.iter().map(|&(i, _, _)| i).collect(),
        samplers,
        texture_group,
        uniform_group,
    })
}

/// A1's global namespace census, over its completed INTERFACE.
///
/// Uniform members, samplers and the reserved `TexCoord`/`FragColor` share one
/// GLSL global namespace. Run over the interface rather than over a walk of the
/// source struct, so it censuses the same members the block is packed from.
fn check_entry_collisions(
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    let mut seen: HashSet<Ustr> =
        [Ustr::from("TexCoord"), Ustr::from("FragColor")].into_iter().collect();
    let names = iface
        .uniform_groups
        .iter()
        .flat_map(|g| g.members.iter().map(|(n, _)| *n))
        .chain(iface.textures.iter().flat_map(|t| t.samplers.iter().map(|(n, _)| *n)))
        .chain(iface.bare_samplers.iter().map(|s| s.name));
    for name in names {
        if !seen.insert(name) {
            return Err(err(span, format!("shader entry: uniform name `{name}` would collide (fields, samplers, TexCoord and FragColor share the GLSL global namespace)")));
        }
    }
    Ok(())
}

/// A1's body-emission view, derived from its completed interface.
///
/// The uniform field types come from `iface`, which got them from
/// `uniform_group_members` -- the same classifier the std140 packer reads, so a
/// body's typing and a block's bytes cannot disagree about a field.
fn entry_shape(class: &EntryClass, iface: &ShaderInterfacePlan) -> EntryShape {
    EntryShape {
        global_params: class.global_params.clone(),
        uniform_fields: stage_uniform_fields(iface, IrStage::Fragment),
        sampler_params: class.sampler_params.clone(),
        arrays: Vec::new(),
        main_out: None,
    }
}

/// Emit a COMPLETE single-file shader for one `@shader(fragment)` root: the
/// generated PIXEL block (declarations + reachable helpers + the entry fn +
/// `main`) and the default fullscreen VERTEX block.
pub fn emit_shader_entry(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    Ok(emit_shader_entry_with_dependencies(ws, mid, root, profile)?.source)
}

pub struct ShaderEmission {
    pub source: String,
    pub dependencies: Vec<FuncRef>,
    /// What this program's interface MEANS, with no target in it. Published so
    /// a second backend can answer it without re-classifying the entry.
    pub interface: ShaderInterfacePlan,
    /// GLSL's physical answer: bindings, std140 layouts, fingerprints and
    /// texture units. The engine binds against THIS -- it never parses `source`
    /// and never recomputes a layout.
    pub abi: ShaderAbiPlan,
    /// The program's concrete bodies, per stage. Target-neutral: a second
    /// backend renders these under the same interface rather than re-walking
    /// the entry and re-deciding what the program packages.
    pub ir: ShaderIR,
}


/// A program's uniform blocks -- scalar AND array -- flattened into the
/// PARALLEL LISTS the registration call carries.
///
/// One entry per block in `names`/`bindings`/`sizes`/`fingerprints`/`counts`,
/// with the members of every block concatenated into the `member_*` lists and
/// `counts[i]` saying how many belong to block `i`. A hand-written driver never
/// has to nest, which is why the shape is this and not a table.
///
/// **Built once, read by both hosts.** The Lua and JS registration lines are
/// written by different emitters, and a second flattener is how the two would
/// come to disagree about member order -- the offsets the host packs against
/// are positional, so a divergence there is silent wrong bytes rather than an
/// error.
///
/// Array blocks are appended AFTER the scalar ones. They travel the same wire
/// deliberately: a block is a block to the driver -- bind, verify against the
/// plan, allocate scratch, upload whole -- and giving arrays their own
/// registration call would split a registration that has to be transactional.
/// The three per-member lists are what tells them apart:
///
/// - `member_kinds` is the [`WireKind::code`] of every member, array or not;
/// - `member_capacities` is `0` for a scalar member and `N` for a `@size(N)`
///   array -- so "how many elements may be written here" answers "is this an
///   array" without a separate flag;
/// - `member_strides` is `0` for a scalar member and the std140 array stride
///   otherwise. Sent rather than re-derived host-side: it is 16 for every kind
///   an array may hold today, and a host that recomputed it would be a second
///   layout authority the moment that stops being true.
#[derive(Debug, Default)]
pub struct BlockWire<'a> {
    pub names: Vec<&'a str>,
    pub bindings: Vec<i64>,
    pub sizes: Vec<i64>,
    pub fingerprints: Vec<i64>,
    pub counts: Vec<i64>,
    pub member_names: Vec<&'a str>,
    pub member_offsets: Vec<i64>,
    pub member_kinds: Vec<i64>,
    pub member_capacities: Vec<i64>,
    pub member_strides: Vec<i64>,
}

impl<'a> BlockWire<'a> {
    pub fn of(
        scalar: impl Iterator<Item = &'a (u32, UniformBlockPlan)>,
        arrays: impl Iterator<Item = &'a (u32, UniformArrayBlockPlan)>,
    ) -> Self {
        let mut w = BlockWire::default();
        for (binding, p) in scalar {
            w.names.push(p.struct_name.as_str());
            w.bindings.push(*binding as i64);
            w.sizes.push(p.size as i64);
            w.fingerprints.push(p.fingerprint() as i64);
            w.counts.push(p.members.len() as i64);
            for m in &p.members {
                w.member_names.push(m.name.as_str());
                w.member_offsets.push(m.offset as i64);
                w.member_kinds.push(m.kind.code() as i64);
                // A scalar member takes no array payload, and the host's push
                // verbs reject a capacity-0 target rather than writing one
                // element into it.
                w.member_capacities.push(0);
                w.member_strides.push(0);
            }
        }
        for (binding, p) in arrays {
            w.names.push(p.block_name.as_str());
            w.bindings.push(*binding as i64);
            w.sizes.push(p.size as i64);
            w.fingerprints.push(p.fingerprint() as i64);
            w.counts.push(p.members.len() as i64);
            for m in &p.members {
                w.member_names.push(m.name.as_str());
                w.member_offsets.push(m.offset as i64);
                w.member_kinds.push(m.elem.code() as i64);
                w.member_capacities.push(m.capacity as i64);
                w.member_strides.push(m.stride as i64);
            }
        }
        w
    }
}

/// Uniform block binding indices, assigned by param ROLE so the slot is
/// structural rather than a consequence of how many groups a program declares.
/// Slot 0 is RESERVED for `@engine` whether or not the program has one, which is
/// what lets the engine reject a foreign block there at registration.
///
/// The array slots are reserved on the same terms. ES 3.0 guarantees 12 uniform
/// blocks per stage and 24 combined, so five reserved indices are affordable.
/// A stage's `UniformArray<T>` members, as a logical group. `None` when the
/// stage declares none.
fn logical_array_group(
    stage: IrStage,
    owner: Ustr,
    arrays: &[ArrayParam],
) -> Option<LogicalArrayGroup> {
    (!arrays.is_empty()).then(|| LogicalArrayGroup {
        stage,
        owner,
        members: arrays.iter().map(|a| a.decl).collect(),
    })
}

/// The neutral wire kind of a tier-0 GLSL type. Total over what an INTERFACE
/// position can hold -- a varying or fragment output is one of these by
/// construction, since anything else is refused when the stage is classified.
fn wire_kind_of(t: GlslTy) -> WireKind {
    match t {
        GlslTy::Float => WireKind::F32,
        GlslTy::Int => WireKind::I32,
        GlslTy::Bool => WireKind::Bool32,
        GlslTy::Vec2 => WireKind::F32x2,
        GlslTy::Vec3 | GlslTy::Bvec3 => WireKind::F32x3,
        GlslTy::Vec4 => WireKind::F32x4,
        GlslTy::Mat4 => WireKind::Mat4,
        // A sampler is a RESOURCE, never an interpolated or returned value; it
        // reaches the interface through `LogicalTextureGroup` instead.
        GlslTy::Sampler2D | GlslTy::ISampler2D => WireKind::I32,
    }
}

/// A stage's own uniform members plus the program's `@engine` members, spelled
/// for GLSL -- what a body needs to type a bare-global read.
///
/// From the INTERFACE. The shapes carried a second GLSL-typed copy of the same
/// fields, which is the drift this split exists to remove.
fn stage_uniform_fields(
    iface: &ShaderInterfacePlan,
    stage: IrStage,
) -> Vec<(Ustr, GlslTy)> {
    iface
        .uniform_group(UniformRole::Engine)
        .into_iter()
        .chain(iface.uniform_group(UniformRole::Stage(stage)))
        .flat_map(|g| g.members.iter().map(|(n, k)| (*n, wire_glsl(*k))))
        .collect()
}


/// The varyings a program passes between stages, spelled for GLSL. Names,
/// kinds and `@flat` come from the INTERFACE; only the spelling is this
/// target's.
fn interface_varyings(iface: &ShaderInterfacePlan) -> Vec<(Ustr, GlslTy, bool)> {
    iface.varyings.iter().map(|v| (v.name, wire_glsl(v.kind), v.flat)).collect()
}

/// The fragment stage's MRT outputs, or `None` for the single implicit one.
fn interface_mrt_outputs(iface: &ShaderInterfacePlan) -> Option<Vec<(Ustr, GlslTy)>> {
    match &iface.fragment_output {
        FragmentOutput::Mrt(outs) => {
            Some(outs.iter().map(|(n, k)| (*n, wire_glsl(*k))).collect())
        }
        FragmentOutput::Single(_) | FragmentOutput::None => None,
    }
}

/// A neutral wire kind spelled as a GLSL type. The inverse of `wire_kind_of`,
/// and the only place the interface's vocabulary becomes GLSL's.
fn wire_glsl(k: WireKind) -> GlslTy {
    match k {
        WireKind::F32 => GlslTy::Float,
        WireKind::I32 => GlslTy::Int,
        WireKind::Bool32 => GlslTy::Bool,
        WireKind::F32x2 => GlslTy::Vec2,
        WireKind::F32x3 => GlslTy::Vec3,
        WireKind::F32x4 => GlslTy::Vec4,
        WireKind::Mat4 => GlslTy::Mat4,
    }
}

/// **The whole GLSL ABI plan checked against the interface it claims to
/// answer**, before any stage looks at it.
///
/// Presence at a binding is not identity: a block of the right shape at the
/// right index emits the wrong declarations if it is a DIFFERENT group's, and
/// every per-binding presence check passes while it does. So this compares the
/// block's own name and its complete member list. It runs over the PROGRAM
/// rather than per stage, because an unknown binding belongs to no stage and
/// would otherwise be nobody's to reject.
fn validate_abi(
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    span: Span,
) -> Result<(), CompilerError> {
    // --- scalar blocks: exactly the interface's groups, at their own bindings.
    let mut expected: Vec<(u32, &LogicalUniformGroup)> = iface
        .uniform_groups
        .iter()
        .map(|g| (ShaderAbiPlan::binding_of(g.role), g))
        .collect();
    expected.sort_by_key(|(b, _)| *b);
    // Two groups claiming one role makes the binding ambiguous, and the defect
    // is the interface's rather than the plan's.
    if let Some(w) = expected.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(err(span, format!(
            "internal: the interface declares two uniform groups at binding {} (`{}` and `{}`)",
            w[0].0, w[0].1.struct_name, w[1].1.struct_name
        )));
    }
    let mut seen: HashSet<u32> = HashSet::default();
    for (b, plan) in &abi.uniform_blocks {
        if !seen.insert(*b) {
            return Err(err(span, format!(
                "internal: the GLSL ABI plan holds more than one uniform block at binding {b}"
            )));
        }
        let Some((_, g)) = expected.iter().find(|(eb, _)| eb == b) else {
            return Err(err(span, format!(
                "internal: the GLSL ABI plan holds a uniform block at binding {b}, which no \
                 interface group claims"
            )));
        };
        if plan.struct_name != g.struct_name {
            return Err(err(span, format!(
                "internal: binding {b} holds block `{}` but the interface's group there is `{}`",
                plan.struct_name, g.struct_name
            )));
        }
        let laid: Vec<(Ustr, WireKind)> = plan.members.iter().map(|m| (m.name, m.kind)).collect();
        if laid != g.members {
            return Err(err(span, format!(
                "internal: block `{}` at binding {b} was laid out over {laid:?}, but the \
                 interface declares {:?}",
                plan.struct_name, g.members
            )));
        }
    }
    for (b, g) in &expected {
        if !seen.contains(b) {
            return Err(err(span, format!(
                "internal: the interface declares group `{}` at binding {b}, which the GLSL ABI \
                 plan has no block for -- its members would be referenced without being declared",
                g.struct_name
            )));
        }
    }

    // --- array blocks: same rule, including each member's CAPACITY, which is
    // what an `@size(N)` change moves and what a name check alone would miss.
    let mut expected_arrays: Vec<(u32, &LogicalArrayGroup)> = iface
        .array_groups
        .iter()
        .map(|g| (ShaderAbiPlan::array_binding_of(g.stage), g))
        .collect();
    expected_arrays.sort_by_key(|(b, _)| *b);
    if let Some(w) = expected_arrays.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(err(span, format!(
            "internal: the interface declares two array groups for one stage at binding {} \
             (owned by `{}` and `{}`)",
            w[0].0, w[0].1.owner, w[1].1.owner
        )));
    }
    let mut seen_arrays: HashSet<u32> = HashSet::default();
    for (b, plan) in &abi.uniform_array_blocks {
        if !seen_arrays.insert(*b) {
            return Err(err(span, format!(
                "internal: the GLSL ABI plan holds more than one array block at binding {b}"
            )));
        }
        let Some((_, g)) = expected_arrays.iter().find(|(eb, _)| eb == b) else {
            return Err(err(span, format!(
                "internal: the GLSL ABI plan holds an array block at binding {b}, which no \
                 interface group claims"
            )));
        };
        if plan.block_name != array_block_name(g.owner) {
            return Err(err(span, format!(
                "internal: binding {b} holds array block `{}`, but the interface's group there \
                 is owned by `{}`",
                plan.block_name, g.owner
            )));
        }
        let laid: Vec<(Ustr, WireKind, u32)> =
            plan.members.iter().map(|m| (m.name, m.elem, m.capacity)).collect();
        let want: Vec<(Ustr, WireKind, u32)> =
            g.members.iter().map(|m| (m.name, m.elem, m.capacity)).collect();
        if laid != want {
            return Err(err(span, format!(
                "internal: array block `{}` was laid out over {laid:?}, but the interface \
                 declares {want:?}",
                plan.block_name
            )));
        }
    }
    for (b, g) in &expected_arrays {
        if !seen_arrays.contains(b) {
            return Err(err(span, format!(
                "internal: the interface declares an array group owned by `{}` at binding {b}, \
                 which the GLSL ABI plan has no block for",
                g.owner
            )));
        }
    }

    // --- attribute locations: a bijection on names AND non-overlapping
    // location RANGES. Two attributes at one location is a silent aliasing
    // that a name bijection alone accepts.
    let attrs: Vec<(Ustr, AttrKind)> =
        iface.vertex_inputs.iter().flat_map(|i| i.attrs.iter().copied()).collect();
    if attrs.len() != abi.attribute_locations.len() {
        return Err(err(span, format!(
            "internal: the interface declares {} vertex attribute(s) and the GLSL ABI plan \
             assigns {} location(s)",
            attrs.len(),
            abi.attribute_locations.len()
        )));
    }
    let mut occupied: HashMap<u32, Ustr> = HashMap::default();
    for (name, kind) in &attrs {
        let Some((_, loc)) = abi.attribute_locations.iter().find(|(an, _)| an == name) else {
            return Err(err(span, format!(
                "internal: vertex attribute `{name}` has no location in the GLSL ABI plan"
            )));
        };
        let Some(end) = loc.checked_add(attr_location_span(*kind)) else {
            return Err(err(span, format!(
                "internal: vertex attribute `{name}` at location {loc} spans past the end of the \
                 location space"
            )));
        };
        for slot in *loc..end {
            if let Some(other) = occupied.insert(slot, *name) {
                return Err(err(span, format!(
                    "internal: vertex attributes `{other}` and `{name}` both occupy location \
                     {slot}"
                )));
            }
        }
    }

    // --- texture units: the same bijection the stage join needs, checked once
    // for the program so an unused stage cannot leave it unasked.
    let group = iface.sampler_names();
    let mut units: HashSet<Ustr> = HashSet::default();
    for u in &abi.texture_units {
        if !units.insert(*u) {
            return Err(err(span, format!(
                "internal: the GLSL ABI plan assigns sampler `{u}` more than one unit"
            )));
        }
        if !group.contains(u) {
            return Err(err(span, format!(
                "internal: the GLSL ABI plan assigns a unit to `{u}`, which the interface's \
                 @textures group does not name"
            )));
        }
    }
    for n in &group {
        if !units.contains(n) {
            return Err(err(span, format!(
                "internal: the interface's @textures group names `{n}`, which the GLSL ABI plan \
                 gives no unit -- it would be referenced without being declared"
            )));
        }
    }
    Ok(())
}

/// **One stage's resolved ABI**: every physical entry the interface says this
/// stage has, and nothing else.
///
/// Projected by [`project_stage_abi`] from a plan [`validate_abi`] has already
/// checked. An `Option` here means the INTERFACE declares no such group; it
/// never means one was declared and not found, because that rejects. A shader
/// referencing what it never declared is a driver compile error with no
/// compiler diagnostic at all.
struct StageAbi<'a> {
    uniform_block: Option<&'a UniformBlockPlan>,
    engine_block: Option<&'a UniformBlockPlan>,
    array_block: Option<&'a UniformArrayBlockPlan>,
    samplers: Vec<(Ustr, SamplerKind)>,
}

/// The one block the interface says exists at `binding`, or a rejection.
///
/// `want` is whether the INTERFACE has the group, so `None` means "not
/// declared" and never "declared but not found". Missing, extra and duplicate
/// are each their own error: "the ABI omitted it", "the ABI invented it" and
/// "the ABI has it twice" are different defects and a count comparison alone
/// cannot tell the second from the third.
fn require_block<'a, T>(
    blocks: &'a [(u32, T)],
    binding: u32,
    want: bool,
    what: &str,
    span: Span,
) -> Result<Option<&'a T>, CompilerError> {
    let mut found = blocks.iter().filter(|(b, _)| *b == binding);
    let first = found.next();
    if found.next().is_some() {
        return Err(err(span, format!(
            "internal: the GLSL ABI plan holds more than one {what} at binding {binding}"
        )));
    }
    match (want, first) {
        (true, Some((_, p))) => Ok(Some(p)),
        (false, None) => Ok(None),
        (true, None) => Err(err(span, format!(
            "internal: the interface declares a {what} but the GLSL ABI plan has none at \
             binding {binding} -- its members would be referenced without being declared"
        ))),
        (false, Some(_)) => Err(err(span, format!(
            "internal: the GLSL ABI plan holds a {what} at binding {binding} that the interface \
             does not declare"
        ))),
    }
}

/// The samplers ONE STAGE declares, in unit order, then its bare ones.
///
/// [`ShaderInterfacePlan::stage_samplers`] is the source; this adds the target's
/// unit ORDER and validates the join. The check is a real BIJECTION, not length
/// plus membership: `[a, a]` against an interface of `[a, b]` has the right
/// length and every element is a member, and it emits `a` twice while dropping
/// `b` entirely.
fn stage_samplers_in_unit_order(
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    stage: IrStage,
) -> Vec<(Ustr, SamplerKind)> {
    let group: Vec<Ustr> = iface.sampler_names();
    let mine = iface.stage_samplers(stage);
    let mut out: Vec<(Ustr, SamplerKind)> = Vec::new();
    for u in &abi.texture_units {
        if let Some(s) = mine.iter().find(|(n, _)| n == u) {
            out.push(*s);
        }
    }
    // The bare ones have no unit to sort by, so they keep declaration order.
    out.extend(mine.iter().filter(|(n, _)| !group.contains(n)).copied());
    out
}

/// **A GLSL ABI plan checked against its interface, with each stage's slice
/// already projected.**
///
/// Validation is a PROGRAM property -- an unknown binding belongs to no stage,
/// and re-running the whole check per stage says nothing the first run did not.
/// Built once by [`ProgramAbi::of`]; the emitters receive projections.
struct ProgramAbi<'a> {
    stages: Vec<(IrStage, StageAbi<'a>)>,
    /// Anchors a projection failure, so a lookup miss is a spanned rejection
    /// rather than a panic.
    span: Span,
    /// The vertex attribute declarations, resolved once. Only the vertex stage
    /// renders them, and the name/location join they need is exactly what
    /// validation just established.
    attributes: Vec<(Ustr, GlslTy, u32)>,
}

impl<'a> ProgramAbi<'a> {
    /// Validate `abi` against `iface`, then project exactly the stages the
    /// renderer will ask for.
    ///
    /// `required` is a census, not a hint: a renderer that emits a vertex
    /// stage the interface does not declare is a defect, and taking
    /// `iface.stages` on trust would turn it into a lookup miss later.
    fn of(
        iface: &ShaderInterfacePlan,
        abi: &'a ShaderAbiPlan,
        required: &[IrStage],
        span: Span,
    ) -> Result<Self, CompilerError> {
        validate_abi(iface, abi, span)?;
        let mut stages = Vec::with_capacity(required.len());
        for &stage in required {
            if !iface.stages.contains(&stage) {
                return Err(err(span, format!(
                    "internal: the program emits a {} stage, which its interface does not declare",
                    stage.name()
                )));
            }
            stages.push((stage, project_stage_abi(iface, abi, stage, span)?));
        }
        Ok(ProgramAbi {
            stages,
            attributes: attribute_declarations(iface, abi, span)?,
            span,
        })
    }

    fn stage(&self, stage: IrStage) -> Result<&StageAbi<'a>, CompilerError> {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, a)| a)
            .ok_or_else(|| err(self.span, format!(
                "internal: no {} stage was projected for this program",
                stage.name()
            )))
    }
}

/// One stage's slice of an already-validated plan.
///
/// [`validate_abi`] has established that every group the interface declares
/// has exactly one matching block, so these lookups should not fail -- and
/// "should not" is why each one REJECTS instead of resolving to `None`. An
/// absence that the interface did not ask for is the same defect as a missing
/// declaration, and turning it back into `None` here would reintroduce it
/// downstream of the check that exists to catch it.
fn project_stage_abi<'a>(
    iface: &ShaderInterfacePlan,
    abi: &'a ShaderAbiPlan,
    stage: IrStage,
    span: Span,
) -> Result<StageAbi<'a>, CompilerError> {
    Ok(StageAbi {
        uniform_block: require_block(
            &abi.uniform_blocks,
            ShaderAbiPlan::binding_of(UniformRole::Stage(stage)),
            iface.uniform_group(UniformRole::Stage(stage)).is_some(),
            "uniform block",
            span,
        )?,
        engine_block: require_block(
            &abi.uniform_blocks,
            UNIFORM_BLOCK_BINDING_ENGINE,
            iface.uniform_group(UniformRole::Engine).is_some(),
            "@engine block",
            span,
        )?,
        array_block: require_block(
            &abi.uniform_array_blocks,
            ShaderAbiPlan::array_binding_of(stage),
            iface.array_group(stage).is_some(),
            "uniform array block",
            span,
        )?,
        samplers: stage_samplers_in_unit_order(iface, abi, stage),
    })
}

/// Every vertex attribute's emitted declaration: its name and neutral kind from
/// the INTERFACE, its location from the target's ABI plan.
///
/// Rejects a missing location rather than skipping the attribute. A dropped
/// `in` declaration is a driver error with no compiler diagnostic, which is the
/// one outcome this path may not produce.
fn attribute_declarations(
    iface: &ShaderInterfacePlan,
    abi: &ShaderAbiPlan,
    span: Span,
) -> Result<Vec<(Ustr, GlslTy, u32)>, CompilerError> {
    let mut out = Vec::new();
    for (n, k) in iface.vertex_inputs.iter().flat_map(|i| i.attrs.iter()) {
        let Some((_, loc)) = abi.attribute_locations.iter().find(|(an, _)| an == n) else {
            return Err(err(span, format!(
                "internal: vertex attribute `{n}` has no location in the GLSL ABI plan -- it \
                 would be referenced without being declared"
            )));
        };
        out.push((*n, attr_glsl_ty(*k), *loc));
    }
    Ok(out)
}

/// The samplers that are NOT part of a `@textures` group -- loose params whose
/// unit the CPU picks. `has_bare_sampler` answers whether any exist (a
/// rejection when a group is also present); this is the list itself.
fn bare_sampler_list(
    samplers: &[(usize, Ustr, SamplerKind)],
    grouped_params: &[usize],
    stage: IrStage,
) -> Vec<LogicalSampler> {
    samplers
        .iter()
        .filter(|(i, _, _)| !grouped_params.contains(i))
        .map(|(_, n, k)| LogicalSampler { name: *n, kind: *k, stage })
        .collect()
}

/// The neutral member list of a uniform group.
///
/// `gpu_layout::uniform_group_members` is the ONE classifier -- the same one
/// `uniform_block` reads before laying anything out, so the interface and the
/// CPU packer cannot disagree about a kind. Places come later, from
/// `std140_block`, over exactly these members.
fn logical_members(
    ws: &Workspace,
    struct_name: Ustr,
    span: Span,
) -> Result<Vec<(Ustr, WireKind)>, CompilerError> {
    skull_mir::gpu_layout::uniform_group_members(ws, struct_name)
        .map_err(|e| err(span, e.message))
}

fn logical_uniform_group(
    ws: &Workspace,
    role: UniformRole,
    struct_name: Ustr,
    span: Span,
) -> Result<LogicalUniformGroup, CompilerError> {
    Ok(LogicalUniformGroup {
        role,
        struct_name,
        members: logical_members(ws, struct_name, span)?,
    })
}

fn logical_textures(
    ws: &Workspace,
    group: Option<Ustr>,
    has_bare_sampler: bool,
    stages: Vec<IrStage>,
    span: Span,
) -> Result<Option<LogicalTextureGroup>, CompilerError> {
    let Some(g) = group else { return Ok(None) };
    if has_bare_sampler {
        return Err(err(span, format!(
            "shader: `{g}` is a @textures group, so every sampler must join it -- a bare Sampler2D param keeps a CPU-assigned unit, which can collide with a compiler-assigned slot"
        )));
    }
    let ts = texture_slots(ws, g).map_err(|e| err(span, e.message))?;
    Ok(Some(LogicalTextureGroup {
        struct_name: Some(g),
        samplers: ts.slots.iter().map(|s| (s.name, s.kind)).collect(),
        stages,
    }))
}

/// **The GLSL ABI of one program's interface.**
///
/// The physical answer to [`ShaderInterfacePlan`]'s logical question, and the
/// ONLY object that holds one: block binding indices, std140 byte offsets and
/// sizes, block fingerprints, array-block names and capacities, sampler units,
/// and attribute locations. Nothing upstream of here knows any of it, and the
/// engine reads this rather than parsing emitted GLSL or re-deriving a layout.
///
/// Built by [`Self::glsl`] from the interface plan alone. A second target
/// derives its own from the SAME interface and is free to answer differently --
/// which is what `hostile_target.rs` proves, and what would be vacuous if the
/// interface plan were a GLSL plan under another name.
#[derive(Clone, Debug, Default)]
pub struct ShaderAbiPlan {
    /// Each std140 block with its BINDING INDEX, in binding order.
    ///
    /// The index comes from the group's ROLE, not from position: `@engine` is
    /// 0, the vertex `@uniform` group 1, the fragment group 2. Position was the
    /// index until the engine block landed, which made 0 mean "engine block"
    /// only for programs that happened to have one -- reserving the slot is
    /// what makes "binding 0 is the engine's" enforceable.
    ///
    /// The single authority for each block's bytes: the GLSL declaration, the
    /// CPU packer and the link-time driver assertion all read it.
    pub uniform_blocks: Vec<(u32, UniformBlockPlan)>,
    /// `UniformArray<T>` blocks with their bindings -- one per stage that
    /// declares `@size(N)` params.
    ///
    /// A SEPARATE block from the stage's scalar group, for a LIFETIME reason
    /// rather than a typing one: a fragment uniform changes per DRAW while
    /// light arrays change per ACTIVATION, so one combined block would force
    /// either a large copy into every batch or partial-update state. Two blocks
    /// keep whole-block uploads and independent fingerprints.
    pub uniform_array_blocks: Vec<(u32, UniformArrayBlockPlan)>,
    /// Sampler names in UNIT order -- unit N is index N. GLES 3.0 has no
    /// `layout(binding=)`, so the engine needs each name once to point the
    /// sampler uniform at its unit at link time; after that it binds by slot.
    pub texture_units: Vec<Ustr>,
    /// Vertex attribute locations, in location order.
    pub attribute_locations: Vec<(Ustr, u32)>,
}

impl ShaderAbiPlan {
    /// GLSL's answer to `iface`.
    /// Takes NO `Workspace`. The layout is a function of `iface` alone, which
    /// is what makes a perturbed interface produce a perturbed layout instead
    /// of quietly re-deriving the declaring struct's.
    /// The binding a uniform group's ROLE takes. One oracle, so the layout and
    /// the validation below cannot disagree about where a group belongs.
    fn binding_of(role: UniformRole) -> u32 {
        match role {
            UniformRole::Engine => UNIFORM_BLOCK_BINDING_ENGINE,
            UniformRole::Stage(IrStage::Vertex) => UNIFORM_BLOCK_BINDING_VERTEX,
            UniformRole::Stage(IrStage::Fragment) => UNIFORM_BLOCK_BINDING_FRAGMENT,
        }
    }

    /// The binding a stage's array block takes.
    fn array_binding_of(stage: IrStage) -> u32 {
        match stage {
            IrStage::Vertex => UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS,
            IrStage::Fragment => UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS,
        }
    }

    pub fn glsl(
        iface: &ShaderInterfacePlan,
        span: Span,
    ) -> Result<Self, CompilerError> {
        let binding = Self::binding_of;
        // std140 over the INTERFACE's members. The Workspace is not read here:
        // a layout is a function of the member list, which is what lets a
        // perturbed interface produce a perturbed layout and a second target
        // lay the same members out its own way.
        let mut uniform_blocks: Vec<(u32, UniformBlockPlan)> = Vec::new();
        for g in &iface.uniform_groups {
            let plan = skull_mir::gpu_layout::std140_block(g.struct_name, &g.members, Some(span))
                .map_err(|e| err(span, e.message))?;
            uniform_blocks.push((binding(g.role), plan));
        }
        uniform_blocks.sort_by_key(|(b, _)| *b);

        let mut uniform_array_blocks: Vec<(u32, UniformArrayBlockPlan)> = Vec::new();
        for g in &iface.array_groups {
            let b = Self::array_binding_of(g.stage);
            uniform_array_blocks
                .push((b, uniform_array_block(array_block_name(g.owner), &g.members)?));
        }
        uniform_array_blocks.sort_by_key(|(b, _)| *b);

        // Declaration order IS unit order, which is the contract `@attr`
        // already uses for locations.
        let texture_units = iface.sampler_names();

        let mut attribute_locations = Vec::new();
        let mut loc = 0u32;
        for vi in &iface.vertex_inputs {
            for (name, kind) in &vi.attrs {
                attribute_locations.push((*name, loc));
                loc += attr_location_span(*kind);
            }
        }

        Ok(ShaderAbiPlan {
            uniform_blocks,
            uniform_array_blocks,
            texture_units,
            attribute_locations,
        })
    }

    pub fn block_at(&self, binding: u32) -> Option<&UniformBlockPlan> {
        self.uniform_blocks.iter().find(|(b, _)| *b == binding).map(|(_, p)| p)
    }
}

/// How many attribute LOCATIONS one attribute kind occupies. Every kind
/// admitted today takes one; a `mat4` attribute would take four, which is why
/// this is a function rather than an `enumerate()` index.
fn attr_location_span(_k: AttrKind) -> u32 {
    1
}

pub const UNIFORM_BLOCK_BINDING_ENGINE: u32 = 0;
pub const UNIFORM_BLOCK_BINDING_VERTEX: u32 = 1;
pub const UNIFORM_BLOCK_BINDING_FRAGMENT: u32 = 2;
pub const UNIFORM_BLOCK_BINDING_VERTEX_ARRAYS: u32 = 3;
pub const UNIFORM_BLOCK_BINDING_FRAGMENT_ARRAYS: u32 = 4;

/// The GLSL block name for a stage's array block, minted from the stage entry.
///
/// Deterministic and per-STAGE: the two entries of a pipeline always have
/// different names, so a program's two array blocks cannot collide with each
/// other. A collision with a user's `@uniform` group name is possible and is
/// rejected (`check_block_name_collisions`), never renamed around.
fn array_block_name(entry: Ustr) -> Ustr {
    Ustr::from(&format!("{entry}_Arrays"))
}

/// **One stage's validated plan**: its lowered body, the emittable helpers it
/// reaches in emission order, and the prelude flags those bodies imply.
///
/// Built from the supplied graph/view population, which performs every
/// rejection the stage's own code can produce -- lowering, the tier-0 body
/// profile, recursion, a foreign body with no GLSL mapping, a helper outside
/// tier 0 and the stage-specific builtin validation. Rendering reads it and
/// cannot reach a different verdict, because there is nothing left to decide.
struct StagePlan {
    /// The stage's concrete bodies -- target-neutral, and the only part of this
    /// plan a non-GLSL backend would want.
    ir: ShaderStageIr,
    /// GLSL-only: whether the reachable bodies need the int div/mod prelude.
    /// GLSL has no integer `/` and `%` with Rock's semantics, which is a
    /// property of this target and not of the bodies.
    uses_idiv: bool,
    uses_imod: bool,
    dependencies: Vec<FuncRef>,
}

impl StagePlan {
    /// The helpers this stage emits: the int-div/mod prelude the reachable
    /// bodies need, then each helper definition in emission order.
    fn render_helpers(
        &self,
        ws: &Workspace,
        interner: &MonoInterner,
    ) -> Result<String, CompilerError> {
        let mut out = String::new();
        let prelude = helper_prelude(self.uses_idiv, self.uses_imod);
        if !prelude.is_empty() {
            out.push_str(&prelude);
            out.push('\n');
        }
        for href in &self.ir.order {
            out.push_str(&emit_fn_def(ws, interner, href, &self.ir.bodies[href])?);
            out.push('\n');
        }
        Ok(out)
    }

    /// The stage's helpers plus the stage entry itself -- the hot-reload
    /// dependency set. DECLARATIONS: a reload watches source files, and two
    /// instances of one generic helper share one.
    fn dependencies(&self) -> Vec<FuncRef> {
        self.dependencies.clone()
    }
}

fn plan_stage_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderStageView,
    expected: Stage,
    concrete_plans: &ConcretePlans,
) -> Result<StagePlan, CompilerError> {
    let expected_ir = match expected {
        Stage::Vertex => ShaderStage::Vertex,
        Stage::Fragment => ShaderStage::Fragment,
    };
    if view.stage != expected_ir {
        return Err(CompilerError::new_no_location(format!(
            "shader graph/view stage mismatch: expected {expected_ir:?}, got {:?}",
            view.stage
        )));
    }
    if view.order.iter().any(|instance| instance == &view.entry)
        || view.order.len() != view.helpers.len()
        || view.order.iter().cloned().collect::<std::collections::BTreeSet<_>>() != view.helpers
    {
        return Err(CompilerError::new_no_location(
            "shader graph/view helper order is not the retained helper population".to_string(),
        ));
    }
    let mut expected_retained = view.helpers.clone();
    expected_retained.insert(view.entry.clone());
    if expected_retained != view.retained {
        return Err(CompilerError::new_no_location(
            "shader graph/view retained population is not entry plus helpers".to_string(),
        ));
    }

    let entry = ShaderInstance::new(view.entry.clone());
    let entry_f = &ws.module(entry.fref().module).functions[entry.fref().func];
    let entry_plan = concrete_plans.get(&view.entry).ok_or_else(|| {
        err(
            entry_f.name_span,
            format!("shader profile: no concrete plan for `{}`", entry_f.name),
        )
    })?;
    let entry_body = lower_instance_with_plan(ws, scx, entry_plan).map_err(|e| {
        err(
            entry_f.name_span,
            format!(
                "shader profile: `{}` cannot lower for GLSL: {}",
                entry_f.name, e.message
            ),
        )
    })?;
    let mut entry_callees = Vec::new();
    profile_body(ws, entry_f, &entry_body, &mut entry_callees)?;

    let mut allowed: HashSet<ShaderInstance> = HashSet::default();
    allowed.insert(entry.clone());
    allowed.extend(view.order.iter().cloned().map(ShaderInstance::new));
    let mut discovered_builtins: HashSet<FuncRef> = HashSet::default();
    let entry_calls = checked_view_callees(
        ws,
        &entry,
        entry_callees,
        &allowed,
        scx.interner(),
        &mut discovered_builtins,
    )?;
    let mut calls: HashMap<ShaderInstance, Vec<ShaderInstance>> = HashMap::default();
    calls.insert(entry.clone(), entry_calls);
    let mut bodies: HashMap<ShaderInstance, MirFunc> = HashMap::default();
    for raw in &view.order {
        let helper = ShaderInstance::new(raw.clone());
        let f = &ws.module(helper.fref().module).functions[helper.fref().func];
        let subst = helper.subst(ws, scx.interner());
        check_tier0_signature_spec(ws, &subst, f)?;
        let helper_plan = concrete_plans.get(raw).ok_or_else(|| {
            err(f.name_span, format!("shader profile: no concrete plan for `{}`", f.name))
        })?;
        let body = lower_instance_with_plan(ws, scx, helper_plan).map_err(|e| {
            err(
                f.name_span,
                format!("shader profile: `{}` cannot lower for GLSL: {}", f.name, e.message),
            )
        })?;
        let mut callees = Vec::new();
        profile_body(ws, f, &body, &mut callees)?;
        calls.insert(
            helper.clone(),
            checked_view_callees(
                ws,
                &helper,
                callees,
                &allowed,
                scx.interner(),
                &mut discovered_builtins,
            )?,
        );
        bodies.insert(helper, body);
    }
    let expected_builtins: HashSet<_> = view.builtins.iter().map(|instance| instance.func).collect();
    if discovered_builtins != expected_builtins {
        return Err(CompilerError::new_no_location(
            "shader graph/view builtin declaration population differs from lowered calls"
                .to_string(),
        ));
    }
    reject_population_cycles(ws, &calls)?;
    reject_stage_violations(
        ws,
        expected,
        bodies.values().chain(std::iter::once(&entry_body)),
        entry_f.name_span,
    )?;

    let (mut uses_idiv, mut uses_imod) = (false, false);
    for body in bodies.values().chain(std::iter::once(&entry_body)) {
        for (_, inst) in body.insts.iter() {
            match inst {
                Inst::IntDiv(..) => uses_idiv = true,
                Inst::IntMod(..) => uses_imod = true,
                _ => {}
            }
        }
    }
    let order: Vec<ShaderInstance> = view.order.iter().cloned().map(ShaderInstance::new).collect();
    let ir_stage = match expected {
        Stage::Vertex => IrStage::Vertex,
        Stage::Fragment => IrStage::Fragment,
    };
    let ir = ShaderStageIr { stage: ir_stage, entry, body: entry_body, order, bodies };
    if ir.declarations() != view.dependencies {
        return Err(CompilerError::new_no_location(
            "shader graph/view hot-reload dependencies differ from lowered order".to_string(),
        ));
    }
    Ok(StagePlan { ir, uses_idiv, uses_imod, dependencies: view.dependencies.clone() })
}

fn checked_view_callees(
    ws: &Workspace,
    caller: &ShaderInstance,
    callees: Vec<ShaderInstance>,
    allowed: &HashSet<ShaderInstance>,
    interner: &MonoInterner,
    builtins: &mut HashSet<FuncRef>,
) -> Result<Vec<ShaderInstance>, CompilerError> {
    let caller_f = &ws.module(caller.fref().module).functions[caller.fref().func];
    let mut retained = Vec::new();
    for callee in callees {
        let f = &ws.module(callee.fref().module).functions[callee.fref().func];
        if renders_inline_instance(ws, f, &callee.inst, interner) {
            builtins.insert(callee.fref());
            continue;
        }
        if is_lowered_native_operator(ws, f, &callee.inst, interner) {
            continue;
        }
        if f.is_extern || f.inline_body.is_some() {
            return Err(err(
                f.name_span,
                format!(
                    "shader profile: `{}` has no emittable body and no GLSL intrinsic mapping",
                    f.name
                ),
            ));
        }
        if !allowed.contains(&callee) {
            return Err(err(
                caller_f.name_span,
                format!(
                    "shader graph/view omitted the concrete callee `{}` reached by `{}`",
                    f.name, caller_f.name
                ),
            ));
        }
        retained.push(callee);
    }
    Ok(retained)
}

fn reject_population_cycles(
    ws: &Workspace,
    calls: &HashMap<ShaderInstance, Vec<ShaderInstance>>,
) -> Result<(), CompilerError> {
    fn visit(
        ws: &Workspace,
        node: &ShaderInstance,
        calls: &HashMap<ShaderInstance, Vec<ShaderInstance>>,
        active: &mut HashSet<ShaderInstance>,
        done: &mut HashSet<ShaderInstance>,
    ) -> Result<(), CompilerError> {
        if active.contains(node) {
            let f = &ws.module(node.fref().module).functions[node.fref().func];
            return Err(err(
                f.name_span,
                format!("shader profile: recursion through `{}` is not allowed on GLSL", f.name),
            ));
        }
        if !done.insert(node.clone()) {
            return Ok(());
        }
        active.insert(node.clone());
        if let Some(callees) = calls.get(node) {
            for callee in callees {
                visit(ws, callee, calls, active, done)?;
            }
        }
        active.remove(node);
        Ok(())
    }

    let mut active = HashSet::default();
    let mut done = HashSet::default();
    for node in calls.keys() {
        visit(ws, node, calls, &mut active, &mut done)?;
    }
    Ok(())
}

/// **The one validated plan for a single-entry (A1) shader program.**
///
/// Everything emission decides before it writes GLSL, in emission's own order:
/// the stage classification of the entry's signature, its lowering and tier-0
/// body pass over the graph/view population, the stage-builtin validation, the
/// `@uniform` block's std140 layout and the program's `@textures` slot
/// assignment.
///
/// [`check_entry_profile`] builds this and DROPS it; [`emit_shader_entry`]
/// builds it and renders it. There is no second sequence, so the mono shader
/// artifact view structurally cannot admit a program emission refuses -- which
/// is not what a summary of these checks could promise, and did not deliver: it
/// accepted a standalone `@shader(vertex)` entry and never reached the
/// layout/slot planning at all.
struct EntryPlan {
    fref: FuncRef,
    shape: EntryShape,
    stage: StagePlan,
    /// What the program's interface MEANS -- roles, names, order, neutral
    /// kinds. Target-neutral, and the sole input to `abi`.
    interface: ShaderInterfacePlan,
    /// GLSL's physical answer to `interface`.
    abi: ShaderAbiPlan,
}

fn plan_shader_entry_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderStageView,
    concrete_plans: &ConcretePlans,
) -> Result<EntryPlan, CompilerError> {
    plan_shader_entry_inner(ws, scx, view.entry.func, view, concrete_plans)
}

fn plan_shader_entry_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    fref: FuncRef,
    stage_view: &ShaderStageView,
    concrete_plans: &ConcretePlans,
) -> Result<EntryPlan, CompilerError> {
    let f = &ws.module(fref.module).functions[fref.func];
    match shader_stage(f) {
        Some("fragment") => {}
        Some(stage) => {
            return Err(err(f.name_span, format!("shader entry: @shader({stage}) is not supported yet -- only @shader(fragment) (vertex entries are A2)")));
        }
        None => {
            return Err(err(f.name_span, format!("shader entry: `{}` has no @shader(fragment) annotation", f.name)));
        }
    }
    // The entry's signature is validated by its STAGE classification, never by
    // the fn-library tier-0 leaf check: an `@uniform` block param is legal here
    // and is not a tier-0 leaf (asking the library check of an entry rejected
    // 1,539 real shaders).
    let class = classify_entry(ws, f)?;
    let stage = plan_stage_from_view(ws, scx, stage_view, Stage::Fragment, concrete_plans)?;
    let interface = ShaderInterfacePlan {
        stages: vec![IrStage::Fragment],
        uniform_groups: class
            .uniform_group
            .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Fragment), g, f.name_span))
            .transpose()?
            .into_iter()
            .collect(),
        textures: logical_textures(
            ws,
            class.texture_group,
            has_bare_sampler(&class.samplers, &class.global_params),
            vec![IrStage::Fragment],
            f.name_span,
        )?,
        bare_samplers: bare_sampler_list(
            &class.samplers,
            &class.global_params,
            IrStage::Fragment,
        ),
        // A1's entry returns one colour; MRT is an A2 feature.
        fragment_output: FragmentOutput::Single(WireKind::F32x4),
        // A1 has no vertex stage of its own (it rides the fullscreen quad), and
        // `classify_entry` refuses a `UniformArray` param outright -- arrays are
        // an A2 feature, so these are absences rather than omissions.
        ..Default::default()
    };
    check_entry_collisions(&interface, f.name_span)?;
    let abi = ShaderAbiPlan::glsl(&interface, f.name_span)?;
    let shape = entry_shape(&class, &interface);
    Ok(EntryPlan { fref, shape, stage, interface, abi })
}

/// Validate one shader ENTRY exactly as emission does, without emitting.
///
/// Published for the mono SHADER ARTIFACT VIEW, which must not be able to admit
/// what emission refuses. It is the view-fed entry plan used by emission,
/// not a summary of it: the only thing left out below this point is the GLSL
/// text itself.
///
/// Declaration-based, like the rest of GLSL lowering today -- `lower_function`
/// takes no substitution -- so a caller holding concrete instances must not
/// hand this a generic declaration and read the answer as concrete.
pub fn check_entry_profile(ws: &Workspace, fref: FuncRef) -> Result<(), CompilerError> {
    let session = shader_program_session(ws, ShaderProgram { vert: None, frag: fref })?;
    check_entry_profile_with_view(ws, &session.scx, &session.view)
}

/// Validate one shader entry against the graph-owned concrete population.
pub fn check_entry_profile_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<(), CompilerError> {
    let stage = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no fragment stage for a single-entry shader".to_string(),
            )
        })?;
    plan_shader_entry_from_view(ws, scx, stage, &view.concrete_plans).map(|_| ())
}

pub fn emit_shader_entry_with_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let fref = resolve_root(ws, mid, root)?;
    let session = shader_program_session(ws, ShaderProgram { vert: None, frag: fref })?;
    emit_shader_entry_with_view(ws, &session.scx, &session.view, profile)
}

/// Emit a single-entry program from the graph-owned population. The caller's
/// `ScalarCtx` must share the graph interner; the view supplies the concrete
/// entry and helper order, while this backend supplies only MIR delivery and
/// GLSL rendering.
pub fn emit_shader_entry_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    if view.program.vert.is_some() {
        return Err(CompilerError::new_no_location(
            "shader graph/view pipeline passed to single-entry GLSL emission".to_string(),
        ));
    }
    let stage = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no fragment stage for a single-entry shader".to_string(),
            )
        })?;
    let plan = plan_shader_entry_from_view(ws, scx, stage, &view.concrete_plans)?;
    render_shader_entry(ws, scx.interner(), &plan, profile)
}

fn render_shader_entry(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &EntryPlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let f = &ws.module(plan.fref.module).functions[plan.fref.func];
    let shape = &plan.shape;

    // A1 goes through the SAME resolver A2 does. Rendering every ABI block
    // directly would emit one the interface never claimed, which is exactly
    // what the program-wide validation exists to refuse.
    let program =
        ProgramAbi::of(&plan.interface, &plan.abi, &[IrStage::Fragment], f.name_span)?;
    let resolved = program.stage(IrStage::Fragment)?;

    let mut px = String::new();
    px.push_str("in vec2 TexCoord;\nout vec4 FragColor;\n\n");
    if let Some(block) = resolved.engine_block {
        px.push_str(&uniform_block_decl(block));
    }
    if let Some(block) = resolved.uniform_block {
        px.push_str(&uniform_block_decl(block));
    }
    // Samplers are opaque: GLSL forbids them inside a uniform block, so they
    // stay loose beside it and bind by unit. Declared from the INTERFACE -- the
    // grouped ones in the ABI's unit order, then the bare ones.
    for (n, k) in &resolved.samplers {
        px.push_str(&format!("uniform {} {};\n", sampler_glsl_ty(*k).uniform_decl(), n));
    }
    px.push('\n');
    px.push_str(&plan.stage.render_helpers(ws, interner)?);
    px.push_str(&emit_fn_def_with(ws, interner, &plan.stage.ir.entry, &plan.stage.ir.body, Some(shape))?);
    px.push('\n');
    px.push_str(&format!("void main() {{\n  FragColor = {}(TexCoord);\n}}\n", f.name));

    // Both stages carry the profile's precision preamble: ES requires an
    // explicit fragment float precision, and the vertex declarations are
    // harmless no-ops (highp is already the vertex default).
    let (version, precision) = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef PIXEL\n\n{precision}{px}\n#endif\n\n#ifdef VERTEX\n\n{precision}{FULLSCREEN_VERTEX}\n#endif\n"
    );
    Ok(ShaderEmission {
        source,
        dependencies: plan.stage.dependencies(),
        interface: plan.interface.clone(),
        abi: plan.abi.clone(),
        ir: ShaderIR { stages: vec![plan.stage.ir.clone()] },
    })
}


// --- A2: vertex+fragment pipeline (docs/shaders.md) ---------------------

/// The vertex stage: which shader stage a reachability walk is emitting (drives
/// per-stage builtin gating).
#[derive(Copy, Clone, PartialEq, Eq)]
enum Stage {
    Vertex,
    Fragment,
}

/// A classified `@shader(vertex)` entry.
struct VertexShape {
    /// Attribute struct param indices, in location-assignment order (per-vertex
    /// `@attr` first, then `@instance` -- the numbering continues).
    attr_params: Vec<usize>,
    /// The attribute structs in location-assignment order, in the NEUTRAL
    /// vocabulary. THE classification -- a GLSL-typed copy with baked locations
    /// stood beside it and was free to disagree with the ABI plan about both.
    /// Locations come from `ShaderAbiPlan::attribute_locations`; the spelling
    /// from `attr_glsl_ty` at the point of emission.
    inputs: Vec<LogicalVertexInput>,
    /// `@uniform` struct param indices (at most one).
    uniform_params: Vec<usize>,
    /// The `@uniform` group's struct name, if the stage declares one. Each
    /// stage gets its own std140 block at its own binding index -- the stages
    /// declare DIFFERENT types and cannot be made to share one.
    uniform_group: Option<Ustr>,
    /// `@engine` struct param indices (at most one).
    engine_params: Vec<usize>,
    /// The `@engine` group's struct name, if the stage declares one. A separate
    /// ROLE from `@uniform`, not a second one of them: the engine produces it,
    /// its lifetime is the pass rather than the draw, and it binds at index 0 so
    /// both stages of a program name the same block.
    engine_group: Option<Ustr>,
    /// `@textures` group param indices -- see the field of the same name on
    /// `FragShape`. Vertex texture fetch is legal in GL 3.3 / GLES 3.0.
    texture_params: Vec<usize>,
    /// The `@textures` group's struct name, if the stage declares one.
    texture_group: Option<Ustr>,
    /// Sampler params (rare in vertex, but legal).
    samplers: Vec<(usize, Ustr, SamplerKind)>,
    /// `UniformArray<T>` params, declaration order -- the array block's members.
    arrays: Vec<ArrayParam>,
    /// The varyings value-struct return type.
    varyings_ty: TyId,
}

/// A classified varyings value struct.
struct VaryingsShape {
    /// Varyings EXCLUDING `position`, in the NEUTRAL vocabulary. This IS the
    /// interface's list -- not a GLSL-typed copy of it -- so there is nothing
    /// for the two to disagree about.
    varyings: Vec<LogicalVarying>,
}

/// A classified pipeline `@shader(fragment)` entry (varyings-consuming form).
struct FragShape {
    /// The varyings-struct param index (always 0 -- the first param).
    varyings_param: usize,
    uniform_params: Vec<usize>,
    /// The `@uniform` group's struct name, if the stage declares one. Its own
    /// std140 block, at its own binding index -- see `VertexShape`.
    uniform_group: Option<Ustr>,
    /// `@engine` struct param indices (at most one) -- see `VertexShape`.
    engine_params: Vec<usize>,
    /// The `@engine` group's struct name, if the stage declares one.
    engine_group: Option<Ustr>,
    /// `@textures` group param indices. Their fields are already expanded into
    /// `samplers`; the params themselves render as bare globals like a uniform
    /// struct, so they belong in `global_params`.
    texture_params: Vec<usize>,
    /// The `@textures` group's struct name, if the stage declares one.
    texture_group: Option<Ustr>,
    samplers: Vec<(usize, Ustr, SamplerKind)>,
    /// `UniformArray<T>` params, declaration order -- the array block's members.
    arrays: Vec<ArrayParam>,
    /// How the stage's colour output is shaped, in the NEUTRAL vocabulary:
    /// `Mrt` when the entry returns a value struct (one
    /// `layout(location=N) out <ty> <field>` per field, declaration order),
    /// `Single` for the implicit `FragColor` (`I32` for an R32I-only target).
    /// This IS the interface's answer, not a GLSL-typed copy of it.
    output: FragmentOutput,
}

fn has_anno(annos: &[bone_semantic::hir::Annotation], name: &str) -> bool {
    annos.iter().any(|a| a.name.name.as_str() == name)
}

/// The int argument of an `@name(N)` annotation, if present (arity/type are
/// registry-validated upstream).
fn anno_int(annos: &[bone_semantic::hir::Annotation], name: &str) -> Option<i64> {
    annos
        .iter()
        .find(|a| a.name.name.as_str() == name)
        .and_then(|a| a.args.first())
        .and_then(|arg| match arg.value {
            AnnoValue::Int(n) => Some(n),
            _ => None,
        })
}

/// The element WIRE KIND of a `UniformArray<T>`-typed param, or `None` when the
/// type is not the uniform-array phantom.
///
/// A wire kind and not a [`GlslTy`], because the array block's DECLARATION is
/// rendered from the layout plan: classifying the element here as a GLSL type
/// too would make the emitter a second authority on what an array holds, free to
/// disagree with the one that computed the offsets. Which kinds are ADMITTED is
/// likewise `uniform_array_block`'s answer, not this function's -- it rejects
/// `Vec3`/`Mat4`/`bool` with a span, so re-stating the set here would be the same
/// drift one level down.
fn uniform_array_elem(ws: &Workspace, ty: TyId, span: Span) -> Result<Option<WireKind>, CompilerError> {
    let Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), params }) = ty_expr(ws, ty) else {
        return Ok(None);
    };
    if n.as_str() != "UniformArray" {
        return Ok(None);
    }
    let Some(first) = params.into_iter().next() else {
        return Err(err(span, "shader: UniformArray needs an element type argument"));
    };
    let ety = ty.with_idx(first);
    if let Some(k) = skull_mir::gpu_layout::wire_kind(ws, ety) {
        return Ok(Some(k));
    }
    // The element has no std140 form. Naming it needs its GLSL spelling, and
    // when THAT fails too the right diagnostic is the one `glsl_ty` produced --
    // it knows why the type is unusable, and this function does not. Replacing
    // it with a fabricated `?` would discard the compiler's own information at
    // exactly the point the author needs it.
    let name = glsl_ty(ws, &SpecSubst::default(), ety, span, "uniform-array element")?;
    Err(err(span, format!(
        "shader: UniformArray element type `{}` has no std140 form (allowed: float, int, Vec2, Vec4)",
        name.name()
    )))
}

/// One classified `UniformArray<T>` param: where it sits in the entry's
/// signature (so it can be dropped from the emitted one) and its DECLARATION for
/// the layout planner.
struct ArrayParam {
    param: usize,
    decl: ArrayMemberDecl,
}

/// Classify param `i` as an array member, or `None` when it is not one.
///
/// The `@size` ANNOTATION's span travels on the decl and is what every layout
/// rejection is reported at. `ParamDecl` carries no span of its own (that is
/// bone's, and off limits), and the annotation is both present on every array
/// param by construction and the exact thing an author edits to fix a capacity
/// or budget rejection.
fn classify_array_param(
    ws: &Workspace,
    pd: &bone_semantic::hir::ParamDecl,
    i: usize,
    span: Span,
    stage: &str,
) -> Result<Option<ArrayParam>, CompilerError> {
    let Some(elem) = uniform_array_elem(ws, pd.ty, span)? else { return Ok(None) };
    let anno = pd.annotations.iter().find(|a| a.name.name.as_str() == "size").ok_or_else(|| {
        err(span, format!(
            "shader {stage}: uniform-array param `{}` needs @size(N) (the declared GLSL array capacity)",
            pd.name
        ))
    })?;
    let n = anno_int(&pd.annotations, "size").expect("@size arity/type is registry-validated");
    // `N` is source-controlled, so it reaches the layout as a `u32` only through
    // a rejection: a negative or absurd literal must not wrap into a small
    // capacity that then passes the very budget check it should fail.
    let capacity = u32::try_from(n).map_err(|_| {
        CompilerError::new_with_label(anno.span, format!(
            "shader {stage}: uniform-array param `{}` has @size({n}), which is not a usable element count",
            pd.name
        ))
    })?;
    Ok(Some(ArrayParam {
        param: i,
        decl: ArrayMemberDecl { name: pd.name, elem, capacity, span: anno.span },
    }))
}

/// The `layout(std140) uniform <Blk> { T name[N]; .. };` declaration for an
/// array block.
///
/// Every line comes from the PLAN -- element kind through [`wire_glsl_ty`], the
/// same function the scalar declaration uses, and capacity from the member the
/// offsets were computed for. The emitter deliberately holds no `GlslTy` for an
/// array element: a second classification of the same param is exactly how a
/// declaration and a layout drift into disagreeing, and "one layout authority"
/// would stop being true the moment it existed.
fn uniform_array_block_decl(plan: &UniformArrayBlockPlan) -> String {
    let mut s = format!("layout(std140) uniform {} {{\n", plan.block_name);
    for m in &plan.members {
        s.push_str(&format!("    {} {}[{}];\n", wire_glsl_ty(m.elem), m.name, m.capacity));
    }
    s.push_str("};\n");
    s
}

/// The [`StructDef`] behind a nominal type, if any.
fn struct_def_of(ws: &Workspace, ty: TyId) -> Option<&StructDef> {
    let sname = nominal_name(ws, ty)?;
    let &sid = ws.struct_by_name.get(&sname)?;
    Some(&ws.modules[sid.module.index()].structs[sid.def])
}

/// A block member's GLSL type spelling. The one function a WGSL or Metal
/// emitter rewrites for the uniform lane, the twin of [`attr_glsl_ty`].
fn wire_glsl_ty(k: WireKind) -> &'static str {
    match k {
        WireKind::F32 => "float",
        WireKind::F32x2 => "vec2",
        WireKind::F32x3 => "vec3",
        WireKind::F32x4 => "vec4",
        WireKind::I32 => "int",
        WireKind::Bool32 => "bool",
        WireKind::Mat4 => "mat4",
    }
}

/// The `layout(std140) uniform <Blk> { .. };` declaration for a group.
///
/// Declared with NO instance name, which is what keeps this change confined to
/// declaration lines: an unnamed-instance block's members are bare GLSL globals,
/// so every body reading them is byte-identical to the loose form.
///
/// Members come from the PLAN, not from a second walk of the struct -- the same
/// single-oracle rule `VertexShape.attrs` follows for attributes. The driver
/// resolves members by name, so the names here and the offsets the packer writes
/// must come from one place or they can drift silently.
fn uniform_block_decl(plan: &UniformBlockPlan) -> String {
    let mut s = format!("layout(std140) uniform {} {{\n", plan.struct_name);
    for m in &plan.members {
        s.push_str(&format!("    {} {};\n", wire_glsl_ty(m.kind), m.name));
    }
    s.push_str("};\n");
    s
}

/// Attribute/uniform fields of a struct param, GLSL-typed (a sampler-typed field
/// rejects -- samplers are params, not struct fields).
fn struct_gl_fields(ws: &Workspace, ty: TyId, span: Span, what: &str) -> Result<Vec<(Ustr, GlslTy)>, CompilerError> {
    let sd = struct_def_of(ws, ty)
        .ok_or_else(|| err(span, format!("shader: {what} must be a value struct")))?;
    let mut out = Vec::new();
    for fd in &sd.fields {
        let fty = glsl_ty(ws, &SpecSubst::default(), fd.ty, span, &format!("{what} field `{}`", fd.name))?;
        if fty.is_sampler() {
            return Err(err(span, format!("shader: {what} field `{}` cannot be a sampler -- pass samplers as fn params", fd.name)));
        }
        if fty == GlslTy::Bvec3 {
            return Err(err(span, format!("shader: {what} field `{}` cannot be a bvec3 -- bool-vector {what}s are unsupported (bvec3 is local/temp only)", fd.name)));
        }
        out.push((fd.name, fty));
    }
    Ok(out)
}

/// Samplers that are NOT part of a `@textures` group -- bare params, still
/// bound by name and a hand-assigned unit on the CPU side.
fn has_bare_sampler(samplers: &[(usize, Ustr, SamplerKind)], texture_params: &[usize]) -> bool {
    samplers.iter().any(|(i, _, _)| !texture_params.contains(i))
}

/// How this backend spells a `SamplerKind`.
/// The neutral kind of a tier-0 sampler type. The inverse of
/// [`sampler_glsl_ty`], and the point where a declared sampler param leaves
/// GLSL's vocabulary for the interface's.
fn sampler_kind_of(t: GlslTy) -> SamplerKind {
    match t {
        GlslTy::ISampler2D => SamplerKind::Int,
        _ => SamplerKind::Float,
    }
}

fn sampler_glsl_ty(k: SamplerKind) -> GlslTy {
    match k {
        SamplerKind::Float => GlslTy::Sampler2D,
        SamplerKind::Int => GlslTy::ISampler2D,
    }
}

/// A `@textures` group param expands to one sampler declaration per field, at
/// the unit the artifact assigned from field order. Every entry carries the
/// GROUP's param index: the param itself is dropped from the GLSL signature and
/// its field reads render as bare globals, exactly like a `@uniform` struct.
fn texture_group_samplers(
    ws: &Workspace,
    ty: TyId,
    param_index: usize,
    span: Span,
    stage: &str,
) -> Result<Vec<(usize, Ustr, SamplerKind)>, CompilerError> {
    let sname = nominal_name(ws, ty)
        .ok_or_else(|| err(span, format!("shader {stage}: an @textures param must be a struct")))?;
    let ts = texture_slots(ws, sname).map_err(|e| err(span, e.message))?;
    Ok(ts.slots.iter().map(|s| (param_index, s.name, s.kind)).collect())
}

/// Every param role, so a check that means "all the others" cannot go stale by
/// listing them. `reject_conflicting_texture_role` enumerated the three that
/// existed when it was written, and adding `@engine` silently widened what it
/// let through.
const PARAM_ROLES: [&str; 4] = ["attr", "instance", "uniform", "engine"];

/// `@textures` is exclusive with the other param roles -- a group is a binding
/// set of its own, not attribute or uniform-block data.
fn reject_conflicting_texture_role(
    annos: &[bone_semantic::hir::Annotation],
    name: Ustr,
    span: Span,
    stage: &str,
) -> Result<(), CompilerError> {
    for other in PARAM_ROLES.iter().copied() {
        if has_anno(annos, other) {
            return Err(err(
                span,
                format!("shader {stage}: param `{name}` has conflicting role markers (@textures and @{other})"),
            ));
        }
    }
    Ok(())
}

/// How this backend spells an `AttrKind`. The artifact stays target-neutral
/// (kind + arity); GLSL names live here, and a WGSL or Metal emitter writes its
/// own version of this one function.
fn attr_glsl_ty(k: AttrKind) -> GlslTy {
    match k {
        AttrKind::F32 => GlslTy::Float,
        AttrKind::I32 => GlslTy::Int,
        AttrKind::F32x2 => GlslTy::Vec2,
        AttrKind::F32x3 => GlslTy::Vec3,
        AttrKind::F32x4 => GlslTy::Vec4,
    }
}

/// Classify a `@shader(vertex)` entry: value-struct params carry a role marker
/// (`@attr` / `@instance` / `@uniform`); samplers are type-distinguished; the
/// return type is the varyings struct.
fn classify_vertex(ws: &Workspace, f: &Function) -> Result<VertexShape, CompilerError> {
    let span = f.name_span;
    let (mut per_vertex, mut instanced, mut uniform_params, mut samplers, mut arrays) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut texture_params = Vec::new();
    let mut texture_group: Option<Ustr> = None;
    let mut uniform_group: Option<Ustr> = None;
    let mut engine_group: Option<Ustr> = None;
    let mut engine_params: Vec<usize> = Vec::new();
    for (i, pd) in f.param_decls.iter().enumerate() {
        if let Some(a) = classify_array_param(ws, pd, i, span, "vertex")? {
            arrays.push(a);
            continue;
        }
        if has_anno(&pd.annotations, "textures") {
            reject_conflicting_texture_role(&pd.annotations, pd.name, span, "vertex")?;
            if texture_group.is_some() {
                return Err(err(span, "shader vertex: at most one @textures group param is allowed -- one group is the binding set"));
            }
            texture_group = nominal_name(ws, pd.ty);
            samplers.extend(texture_group_samplers(ws, pd.ty, i, span, "vertex")?);
            texture_params.push(i);
            continue;
        }
        match glsl_ty(ws, &SpecSubst::default(), pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, sampler_kind_of(t))),
            Ok(other) => {
                return Err(err(span, format!(
                    "shader vertex: param `{}` ({}) must be an @attr/@instance/@uniform value struct or a Sampler2D",
                    pd.name, other.name()
                )));
            }
            Err(_) => match (has_anno(&pd.annotations, "attr"), has_anno(&pd.annotations, "instance"), has_anno(&pd.annotations, "uniform"), has_anno(&pd.annotations, "engine")) {
                (true, false, false, false) => per_vertex.push(i),
                (false, true, false, false) => instanced.push(i),
                (false, false, true, false) => {
                    uniform_group = nominal_name(ws, pd.ty);
                    uniform_params.push(i);
                }
                (false, false, false, true) => {
                    engine_group = nominal_name(ws, pd.ty);
                    engine_params.push(i);
                }
                (false, false, false, false) => {
                    return Err(err(span, format!(
                        "shader vertex: value-struct param `{}` needs a role marker -- @attr (per-vertex attributes), @instance (per-instance attributes), @uniform (this shader's uniform block), or @engine (the engine-owned frame/pass block)",
                        pd.name
                    )));
                }
                _ => return Err(err(span, format!("shader vertex: param `{}` has conflicting role markers", pd.name))),
            },
        }
    }
    if uniform_params.len() > 1 {
        return Err(err(span, "shader vertex: at most one @uniform struct param is allowed"));
    }
    if engine_params.len() > 1 {
        return Err(err(span, "shader vertex: at most one @engine struct param is allowed -- it is one engine-owned block"));
    }
    let mut attr_params = Vec::new();
    let mut inputs: Vec<LogicalVertexInput> = Vec::new();
    // @attr params first, then @instance -- the ORDER locations are assigned
    // in, which is all the classification owns; the numbers are the ABI's.
    let instanced_from = per_vertex.len();
    for (n, &i) in per_vertex.iter().chain(instanced.iter()).enumerate() {
        attr_params.push(i);
        // GLSL forbids `in bool` -- bool is legal only as a uniform. A GLSL
        // legality gate, not a source of attribute data: the artifact would
        // reject bool too, but only as "not a vertex-attribute type".
        let gl_fields = struct_gl_fields(ws, f.param_decls[i].ty, span, "attribute")?;
        if let Some((n, _)) = gl_fields.iter().find(|&&(_, t)| t == GlslTy::Bool) {
            return Err(err(span, format!(
                "shader vertex: attribute `{n}` cannot be bool (GLSL forbids bool vertex inputs) -- use int or float"
            )));
        }
        let sname = nominal_name(ws, f.param_decls[i].ty)
            .ok_or_else(|| err(span, "shader vertex: attribute param must be a value struct"))?;
        let va = vertex_attrs(ws, sname).map_err(|e| err(span, e.message))?;
        inputs.push(LogicalVertexInput {
            struct_name: sname,
            rate: if n < instanced_from { VertexRate::PerVertex } else { VertexRate::PerInstance },
            attrs: va.attrs.iter().map(|a| (a.name, a.kind)).collect(),
        });
    }
    let varyings_ty = f
        .return_ty
        .ok_or_else(|| err(span, "shader vertex: entry must return a varyings value struct"))?;
    Ok(VertexShape {
        uniform_group,
        engine_params,
        engine_group,
        attr_params,
        inputs,
        uniform_params,
        texture_params,
        texture_group,
        samplers,
        arrays,
        varyings_ty,
    })
}

/// Classify the varyings struct: a required `position: Vec4` field (-> gl_Position,
/// suppressed as a varying), plus the other fields as varyings. An int-typed
/// varying without `@flat` rejects (GLSL mandates flat integer interpolation).
fn classify_varyings(ws: &Workspace, ty: TyId, span: Span) -> Result<VaryingsShape, CompilerError> {
    let sd = struct_def_of(ws, ty)
        .ok_or_else(|| err(span, "shader: the varyings type must be a value struct"))?;
    let mut has_position = false;
    let mut varyings = Vec::new();
    for fd in &sd.fields {
        let fty = glsl_ty(ws, &SpecSubst::default(), fd.ty, span, &format!("varying `{}`", fd.name))?;
        let kind = wire_kind_of(fty);
        if fty == GlslTy::Bvec3 {
            return Err(err(span, format!("shader: varying `{}` cannot be a bvec3 (GLSL forbids bool-vector interpolants) -- bvec3 is local/temp only", fd.name)));
        }
        if fd.name.as_str() == "position" {
            if fty != GlslTy::Vec4 {
                return Err(err(span, "shader: the varyings `position` field must be Vec4 (it becomes gl_Position)"));
            }
            has_position = true;
            continue;
        }
        let flat = has_anno(&fd.annotations, "flat");
        if fty == GlslTy::Int && !flat {
            return Err(err(span, format!(
                "shader: int-typed varying `{}` needs @flat (GLSL requires flat integer interpolation)",
                fd.name
            )));
        }
        // GLSL forbids bool in/out variables entirely (even flat).
        if fty == GlslTy::Bool {
            return Err(err(span, format!(
                "shader: varying `{}` cannot be bool (GLSL forbids bool varyings) -- use float or int + @flat",
                fd.name
            )));
        }
        varyings.push(LogicalVarying { name: fd.name, kind, flat });
    }
    if !has_position {
        return Err(err(span, "shader: the varyings struct must have a `position: Vec4` field (it becomes gl_Position)"));
    }
    Ok(VaryingsShape { varyings })
}

/// Classify a pipeline `@shader(fragment)` entry: the first param is the varyings
/// struct (its type must equal the vertex entry's return type); the rest are
/// `@uniform` structs and samplers.
fn classify_pipeline_fragment(ws: &Workspace, f: &Function, vert_varyings_ty: TyId) -> Result<FragShape, CompilerError> {
    let span = f.name_span;
    let Some(first) = f.param_decls.first() else {
        return Err(err(span, "shader fragment: the entry must take the varyings struct as its first param"));
    };
    if nominal_name(ws, first.ty).is_none() || nominal_name(ws, first.ty) != nominal_name(ws, vert_varyings_ty) {
        return Err(err(span, "shader fragment: the first param must be the varyings struct (the vertex entry's return type) -- the pair's varyings types do not match"));
    }
    let (mut uniform_params, mut samplers, mut arrays) = (Vec::new(), Vec::new(), Vec::new());
    let mut texture_params = Vec::new();
    let mut texture_group: Option<Ustr> = None;
    let mut uniform_group: Option<Ustr> = None;
    let mut engine_group: Option<Ustr> = None;
    let mut engine_params: Vec<usize> = Vec::new();
    for (i, pd) in f.param_decls.iter().enumerate().skip(1) {
        if let Some(a) = classify_array_param(ws, pd, i, span, "fragment")? {
            arrays.push(a);
            continue;
        }
        if has_anno(&pd.annotations, "textures") {
            reject_conflicting_texture_role(&pd.annotations, pd.name, span, "fragment")?;
            if texture_group.is_some() {
                return Err(err(span, "shader fragment: at most one @textures group param is allowed -- one group is the binding set"));
            }
            texture_group = nominal_name(ws, pd.ty);
            samplers.extend(texture_group_samplers(ws, pd.ty, i, span, "fragment")?);
            texture_params.push(i);
            continue;
        }
        match glsl_ty(ws, &SpecSubst::default(), pd.ty, span, "parameter") {
            Ok(t) if t.is_sampler() => samplers.push((i, pd.name, sampler_kind_of(t))),
            Ok(other) => {
                return Err(err(span, format!("shader fragment: param `{}` ({}) must be @uniform or a Sampler2D", pd.name, other.name())));
            }
            Err(_) => {
                if has_anno(&pd.annotations, "engine") {
                    if has_anno(&pd.annotations, "uniform") {
                        return Err(err(span, format!("shader fragment: param `{}` has conflicting role markers", pd.name)));
                    }
                    // The group's fields are validated where they become the
                    // interface's members (`uniform_group_members`), which is
                    // the std140 question. A `glsl_ty` pass here was a weaker
                    // restatement -- it admits Sampler2D and Bvec3, which no
                    // uniform block can pack.
                    engine_group = nominal_name(ws, pd.ty);
                    engine_params.push(i);
                } else {
                    if !has_anno(&pd.annotations, "uniform") {
                        return Err(err(span, format!("shader fragment: value-struct param `{}` needs @uniform (this shader's block) or @engine (the engine-owned frame/pass block)", pd.name)));
                    }
                    uniform_group = nominal_name(ws, pd.ty);
                    uniform_params.push(i);
                }
            }
        }
    }
    if uniform_params.len() > 1 {
        return Err(err(span, "shader fragment: at most one @uniform struct param is allowed"));
    }
    if engine_params.len() > 1 {
        return Err(err(span, "shader fragment: at most one @engine struct param is allowed -- it is one engine-owned block"));
    }
    // Return: a single Vec4 (implicit FragColor), a single int (an R32I-only
    // target, e.g. the silhouette mask), OR a value struct of Vec4/int fields
    // (MRT -- one `layout(location=N) out <ty> <field>` per field, in order;
    // int fields feed R32I attachments).
    let output = match f.return_ty.map(|rt| (rt, glsl_ty(ws, &SpecSubst::default(), rt, span, "return"))) {
        Some((_, Ok(GlslTy::Vec4))) => FragmentOutput::Single(WireKind::F32x4),
        Some((_, Ok(GlslTy::Int))) => FragmentOutput::Single(WireKind::I32),
        Some((rt, Err(_))) => {
            let sd = struct_def_of(ws, rt).ok_or_else(|| {
                err(span, "shader fragment: the return type must be Vec4 (FragColor), int (an integer id target), or a value struct of Vec4/int fields (MRT outputs)")
            })?;
            let mut outs = Vec::new();
            for fd in &sd.fields {
                let fty = glsl_ty(ws, &SpecSubst::default(), fd.ty, span, &format!("MRT output `{}`", fd.name))?;
                if fty != GlslTy::Vec4 && fty != GlslTy::Int {
                    return Err(err(span, format!(
                        "shader fragment: MRT output `{}` ({}) must be Vec4 or int (each output is a `layout(location=N) out <ty>`)",
                        fd.name, fty.name()
                    )));
                }
                outs.push((fd.name, wire_kind_of(fty)));
            }
            if outs.is_empty() {
                return Err(err(span, "shader fragment: the MRT output struct must have at least one field"));
            }
            FragmentOutput::Mrt(outs)
        }
        _ => return Err(err(span, "shader fragment: the return type must be Vec4 (FragColor), int (an integer id target), or a value struct of Vec4/int fields (MRT outputs)")),
    };
    Ok(FragShape {
        uniform_group,
        engine_params,
        engine_group,
        varyings_param: 0,
        uniform_params,
        texture_params,
        texture_group,
        samplers,
        arrays,
        output,
    })
}

/// Reject GPU builtins reached from the wrong stage: frag_coord/fwidth/discard
/// are fragment-only, vertex_id is vertex-only.
fn reject_stage_violations<'a>(
    ws: &Workspace,
    stage: Stage,
    bodies: impl Iterator<Item = &'a MirFunc>,
    span: Span,
) -> Result<(), CompilerError> {
    for mf in bodies {
        for (_, inst) in mf.insts.iter() {
            if let Inst::Call { func, .. } = inst {
                let n = ws.module(func.module).functions[func.func].name;
                match (stage, n.as_str()) {
                    (Stage::Vertex, "fwidth" | "frag_coord" | "discard") => {
                        return Err(err(span, format!("shader vertex: `{n}()` is a fragment-stage builtin, not available in the vertex stage")));
                    }
                    (Stage::Fragment, "vertex_id") => {
                        return Err(err(span, format!("shader fragment: `{n}()` is a vertex-stage builtin, not available in the fragment stage")));
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Reject a fragment-stage read of the varyings `position` field: it is
/// gl_Position (vertex-only), not a fragment input.
fn reject_position_read(ws: &Workspace, mf: &MirFunc, varyings_cell: Idx<Cell>, span: Span) -> Result<(), CompilerError> {
    let _ = ws;
    for (_, inst) in mf.insts.iter() {
        if let Inst::Field { base, field } = inst {
            if field.as_str() == "position" && matches!(mf.insts[*base], Inst::Load(c) if c == varyings_cell) {
                return Err(err(span, "shader fragment: `position` is gl_Position (vertex-only) and is not emitted as a varying -- it cannot be read in the fragment stage"));
            }
        }
    }
    Ok(())
}

fn emit_vertex_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    stage: &StagePlan,
    v: &VertexShape,
    iface: &ShaderInterfacePlan,
    program: &ProgramAbi<'_>,
) -> Result<(String, Vec<FuncRef>), CompilerError> {
    let resolved = program.stage(IrStage::Vertex)?;
    let (vert_block, engine_block, array_block) =
        (resolved.uniform_block, resolved.engine_block, resolved.array_block);
    let shape = EntryShape {
        global_params: v
            .attr_params
            .iter()
            .copied()
            .chain(v.engine_params.iter().copied())
            .chain(v.uniform_params.iter().copied())
            .chain(v.texture_params.iter().copied())
            .collect(),
        // Body-level typing of bare-global reads. Attribute types come from the
        // INTERFACE, spelled here -- `GlslTy` is the emitter's own working type
        // for body cells and does not travel back upward.
        uniform_fields: iface
            .vertex_inputs
            .iter()
            .flat_map(|i| i.attrs.iter().map(|(n, k)| (*n, attr_glsl_ty(*k))))
            .chain(stage_uniform_fields(iface, IrStage::Vertex))
            .collect(),
        sampler_params: v.samplers.iter().map(|&(i, _, _)| i).collect(),
        arrays: v.arrays.iter().map(|a| a.param).collect(),
        main_out: Some(MainOut {
            fields: iface.varyings.iter().map(|v| (v.name, wire_glsl(v.kind))).collect(),
            position_is_gl: true,
        }),
    };
    let (mf, helpers, dependencies) =
        (&stage.ir.body, stage.render_helpers(ws, interner)?, stage.dependencies());

    let mut out = String::new();
    for (n, t, loc) in &program.attributes {
        out.push_str(&format!("layout (location = {loc}) in {} {};\n", t.name(), n));
    }
    out.push('\n');
    // The engine block first: it binds at index 0 and both stages declare the
    // same one, so its declaration is identical in each.
    if let Some(plan) = engine_block {
        out.push_str(&uniform_block_decl(plan));
    }
    // No loose-uniform form: an interface group with no block has already been
    // rejected, so absence here means the stage declares no group at all.
    if let Some(plan) = vert_block {
        out.push_str(&uniform_block_decl(plan));
    }
    // The stage's `@size(N)` params, as ONE std140 block at their own reserved
    // binding. Separate from the scalar block above because their LIFETIME is:
    // arrays change per activation, the scalar group per draw.
    if let Some(plan) = array_block {
        out.push_str(&uniform_array_block_decl(plan));
    }
    // Samplers are opaque and cannot live in a uniform block, so each stage
    // declares the ones IT reads. Vertex texture fetch is legal in GL 3.3 /
    // GLES 3.0, and a stage that samples without declaring emits a reference to
    // an identifier the driver has never seen.
    for (n, t) in &resolved.samplers {
        let (n, t) = (n, *t);
        out.push_str(&format!("uniform {} {};\n", sampler_glsl_ty(t).uniform_decl(), n));
    }
    if vert_block.is_some() || engine_block.is_some() || array_block.is_some() {
        out.push('\n');
    }
    for (n, t, flat) in interface_varyings(iface) {
        let (n, t, flat) = (&n, &t, &flat);
        let q = if *flat { "flat " } else { "" };
        out.push_str(&format!("{q}out {} {};\n", t.name(), n));
    }
    out.push('\n');
    out.push_str(&helpers);
    out.push_str(&emit_fn_def_with(ws, interner, &stage.ir.entry, mf, Some(&shape))?);
    Ok((out, dependencies))
}

fn emit_fragment_stage(
    ws: &Workspace,
    interner: &MonoInterner,
    stage: &StagePlan,
    fs: &FragShape,
    iface: &ShaderInterfacePlan,
    program: &ProgramAbi<'_>,
) -> Result<(String, Vec<FuncRef>), CompilerError> {
    let span = ws.module(stage.ir.entry.fref().module).functions[stage.ir.entry.fref().func].name_span;
    let resolved = program.stage(IrStage::Fragment)?;
    let (frag_block, engine_block, array_block) =
        (resolved.uniform_block, resolved.engine_block, resolved.array_block);
    let fref = stage.ir.entry.fref();
    // MRT: the entry is emitted as `void main()` destructuring its returned
    // output struct into the `layout(location=N) out` globals (like the vertex
    // varyings path). Single-output keeps the `vec4 <fn>()` + wrapper form.
    let main_out = interface_mrt_outputs(iface).as_ref().map(|outs| MainOut {
        fields: outs.iter().copied().collect(),
        position_is_gl: false,
    });
    let shape = EntryShape {
        global_params: std::iter::once(fs.varyings_param)
            .chain(fs.engine_params.iter().copied())
            .chain(fs.uniform_params.iter().copied())
            .chain(fs.texture_params.iter().copied())
            .collect(),
        uniform_fields: iface
            .varyings
            .iter()
            .map(|v| (v.name, wire_glsl(v.kind)))
            .chain(stage_uniform_fields(iface, IrStage::Fragment))
            .collect(),
        sampler_params: fs.samplers.iter().map(|&(i, _, _)| i).collect(),
        arrays: fs.arrays.iter().map(|a| a.param).collect(),
        main_out,
    };
    let (mf, helpers, dependencies) = (&stage.ir.body, stage.render_helpers(ws, interner)?, stage.dependencies());

    let mut out = String::new();
    for (n, t, flat) in interface_varyings(iface) {
        let (n, t, flat) = (&n, &t, &flat);
        let q = if *flat { "flat " } else { "" };
        out.push_str(&format!("{q}in {} {};\n", t.name(), n));
    }
    match &interface_mrt_outputs(iface) {
        None => {
            // `None` here means the interface declares no colour output while a
            // FRAGMENT stage is being emitted -- an inconsistent plan, not a
            // shape to guess a `vec4` for.
            let FragmentOutput::Single(k) = iface.fragment_output else {
                return Err(err(span, "internal: the interface declares no fragment output, but a fragment stage is being emitted"));
            };
            out.push_str(&format!("out {} FragColor;\n\n", wire_glsl(k).name()))
        }
        Some(outs) => {
            for (loc, (n, t)) in outs.iter().enumerate() {
                out.push_str(&format!("layout(location = {loc}) out {} {n};\n", t.name()));
            }
            out.push('\n');
        }
    }
    // The engine block first -- same declaration the vertex stage emits.
    if let Some(plan) = engine_block {
        out.push_str(&uniform_block_decl(plan));
    }
    if let Some(plan) = frag_block {
        out.push_str(&uniform_block_decl(plan));
    }
    // The fragment stage's array block -- see the vertex stage.
    if let Some(plan) = array_block {
        out.push_str(&uniform_array_block_decl(plan));
    }
    for (n, t) in &resolved.samplers {
        out.push_str(&format!("uniform {} {};\n", sampler_glsl_ty(*t).uniform_decl(), n));
    }
    out.push('\n');
    out.push_str(&helpers);
    out.push_str(&emit_fn_def_with(ws, interner, &stage.ir.entry, mf, Some(&shape))?);
    // Single-output: the entry fn returns Vec4; main() forwards it to FragColor.
    // MRT: the entry IS `void main()` (destructured), so no wrapper.
    if !matches!(iface.fragment_output, FragmentOutput::Mrt(_)) {
        out.push('\n');
        out.push_str(&format!(
            "void main() {{\n  FragColor = {}();\n}}\n",
            ws.module(fref.module).functions[fref.func].name
        ));
    }
    Ok((out, dependencies))
}

/// Reject duplicate global names across BOTH stages (decision 7): attributes,
/// uniforms, varyings, samplers and the reserved gl_Position/FragColor share
/// one GLSL namespace -- the linked program has a single uniform namespace, so
/// a vertex/fragment same-name pair is a link error (or a silent alias), never
/// two distinct bindings. Varyings are one name in both stages (inserted once).
///
/// THE ENGINE BLOCK'S MEMBERS BELONG HERE TOO. An unnamed-instance block's
/// members are bare globals, so they sit in the same namespace as everything
/// above. A stage's own `@uniform` group already arrives through `uniforms`
/// whether or not it became a block, but `@engine` is a separate ROLE landing in
/// `engine_uniforms`, so it was outside this census entirely -- and a missed
/// pair is a raw driver error carrying no Rock span (`declaration of "uTime"
/// conflicts with previous declaration`), which is exactly the class this
/// exists to convert into a spanned rejection.
///
/// Taken from the PLAN rather than from both shapes' `engine_uniforms`: the
/// engine block is declared in both stages but is ONE block, so listing it per
/// stage would collide with itself.
/// Reject two interface names that would land in GLSL's ONE cross-stage
/// namespace.
///
/// Censuses the INTERFACE, which is what makes the list complete by
/// construction: every name a program declares is in there exactly once, so a
/// role added to the interface joins this check by existing rather than by
/// someone remembering to chain it.
fn check_pipeline_collisions(
    iface: &ShaderInterfacePlan,
    span: Span,
) -> Result<(), CompilerError> {
    // gl_Position is always reserved. `FragColor` is reserved only for the
    // single-output form -- with MRT the outputs are explicit named fields (one
    // of which may itself be `FragColor`), so they go through the collision check
    // like any other name rather than being pre-seeded.
    let mut seen: HashSet<Ustr> = [Ustr::from("gl_Position")].into_iter().collect();
    if !matches!(iface.fragment_output, FragmentOutput::Mrt(_)) {
        seen.insert(Ustr::from("FragColor"));
    }
    let names = iface
        .vertex_inputs
        .iter()
        .flat_map(|i| i.attrs.iter().map(|(n, _)| *n))
        .chain(iface.uniform_groups.iter().flat_map(|g| g.members.iter().map(|(n, _)| *n)))
        .chain(iface.array_groups.iter().flat_map(|g| g.members.iter().map(|m| m.name)))
        .chain(iface.varyings.iter().map(|v| v.name))
        .chain(iface.textures.iter().flat_map(|t| t.samplers.iter().map(|(n, _)| *n)))
        .chain(iface.bare_samplers.iter().map(|s| s.name))
        .chain(match &iface.fragment_output {
            FragmentOutput::Mrt(outs) => outs.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            _ => Vec::new(),
        });
    for n in names {
        if !seen.insert(n) {
            return Err(err(span, format!("shader pipeline: name `{n}` would collide (attributes, uniforms, engine-block members, varyings, samplers and reserved names share one GLSL namespace across both stages)")));
        }
    }
    Ok(())
}

/// Reject two blocks of one program sharing a BLOCK name.
///
/// Block names are their own GLSL namespace -- distinct from the member names
/// `check_pipeline_collisions` censuses, which is why this is a second check and
/// not a line in that one. It matters because an array block's name is MINTED
/// (`<entry>_Arrays`) rather than written by the author: a user struct that
/// happens to spell it would otherwise produce two `layout(std140) uniform
/// <same>` declarations, which is a link error carrying no Rock span. Minting a
/// different name instead is the fallback this repo does not take -- the
/// compiler's job is to refuse the program.
fn check_block_name_collisions<'a>(
    scalar: impl Iterator<Item = &'a UniformBlockPlan>,
    arrays: impl Iterator<Item = &'a UniformArrayBlockPlan>,
    span: Span,
) -> Result<(), CompilerError> {
    let mut seen: HashSet<Ustr> = HashSet::default();
    // The engine block is declared in BOTH stages but is ONE block, so the
    // scalar list is deduped by identity before the census rather than after --
    // otherwise it collides with itself.
    let names = scalar
        .map(|p| p.struct_name)
        .collect::<HashSet<_>>()
        .into_iter()
        .chain(arrays.map(|p| p.block_name));
    for n in names {
        if !seen.insert(n) {
            return Err(err(span, format!(
                "shader pipeline: uniform block name `{n}` is declared twice -- a stage's array block is named `<entry>_Arrays`, so a @uniform group of that name collides with it"
            )));
        }
    }
    Ok(())
}

/// A2: compile a `@shader(vertex)` + `@shader(fragment)` pair into one
/// `#ifdef VERTEX`/`#ifdef PIXEL` file (docs/shaders.md). The vertex entry's
/// return type (the varyings value struct) must be exactly the fragment entry's
/// first param type. Vertex params are classified by annotation: `@attr` (per-
/// vertex attribute struct), `@instance` (per-instance attribute divisor), and
/// `@uniform` (uniform block); the varyings `position: Vec4` field drives
/// `gl_Position` and is suppressed as a varying.
pub fn emit_shader_pipeline(
    ws: &Workspace,
    mid: ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    Ok(emit_shader_pipeline_with_dependencies(
        ws, mid, vert_root, frag_root, profile,
    )?
    .source)
}

/// **The one validated plan for an A2 `(vertex, fragment)` program.**
///
/// Everything emission decides before it writes GLSL, in emission's own order:
/// both stage annotations, the vertex shape and its varyings, the fragment
/// classified AGAINST those varyings (not as a standalone entry, where a
/// varyings param has no meaning), the cross-stage collisions, the per-stage
/// `@uniform` and program `@engine` std140 layouts, each stage's own lowering +
/// tier-0 + graph/view population + stage-builtin pass, and the program's
/// `@textures` slot assignment.
///
/// [`check_pipeline_profile`] builds this and DROPS it; emission builds it and
/// renders it.
struct PipelinePlan {
    vshape: VertexShape,
    fshape: FragShape,
    vert: StagePlan,
    frag: StagePlan,
    /// What the program's interface MEANS -- roles, names, order, neutral
    /// kinds. Target-neutral, and the sole input to `abi`.
    interface: ShaderInterfacePlan,
    /// GLSL's physical answer to `interface`.
    abi: ShaderAbiPlan,
}

fn plan_shader_pipeline_from_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<PipelinePlan, CompilerError> {
    let vertex = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Vertex)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no vertex stage for a pipeline".to_string(),
            )
        })?;
    let fragment = view
        .stages
        .iter()
        .find(|stage| stage.stage == ShaderStage::Fragment)
        .ok_or_else(|| {
            CompilerError::new_no_location(
                "shader graph/view has no fragment stage for a pipeline".to_string(),
            )
        })?;
    plan_shader_pipeline_inner(
        ws,
        scx,
        vertex.entry.func,
        fragment.entry.func,
        (vertex, fragment),
        &view.concrete_plans,
    )
}

fn plan_shader_pipeline_inner(
    ws: &Workspace,
    scx: &ScalarCtx,
    vfref: FuncRef,
    ffref: FuncRef,
    stage_views: (&ShaderStageView, &ShaderStageView),
    concrete_plans: &ConcretePlans,
) -> Result<PipelinePlan, CompilerError> {
    let vf = &ws.module(vfref.module).functions[vfref.func];
    let ff = &ws.module(ffref.module).functions[ffref.func];
    if shader_stage(vf) != Some("vertex") {
        return Err(err(
            vf.name_span,
            format!("shader pipeline: the vertex root `{}` must carry @shader(vertex)", vf.name),
        ));
    }
    if shader_stage(ff) != Some("fragment") {
        return Err(err(
            ff.name_span,
            format!("shader pipeline: the fragment root `{}` must carry @shader(fragment)", ff.name),
        ));
    }

    let vshape = classify_vertex(ws, vf)?;
    let varyings = classify_varyings(ws, vshape.varyings_ty, vf.name_span)?;
    let fshape = classify_pipeline_fragment(ws, ff, vshape.varyings_ty)?;

    // One std140 block per STAGE, each at its own binding index. The stages
    // declare different `@uniform` types and cannot be made to share one: a
    // vertex entry is reused across several fragment entries (mag's `ui_vert`
    // serves three), so a merged per-program type would need that one fn to
    // declare a different struct per program.
    //
    // A stage group holds only what that shader OWNS: the engine-authored
    // camera/time/screen band lives in `@engine` instead, which is what lets
    // bitgun's grass keep a game-owned `entity_count` beside them.
    let vert_group = vshape
        .uniform_group
        .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Vertex), g, vf.name_span))
        .transpose()?;
    let frag_group = fshape
        .uniform_group
        .map(|g| logical_uniform_group(ws, UniformRole::Stage(IrStage::Fragment), g, ff.name_span))
        .transpose()?;

    // `@engine` is ONE block per program, so unlike `@uniform` the stages must
    // agree: it is engine-owned and fixed, and both stages naming the same block
    // at index 0 is what lets the engine fill it once per pass rather than once
    // per stage.
    //
    // What this does NOT check is that the type IS the engine's own `EngineU` --
    // deliberately. A compiler that knows an engine type by name has engine
    // policy inside the language. The fingerprint the plan ships is what the
    // ENGINE compares at registration, which is the layer that owns the answer;
    // a game marking its own struct `@engine` gets a block nothing fills, and
    // that check belongs there rather than here.
    let engine_group = match (vshape.engine_group, fshape.engine_group) {
        (Some(v), Some(f)) if v != f => {
            return Err(err(vf.name_span, format!(
                "shader pipeline: the stages declare different @engine groups (`{v}` and `{f}`) -- the engine block is one per program"
            )));
        }
        (v, f) => v.or(f),
    };
    let engine_logical = engine_group
        .map(|g| logical_uniform_group(ws, UniformRole::Engine, g, vf.name_span))
        .transpose()?;

    // One array block per stage that declares `@size(N)` params, at its own
    // reserved binding. Built from the SAME std140 authority as the scalar
    // blocks above and refusing the same element set, so the declaration and the
    // host's offsets cannot come from two different answers.
    let vert_array_group = logical_array_group(IrStage::Vertex, vf.name, &vshape.arrays);
    let frag_array_group = logical_array_group(IrStage::Fragment, ff.name, &fshape.arrays);

    // `@textures` is one group per PROGRAM (texture units are program-scoped),
    // unlike `@uniform`, which is one block per STAGE.
    let group = match (vshape.texture_group, fshape.texture_group) {
        (Some(v), Some(f)) if v != f => {
            return Err(err(vf.name_span, format!(
                "shader pipeline: the stages declare different @textures groups (`{v}` and `{f}`) -- texture units are program-scoped, so one program has one group"
            )));
        }
        (v, f) => v.or(f),
    };
    let bare = has_bare_sampler(&vshape.samplers, &vshape.texture_params)
        || has_bare_sampler(&fshape.samplers, &fshape.texture_params);
    // WHICH stages read the group -- a stage that samples must declare the
    // sampler in its own section, so "the program has one" cannot drive
    // emission.
    let group_stages: Vec<IrStage> = [
        vshape.texture_group.is_some().then_some(IrStage::Vertex),
        fshape.texture_group.is_some().then_some(IrStage::Fragment),
    ]
    .into_iter()
    .flatten()
    .collect();
    let textures = logical_textures(ws, group, bare, group_stages, vf.name_span)?;

    // THE program's logical interface. Everything downstream -- the name
    // census, the ABI plan and every emitted declaration -- reads this.
    let interface = ShaderInterfacePlan {
        stages: vec![IrStage::Vertex, IrStage::Fragment],
        vertex_inputs: vshape.inputs.clone(),
        varyings: varyings.varyings.clone(),
        fragment_output: fshape.output.clone(),
        bare_samplers: bare_sampler_list(
            &vshape.samplers,
            &vshape.texture_params,
            IrStage::Vertex,
        )
        .into_iter()
        .chain(bare_sampler_list(&fshape.samplers, &fshape.texture_params, IrStage::Fragment))
        .collect(),
        uniform_groups: engine_logical.into_iter().chain(vert_group).chain(frag_group).collect(),
        array_groups: vert_array_group.into_iter().chain(frag_array_group).collect(),
        textures,
    };

    check_pipeline_collisions(&interface, vf.name_span)?;
    let abi = ShaderAbiPlan::glsl(&interface, vf.name_span)?;
    check_block_name_collisions(
        abi.uniform_blocks.iter().map(|(_, p)| p),
        abi.uniform_array_blocks.iter().map(|(_, p)| p),
        vf.name_span,
    )?;

    let vert = plan_stage_from_view(ws, scx, stage_views.0, Stage::Vertex, concrete_plans)?;
    let frag = plan_stage_from_view(ws, scx, stage_views.1, Stage::Fragment, concrete_plans)?;
    reject_position_read(ws, &frag.ir.body, frag.ir.body.params[fshape.varyings_param], ff.name_span)?;

    Ok(PipelinePlan { vshape, fshape, vert, frag, interface, abi })
}

/// The PIPELINE counterpart of [`check_entry_profile`]: emission's own
/// preparation for a `(vertex, fragment)` pair, run without emitting.
///
/// A pipeline fragment is NOT checkable as a single entry: `classify_entry`
/// refuses its varyings param outright, which is why the mono view must be
/// given the PAIRS production registers rather than a flat root list.
pub fn check_pipeline_profile(
    ws: &Workspace,
    vfref: FuncRef,
    ffref: FuncRef,
) -> Result<(), CompilerError> {
    let session = shader_program_session(
        ws,
        ShaderProgram { vert: Some(vfref), frag: ffref },
    )?;
    check_pipeline_profile_with_view(ws, &session.scx, &session.view)
}

/// Validate one shader pipeline against the graph-owned concrete populations.
pub fn check_pipeline_profile_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<(), CompilerError> {
    if view.program.vert.is_none() {
        return Err(CompilerError::new_no_location(
            "shader graph/view single-entry program passed to pipeline GLSL validation".to_string(),
        ));
    }
    plan_shader_pipeline_from_view(ws, scx, view).map(|_| ())
}

pub fn emit_shader_pipeline_with_dependencies(
    ws: &Workspace,
    mid: ModuleId,
    vert_root: &str,
    frag_root: &str,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let vfref = resolve_root(ws, mid, vert_root)?;
    let ffref = resolve_root(ws, mid, frag_root)?;
    let session = shader_program_session(
        ws,
        ShaderProgram { vert: Some(vfref), frag: ffref },
    )?;
    emit_shader_pipeline_with_view(ws, &session.scx, &session.view, profile)
}

/// Emit a pipeline from the graph-owned stage populations in `view`.
pub fn emit_shader_pipeline_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    if view.program.vert.is_none() {
        return Err(CompilerError::new_no_location(
            "shader graph/view single-entry program passed to pipeline GLSL emission".to_string(),
        ));
    }
    let plan = plan_shader_pipeline_from_view(ws, scx, view)?;
    render_shader_pipeline(ws, scx.interner(), &plan, profile)
}

fn render_shader_pipeline(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &PipelinePlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    // Validated ONCE for the program: an unknown binding belongs to no stage,
    // so re-running the check per stage says nothing the first run did not.
    let vspan = ws.module(plan.vert.ir.entry.fref().module).functions[plan.vert.ir.entry.fref().func]
        .name_span;
    let program = ProgramAbi::of(
        &plan.interface,
        &plan.abi,
        &[IrStage::Vertex, IrStage::Fragment],
        vspan,
    )?;

    let (vertex_block, vertex_dependencies) = emit_vertex_stage(
        ws,
        interner,
        &plan.vert,
        &plan.vshape,
        &plan.interface,
        &program,
    )?;
    let (pixel_block, pixel_dependencies) = emit_fragment_stage(
        ws,
        interner,
        &plan.frag,
        &plan.fshape,
        &plan.interface,
        &program,
    )?;

    let (version, precision) = profile_header(profile);
    let source = format!(
        "{version}\n\n#ifdef VERTEX\n\n{precision}{vertex_block}\n#endif\n\n#ifdef PIXEL\n\n{precision}{pixel_block}\n#endif\n"
    );
    let dependencies = vertex_dependencies
        .into_iter()
        .chain(pixel_dependencies)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    // Each block carries its binding, taken from the param ROLE: the engine
    // block at 0 (both stages name it), the vertex group at 1, the fragment
    // group at 2. Unlike `@textures` -- one group per PROGRAM, since texture
    // units are program-scoped -- `@uniform` blocks are per STAGE, so differing
    // types there are correct rather than a collision.
    Ok(ShaderEmission {
        source,
        dependencies,
        interface: plan.interface.clone(),
        abi: plan.abi.clone(),
        ir: ShaderIR { stages: vec![plan.vert.ir.clone(), plan.frag.ir.clone()] },
    })
}

/// CLI entry point: dispatch on the root's `@shader` annotation -- an annotated
/// root compiles as a whole-shader fragment entry (exactly one root), plain
/// roots as a fn library.
pub fn emit_shader(
    ws: &Workspace,
    mid: ModuleId,
    roots: &[String],
    profile: GlslProfile,
) -> Result<String, CompilerError> {
    let annotated = roots
        .iter()
        .filter(|r| {
            resolve_root(ws, mid, r)
                .ok()
                .is_some_and(|fref| shader_stage(&ws.module(fref.module).functions[fref.func]).is_some())
        })
        .count();
    match annotated {
        0 => emit_shader_library(ws, mid, roots),
        1 if roots.len() == 1 => emit_shader_entry(ws, mid, &roots[0], profile),
        _ => Err(err(
            Span::default(),
            "rock shader: a @shader(fragment) entry compiles alone -- pass exactly one root fn",
        )),
    }
}

/// The scalar return kind of a value-test fn -- selects the readback texture
/// format on the GL side (int/bool -> R32I, float -> R32F) and the fragment
/// output declaration.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueTestRet {
    Int,
    Float,
    Bool,
}

impl ValueTestRet {
    /// The GL runner's tag for this kind (matches the JSON job protocol).
    pub fn tag(self) -> &'static str {
        match self {
            ValueTestRet::Int => "int",
            ValueTestRet::Float => "float",
            ValueTestRet::Bool => "bool",
        }
    }
}

/// A complete GLSL shader pair driving one `@test(returns = ...)` value test
/// (`docs/multi-backend-testing-plan.md` G1). The fragment shader wraps the
/// emitted root fn and writes its scalar result to `layout(location = 0)`; the
/// vertex shader draws a fullscreen triangle (no vertex attributes). Both carry
/// the profile's `#version` line so the GL runner compiles them as one program.
#[derive(Debug)]
pub struct ValueTestShaders {
    pub vertex: String,
    pub fragment: String,
    pub ret: ValueTestRet,
}

/// The profile's `#version` line plus the fragment-stage precision preamble
/// (ES needs an explicit float/int precision; core does not).
fn profile_header(profile: GlslProfile) -> (&'static str, &'static str) {
    match profile {
        GlslProfile::Core330 => ("#version 330 core", ""),
        GlslProfile::Es300 => ("#version 300 es", "precision highp float;\nprecision highp int;\n"),
    }
}

/// Emit the vertex+fragment GLSL pair for a scalar-returning `@test(returns=)`
/// fn under `profile`. The root must take no parameters (a `@test` fn) and
/// return `int`/`float`/`bool`; anything else -- a non-tier-0 return, a param,
/// or a body outside the tier-0 profile -- rejects with a spanned
/// `CompilerError` (the G1 two-tier SKIP reason). No fallback: an unmappable
/// intrinsic or non-scalar shape is a rejection, never a guess.
pub fn emit_value_test(
    ws: &Workspace,
    mid: ModuleId,
    root: &str,
    profile: GlslProfile,
) -> Result<ValueTestShaders, CompilerError> {
    let fref = resolve_root(ws, mid, root)?;
    let f = &ws.module(fref.module).functions[fref.func];
    if !f.param_decls.is_empty() {
        return Err(err(
            f.name_span,
            "value test: the root fn must take no parameters (a @test fn)",
        ));
    }
    let ret = match f.return_ty.map(|rt| glsl_ty(ws, &SpecSubst::default(), rt, f.name_span, "return")) {
        Some(Ok(GlslTy::Int)) => ValueTestRet::Int,
        Some(Ok(GlslTy::Float)) => ValueTestRet::Float,
        Some(Ok(GlslTy::Bool)) => ValueTestRet::Bool,
        Some(Ok(other)) => {
            return Err(err(
                f.name_span,
                format!(
                    "value test: return type `{}` is not a scalar (only int/float/bool return \
                     values are read back today)",
                    other.name()
                ),
            ))
        }
        _ => {
            return Err(err(
                f.name_span,
                "value test: the root fn must return int/float/bool",
            ))
        }
    };

    // The fn library runs the strict tier-0 profile pass; a body outside the
    // subset rejects here (the SKIP reason -- the A1 fn-emission work list).
    let defs = emit_shader_library(ws, mid, std::slice::from_ref(&root.to_string()))?;
    let call = mangle_fn(ws, f);

    let (version, precision) = profile_header(profile);
    let (out_ty, out_expr) = match ret {
        ValueTestRet::Int => ("int", format!("{call}()")),
        ValueTestRet::Float => ("float", format!("{call}()")),
        ValueTestRet::Bool => ("int", format!("({call}() ? 1 : 0)")),
    };
    let fragment = format!(
        "{version}\n{precision}\n{defs}\nlayout(location = 0) out {out_ty} _rock_value_out;\n\
         void main() {{\n  _rock_value_out = {out_expr};\n}}\n"
    );
    // A fullscreen triangle from gl_VertexID -- no vertex buffers/attributes, so
    // the runner binds only an empty VAO. gl_VertexID is core since 3.0 / ES 3.0.
    let vertex = format!(
        "{version}\nvoid main() {{\n  \
         vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n  \
         gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n}}\n"
    );
    Ok(ValueTestShaders { vertex, fragment, ret })
}

/// True iff `root` resolves to a `@shader(vertex)` fn -- the CLI routes to the
/// two-root vertex+fragment pipeline form when any root is a vertex entry.
pub fn is_vertex_shader_root(ws: &Workspace, mid: ModuleId, root: &str) -> bool {
    resolve_root(ws, mid, root)
        .ok()
        .is_some_and(|fref| shader_stage(&ws.module(fref.module).functions[fref.func]) == Some("vertex"))
}

/// True iff `root` resolves to a `@shader(fragment)` fn -- the CLI uses this to
/// admit `--profile` for the single-root fullscreen form while still rejecting
/// it for the route-B fn library (which emits no `#version` line to switch).
pub fn is_fragment_shader_root(ws: &Workspace, mid: ModuleId, root: &str) -> bool {
    resolve_root(ws, mid, root)
        .ok()
        .is_some_and(|fref| shader_stage(&ws.module(fref.module).functions[fref.func]) == Some("fragment"))
}

/// The `@shader(<stage>)` annotation's stage argument, if the fn carries one.
pub fn shader_stage(f: &Function) -> Option<&'static str> {
    let ann = f.annotations.iter().find(|a| a.name.name.as_str() == "shader")?;
    match ann.args.first().map(|a| &a.value) {
        Some(AnnoValue::Ident(id)) if id.name.as_str() == "fragment" => Some("fragment"),
        Some(AnnoValue::Ident(id)) if id.name.as_str() == "vertex" => Some("vertex"),
        _ => Some("<unknown>"),
    }
}

#[cfg(test)]
mod assertion_flip_tests {
    use super::*;
    use ustr::Ustr;

    fn build_ws(src: &str) -> (Workspace, ModuleId) {
        let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module(f.contents, id).expect("stdlib parse")
            })
            .collect();
        modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
        let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("mid");
        (ws, mid)
    }

    /// The unfuck's flipped assertion: a MISSED int->float coercion sink reaches
    /// the emitter as a raw int-typed value in a float slot and must be a HARD
    /// emit error, never silent driver-dependent GLSL. Here we lower
    /// `f(i: int) -> float { return i }` (whose return sink DOES coerce) and then
    /// strip the `IntToFloat` to simulate a missed sink.
    #[test]
    fn missed_sink_is_a_hard_emit_error() {
        let (ws, mid) = build_ws("@pure fn f(i: int) -> float { return i }");
        let scx = ScalarCtx::new_values_first_class(&ws);
        let fref = resolve_root(&ws, mid, "f").expect("root");
        let mut mf =
            lower_instance(&ws, &scx, &Instance::base(fref))
                .expect("lower");

        // The coercion is present today (regression pin for the return sink).
        assert!(
            mf.insts.iter().any(|(_, i)| matches!(i, Inst::IntToFloat(_))),
            "expected an IntToFloat coercion at the return sink"
        );

        // Strip it: rewrite each `return` to the raw int operand.
        let bids: Vec<_> = mf.blocks.iter().map(|(b, _)| b).collect();
        for bid in bids {
            if let Term::Return(vs) = mf.blocks[bid].term.clone() {
                let stripped: Vec<_> = vs
                    .iter()
                    .map(|&v| match mf.insts[v] {
                        Inst::IntToFloat(x) => x,
                        _ => v,
                    })
                    .collect();
                mf.set_term(bid, Term::Return(stripped));
            }
        }

        let r = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf);
        let msg = r.expect_err("a missed int->float sink must be a hard emit error").message;
        assert!(msg.contains("missed lowering sink"), "unexpected error: {msg}");
    }

    /// **A generic callee with no recorded instantiation is a REJECTION.**
    ///
    /// Reading an absent `spec_bindings` entry as "no instantiation" is right
    /// for a callee that has no generics and wrong for one that does: it
    /// silently substitutes the declaration's shape, which GLSL has no boxed
    /// representation for. Removing the record must reject with a span, never
    /// emit against the declaration.
    ///
    /// The CONTROL is asserted first: with the record present the same body
    /// emits and names the specialized callee, so the rejection below measures
    /// the missing record and not an unemittable fixture.
    #[test]
    fn a_generic_callee_with_no_recorded_instantiation_rejects() {
        let (ws, mid) = build_ws(
            "@pure fn passthrough<T>(x: T) -> T { return x }\n             @pure fn caller(x: float) -> float { return passthrough(x) }",
        );
        let scx = ScalarCtx::new_values_first_class(&ws);
        let fref = resolve_root(&ws, mid, "caller").expect("root");
        let mut mf =
            lower_instance(&ws, &scx, &Instance::base(fref))
                .expect("lower");

        // CONTROL: the record exists and the body emits against it.
        assert_eq!(mf.spec_bindings.len(), 1, "the fixture recorded no instantiation to remove");
        let ok = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
            .expect("the body emits with its instantiation recorded");
        assert!(ok.contains("passthrough_T_ret_T_spec_p_float("), "{ok}");

        // Remove exactly the record, change nothing else.
        mf.spec_bindings.clear();
        let msg = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
            .expect_err("a generic callee with no instantiation must reject")
            .message;
        assert!(
            msg.contains("no recorded instantiation") && msg.contains("passthrough"),
            "the rejection does not name the callee or the cause: {msg}"
        );
    }

    /// **ADMISSION CONTROL for the rule above.** A callee that declares no
    /// generics has no record to miss, so it must pass the same gate and emit.
    /// Without this the rejection and "GLSL stopped emitting calls" look alike.
    #[test]
    fn a_nongeneric_callee_needs_no_recorded_instantiation() {
        let (ws, mid) = build_ws(
            "@pure fn doubled(x: float) -> float { return x * 2.0 }\n             @pure fn caller(x: float) -> float { return doubled(x) }",
        );
        let scx = ScalarCtx::new_values_first_class(&ws);
        let fref = resolve_root(&ws, mid, "caller").expect("root");
        let mf = lower_instance(&ws, &scx, &Instance::base(fref))
            .expect("lower");

        // The fixture really does exercise the gate: a call is present, and it
        // carries NO record -- which is the input the generic arm rejects.
        assert!(
            mf.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })),
            "the fixture has no call to admit"
        );
        assert!(mf.spec_bindings.is_empty(), "a non-generic callee recorded an instantiation");

        let out = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
            .expect("a non-generic callee must be admitted with no record");
        assert!(out.contains("doubled_float_ret_float("), "{out}");
    }

    /// A fragment-only interface with an engine group, a stage group, an array
    /// group and two samplers -- one of every physical entry, so each arm below
    /// perturbs exactly one of them.
    fn abi_fixture() -> ShaderInterfacePlan {
        use skull_mir::gpu_layout::ArrayMemberDecl;
        use skull_mir::shader_ir::{LogicalArrayGroup, LogicalTextureGroup, LogicalUniformGroup};
        ShaderInterfacePlan {
            stages: vec![IrStage::Fragment],
            uniform_groups: vec![
                LogicalUniformGroup {
                    role: UniformRole::Engine,
                    struct_name: Ustr::from("EngineU"),
                    members: vec![(Ustr::from("time"), WireKind::F32)],
                },
                LogicalUniformGroup {
                    role: UniformRole::Stage(IrStage::Fragment),
                    struct_name: Ustr::from("FragU"),
                    members: vec![(Ustr::from("cutoff"), WireKind::F32)],
                },
            ],
            array_groups: vec![LogicalArrayGroup {
                stage: IrStage::Fragment,
                owner: Ustr::from("ff"),
                members: vec![ArrayMemberDecl {
                    name: Ustr::from("lights"),
                    elem: WireKind::F32x4,
                    capacity: 4,
                    span: Span::default(),
                }],
            }],
            textures: Some(LogicalTextureGroup {
                struct_name: Some(Ustr::from("Tex")),
                samplers: vec![
                    (Ustr::from("a"), SamplerKind::Float),
                    (Ustr::from("b"), SamplerKind::Float),
                ],
                stages: vec![IrStage::Fragment],
            }),
            ..Default::default()
        }
    }

    /// **The ABI is validated against the interface it claims to answer, and a
    /// dropped declaration is impossible.**
    ///
    /// Each arm perturbs ONE physical entry. A missing declaration surfaces as
    /// a driver compile error with no compiler diagnostic, so "omit it and
    /// carry on" is the one behaviour that must not exist.
    #[test]
    fn the_abi_validation_rejects_every_disagreement_with_the_interface() {
        let iface = abi_fixture();
        let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("the fixture must lay out");

        // CONTROL: the unperturbed pair validates and resolves with every entry
        // present, so each rejection below measures its perturbation.
        validate_abi(&iface, &abi, Span::default()).expect("the unperturbed plan must validate");
        let program = ProgramAbi::of(&iface, &abi, &[IrStage::Fragment], Span::default())
            .expect("the unperturbed plan must resolve");
        let ok = program.stage(IrStage::Fragment).expect("the fragment projection");
        assert!(ok.uniform_block.is_some() && ok.engine_block.is_some());
        assert!(ok.array_block.is_some());
        assert_eq!(ok.samplers.len(), 2);

        let reject = |a: &ShaderAbiPlan, what: &str| -> String {
            validate_abi(&iface, a, Span::default())
                .err()
                .unwrap_or_else(|| panic!("{what} must reject"))
                .message
        };

        // MISSING stage block.
        let mut m = abi.clone();
        m.uniform_blocks.retain(|(b, _)| *b != UNIFORM_BLOCK_BINDING_FRAGMENT);
        assert!(reject(&m, "a missing stage block").contains("has no block for"));

        // MISSING @engine block -- silently omitted before.
        let mut m = abi.clone();
        m.uniform_blocks.retain(|(b, _)| *b != UNIFORM_BLOCK_BINDING_ENGINE);
        assert!(reject(&m, "a missing @engine block").contains("EngineU"));

        // MISSING array block, over an interface that DOES declare one -- the
        // arm a fixture without an array group cannot reach.
        let mut m = abi.clone();
        m.uniform_array_blocks.clear();
        assert!(reject(&m, "a missing array block").contains("array group"));

        // WRONG BLOCK at the right binding: presence is not identity. The
        // engine block sits at binding 2, so every per-binding presence check
        // passes while the emitted members are another group's.
        let mut m = abi.clone();
        let engine = m
            .uniform_blocks
            .iter()
            .find(|(b, _)| *b == UNIFORM_BLOCK_BINDING_ENGINE)
            .map(|(_, p)| p.clone())
            .expect("the engine block");
        for (b, p) in m.uniform_blocks.iter_mut() {
            if *b == UNIFORM_BLOCK_BINDING_FRAGMENT {
                *p = engine.clone();
            }
        }
        let msg = reject(&m, "a foreign block at the right binding");
        assert!(msg.contains("EngineU") && msg.contains("FragU"), "{msg}");

        // WRONG MEMBERS in the right block: the name matches and the layout
        // does not.
        let mut m = abi.clone();
        for (b, p) in m.uniform_blocks.iter_mut() {
            if *b == UNIFORM_BLOCK_BINDING_FRAGMENT {
                p.members[0].name = Ustr::from("renamed");
            }
        }
        assert!(reject(&m, "a renamed member").contains("but the interface declares"));

        // WRONG CAPACITY in an array block -- what an `@size(N)` change moves,
        // and what a block-name check alone cannot see.
        let mut m = abi.clone();
        m.uniform_array_blocks[0].1.members[0].capacity = 8;
        assert!(reject(&m, "a changed array capacity").contains("interface declares"));

        // UNKNOWN binding: no interface group claims it, and it belongs to no
        // stage, so only a program-wide check can refuse it.
        let mut m = abi.clone();
        m.uniform_blocks.push((17, engine.clone()));
        assert!(reject(&m, "an unknown binding").contains("which no"));

        // DUPLICATE binding.
        let mut m = abi.clone();
        let d = m
            .uniform_blocks
            .iter()
            .find(|(b, _)| *b == UNIFORM_BLOCK_BINDING_FRAGMENT)
            .cloned()
            .expect("the stage block");
        m.uniform_blocks.push(d);
        assert!(reject(&m, "a duplicated block").contains("more than one"));
    }

    /// **The sampler join is a BIJECTION**, in all three directions: a
    /// duplicated unit, a unit for a sampler the group does not name, and a
    /// named sampler with no unit.
    ///
    /// `[a, a]` against an interface naming `[a, b]` has the right length and
    /// every element is a member -- and it declares `a` twice while `b` is
    /// never declared at all.
    #[test]
    fn the_sampler_join_is_a_bijection() {
        let iface = abi_fixture();
        let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("lay out");
        assert_eq!(abi.texture_units, vec![Ustr::from("a"), Ustr::from("b")]);
        assert_eq!(
            ProgramAbi::of(&iface, &abi, &[IrStage::Fragment], Span::default())
                .expect("the unperturbed join must resolve")
                .stage(IrStage::Fragment)
                .expect("the fragment projection")
                .samplers
                .len(),
            2
        );

        let reject = |units: Vec<Ustr>, what: &str| -> String {
            let mut a = abi.clone();
            a.texture_units = units;
            validate_abi(&iface, &a, Span::default())
                .err()
                .unwrap_or_else(|| panic!("{what} must reject"))
                .message
        };
        assert!(reject(vec![Ustr::from("a"), Ustr::from("a")], "a duplicated unit")
            .contains("more than one unit"));
        assert!(reject(vec![Ustr::from("a"), Ustr::from("zzz")], "an alien unit")
            .contains("does not name"));
        // The MISSING branch: fewer units than the group names.
        assert!(reject(vec![Ustr::from("a")], "a sampler with no unit").contains("gives no unit"));
    }

    /// **Two attributes may not share a location.** A bijection on NAMES
    /// accepts `[(a, 0), (b, 0)]`, which aliases one buffer slot onto two
    /// inputs and is invisible until the geometry is wrong.
    #[test]
    fn overlapping_attribute_locations_reject() {
        use skull_mir::gpu_layout::AttrKind;
        use skull_mir::shader_ir::{LogicalVertexInput, VertexRate};
        let iface = ShaderInterfacePlan {
            stages: vec![IrStage::Vertex],
            vertex_inputs: vec![LogicalVertexInput {
                struct_name: Ustr::from("V"),
                rate: VertexRate::PerVertex,
                attrs: vec![
                    (Ustr::from("a"), AttrKind::F32x2),
                    (Ustr::from("b"), AttrKind::F32x2),
                ],
            }],
            ..Default::default()
        };
        let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("lay out");
        // CONTROL: distinct locations validate.
        assert_eq!(abi.attribute_locations, vec![(Ustr::from("a"), 0), (Ustr::from("b"), 1)]);
        validate_abi(&iface, &abi, Span::default()).expect("distinct locations must validate");

        let mut overlap = abi.clone();
        overlap.attribute_locations = vec![(Ustr::from("a"), 0), (Ustr::from("b"), 0)];
        let msg = validate_abi(&iface, &overlap, Span::default())
            .err()
            .expect("overlapping locations must reject")
            .message;
        assert!(msg.contains("both occupy location"), "{msg}");

        // A location at the top of the space must REJECT, not overflow: the
        // validator exists to refuse malformed plans and may not panic or wrap
        // on one.
        let mut top = abi.clone();
        top.attribute_locations = vec![(Ustr::from("a"), u32::MAX), (Ustr::from("b"), 1)];
        let msg = validate_abi(&iface, &top, Span::default())
            .err()
            .expect("a location at u32::MAX must reject")
            .message;
        assert!(msg.contains("spans past the end"), "{msg}");
    }

    /// **A stage the interface does not declare is a REJECTION, not a lookup
    /// miss.**
    ///
    /// `ProgramAbi::of` takes the stages the renderer will emit as a census.
    /// Trusting `iface.stages` instead would push the disagreement to the
    /// projection lookup, where the only options are a panic or a silent
    /// fallback -- and both are the shape this arc removes.
    #[test]
    fn a_stage_the_interface_does_not_declare_rejects() {
        let mut iface = abi_fixture();
        let abi = ShaderAbiPlan::glsl(&iface, Span::default()).expect("lay out");

        // CONTROL: the fixture declares Fragment, and projecting it works.
        assert_eq!(iface.stages, vec![IrStage::Fragment]);
        let program = ProgramAbi::of(&iface, &abi, &[IrStage::Fragment], Span::default())
            .expect("the declared stage must project");
        assert!(program.stage(IrStage::Fragment).is_ok());
        // ...and asking for one it never projected is an error, not a panic.
        assert!(program.stage(IrStage::Vertex).is_err());

        // Requiring a stage the interface does not declare rejects up front.
        let msg = ProgramAbi::of(&iface, &abi, &[IrStage::Vertex], Span::default())
            .err()
            .expect("an undeclared stage must reject")
            .message;
        assert!(msg.contains("vertex") && msg.contains("does not declare"), "{msg}");

        // ...and once the interface declares it, the same request succeeds --
        // so the rejection is about the census and not about Vertex.
        iface.stages.push(IrStage::Vertex);
        ProgramAbi::of(&iface, &abi, &[IrStage::Vertex], Span::default())
            .expect("a declared stage must project");
    }

    /// **Two logical groups claiming one binding is the INTERFACE's defect.**
    ///
    /// Normal construction turns it into a duplicate ABI entry and it is caught
    /// downstream, which makes the diagnostic about the wrong side. The
    /// validator states it directly, for both group kinds.
    #[test]
    fn duplicate_expected_bindings_reject() {
        use skull_mir::gpu_layout::ArrayMemberDecl;
        use skull_mir::shader_ir::{LogicalArrayGroup, LogicalUniformGroup};

        // CONTROL: one group per role validates.
        let base = abi_fixture();
        let abi = ShaderAbiPlan::glsl(&base, Span::default()).expect("lay out");
        validate_abi(&base, &abi, Span::default()).expect("the control must validate");

        let mut two_roles = base.clone();
        two_roles.uniform_groups.push(LogicalUniformGroup {
            role: UniformRole::Stage(IrStage::Fragment),
            struct_name: Ustr::from("OtherU"),
            members: vec![(Ustr::from("x"), WireKind::F32)],
        });
        let msg = validate_abi(&two_roles, &abi, Span::default())
            .err()
            .expect("two groups at one role must reject")
            .message;
        assert!(msg.contains("two uniform groups at binding"), "{msg}");

        let mut two_arrays = base.clone();
        two_arrays.array_groups.push(LogicalArrayGroup {
            stage: IrStage::Fragment,
            owner: Ustr::from("other"),
            members: vec![ArrayMemberDecl {
                name: Ustr::from("y"),
                elem: WireKind::F32x4,
                capacity: 2,
                span: Span::default(),
            }],
        });
        let msg = validate_abi(&two_arrays, &abi, Span::default())
            .err()
            .expect("two array groups for one stage must reject")
            .message;
        assert!(msg.contains("two array groups"), "{msg}");
    }

    /// **A fragment stage with no declared colour output rejects**, rather than
    /// having a `vec4` invented for an inconsistent plan.
    #[test]
    fn a_fragment_stage_with_no_declared_output_rejects() {
        use skull_mir::shader_ir::FragmentOutput;
        let (ws, mid) = build_ws(
            "value Vary { position: Vec4, uv: Vec2 }\n\
             value QuadVert { pos: Vec2, texcoord: Vec2 }\n\
             @shader(vertex)\n\
             fn ov(@attr a: QuadVert) -> Vary { return Vary { position: v(a.pos.x, a.pos.y, 0.0, 1.0), uv: a.texcoord } }\n\
             @shader(fragment)\n\
             fn of(vary: Vary) -> Vec4 { return v(vary.uv.x, 0.0, 0.0, 1.0) }\n\
             fn main() { let k = rock_shader(ov, of) }",
        );
        let vf = resolve_root(&ws, mid, "ov").expect("root");
        let ff = resolve_root(&ws, mid, "of").expect("root");
        let session = shader_program_session(
            &ws,
            ShaderProgram { vert: Some(vf), frag: ff },
        )
        .expect("the fixture must realize");
        let mut plan = plan_shader_pipeline_from_view(&ws, &session.scx, &session.view)
            .expect("the fixture must plan");

        // CONTROL: it renders with the output declared.
        assert!(matches!(plan.interface.fragment_output, FragmentOutput::Single(_)));
        render_shader_pipeline(&ws, session.scx.interner(), &plan, GlslProfile::Core330)
            .expect("the control must render");

        plan.interface.fragment_output = FragmentOutput::None;
        let msg = render_shader_pipeline(
            &ws,
            session.scx.interner(),
            &plan,
            GlslProfile::Core330,
        )
            .err()
            .expect("a fragment stage with no output must reject")
            .message;
        assert!(msg.contains("no fragment output"), "{msg}");
    }

    /// **A key of the WRONG ARITY rejects.**
    ///
    /// The key is positional against the callee's declared generics, so a key
    /// of a different length names a different function's instantiation.
    /// `spec_subst_from_key` zips the two, which silently drops the tail and
    /// substitutes the head -- a body emitted at a shape nobody asked for, with
    /// no complaint. Length is checked, not assumed.
    ///
    /// Both directions, and the SHORT case is non-empty on purpose: an empty
    /// key is already refused as "no recorded instantiation", so a `vec![]`
    /// fixture would pass against the missing length check and prove nothing.
    /// That is why the callee declares TWO generics.
    #[test]
    fn a_generic_callee_with_a_wrong_arity_key_rejects() {
        use skull_mir::monoty::MonoTy;
        let (ws, mid) = build_ws(
            "@pure fn pick<A, B>(a: A, b: B) -> A { return a }\n\
             @pure fn caller(x: float) -> float { return pick(x, 1) }",
        );
        let scx = ScalarCtx::new_values_first_class(&ws);
        let fref = resolve_root(&ws, mid, "caller").expect("root");
        let base = lower_instance(&ws, &scx, &Instance::base(fref))
            .expect("lower");

        // CONTROL: the recorded key has the declared arity, and the body emits.
        let iid = *base.spec_bindings.keys().next().expect("a recorded instantiation");
        assert_eq!(
            base.spec_bindings[&iid].args.len(),
            2,
            "the callee declares two generics"
        );
        emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &base)
            .expect("the fixture must emit with a well-formed key");

        for (label, key) in [
            ("too long", vec![MonoTy::Float, MonoTy::Int, MonoTy::Int]),
            // Non-empty and short: the case an `is_empty` check cannot see.
            ("too short", vec![MonoTy::Float]),
        ] {
            let mut mf = base.clone();
            let callee = base.spec_bindings[&iid].func;
            mf.spec_bindings.insert(iid, Instance::intern(scx.interner(), callee, key));
            let msg = emit_fn_def(&ws, scx.interner(), &ShaderInstance::plain(fref), &mf)
                .err()
                .unwrap_or_else(|| panic!("a {label} key must reject"))
                .message;
            assert!(
                msg.contains("generic") && msg.contains("pick"),
                "the {label} rejection does not name the callee: {msg}"
            );
        }
    }

    // A partial key -- a recorded instantiation with an unreified slot -- used to
    // be pinned here as its own rejection. A key is TOTAL now, so the producer
    // returns no key at all rather than one with a hole, and the state arrives
    // as an absent record. That is
    // `a_generic_callee_with_no_recorded_instantiation_rejects` above, which is
    // the surviving coverage; a second test inserting an empty key would assert
    // the same arm twice.
}

#[cfg(test)]
mod value_test_shader {
    use super::*;
    use ustr::Ustr;

    fn build_ws(src: &str) -> (Workspace, ModuleId) {
        let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
            .iter()
            .map(|f| {
                let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
                skull_parser::parse_module(f.contents, id).expect("stdlib parse")
            })
            .collect();
        modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
        let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("mid");
        (ws, mid)
    }

    #[test]
    fn int_return_wraps_root_and_writes_int_output() {
        let (ws, mid) = build_ws("fn t() -> int { return min(3, 5) }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ValueTestRet::Int);
        assert!(s.fragment.starts_with("#version 330 core\n"), "{}", s.fragment);
        assert!(s.fragment.contains("layout(location = 0) out int _rock_value_out;"), "{}", s.fragment);
        assert!(s.fragment.contains("_rock_value_out = t_ret_int();"), "{}", s.fragment);
        assert!(s.fragment.contains("int t_ret_int()"), "callee def present: {}", s.fragment);
        assert!(s.vertex.contains("gl_VertexID"), "{}", s.vertex);
    }

    #[test]
    fn float_return_uses_r32f_shape() {
        let (ws, mid) = build_ws("fn t() -> float { return clamp(9.0, 0.0, 5.0) }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ValueTestRet::Float);
        assert!(s.fragment.contains("out float _rock_value_out;"), "{}", s.fragment);
        assert!(s.fragment.contains("_rock_value_out = t_ret_float();"), "{}", s.fragment);
    }

    #[test]
    fn bool_return_writes_zero_or_one_to_int_output() {
        let (ws, mid) = build_ws("fn t() -> bool { return 3.0 == 3.0 }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
        assert_eq!(s.ret, ValueTestRet::Bool);
        assert!(s.fragment.contains("out int _rock_value_out;"), "{}", s.fragment);
        assert!(s.fragment.contains("_rock_value_out = (t_ret_bool() ? 1 : 0);"), "{}", s.fragment);
    }

    #[test]
    fn es_profile_carries_version_and_precision() {
        let (ws, mid) = build_ws("fn t() -> float { return abs(-6.5) }");
        let s = emit_value_test(&ws, mid, "t", GlslProfile::Es300).expect("emit");
        assert!(s.fragment.starts_with("#version 300 es\n"), "{}", s.fragment);
        assert!(s.fragment.contains("precision highp float;"), "{}", s.fragment);
        assert!(s.vertex.starts_with("#version 300 es\n"), "{}", s.vertex);
    }

    #[test]
    fn parametered_root_rejects() {
        let (ws, mid) = build_ws("fn t(x: int) -> int { return x }");
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("params reject");
        assert!(e.message.contains("no parameters"), "{}", e.message);
    }

    #[test]
    fn non_scalar_return_rejects() {
        let (ws, mid) = build_ws("fn t() -> Vec2 { return vec2(1.0, 2.0) }");
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("vec reject");
        assert!(e.message.contains("not a scalar"), "{}", e.message);
    }

    #[test]
    fn body_outside_tier0_rejects_with_profile_reason() {
        // A string is well above tier 0 -- the fn library profile pass rejects,
        // surfacing the SKIP reason rather than emitting a guess.
        let (ws, mid) = build_ws("fn t() -> int { let s = \"hi\"; return 1 }");
        let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("string reject");
        assert!(!e.message.is_empty(), "reason present");
    }
}
