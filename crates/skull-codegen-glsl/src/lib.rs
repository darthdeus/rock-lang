//! skull-codegen-glsl -- GLSL function-library emission.
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
//! spec, keyed by the callee's mangled signature (name +
//! GLSL param types) -- the future `@intrinsic` GLSL column.

pub use skull_mir::shader_ir::GlslProfile;

use bone_core::hir::{AnnoValue, BuiltinTy, Function, StructDef, TyId, TypeCtor, TypeExpr};
use bone_core::prelude::{FnList, ModuleId, Span};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_codegen_shader::const_lit;
use skull_graph::instance_graph::{InstanceGraph, SiteOutcome};
pub use skull_graph::shader::ShaderLibraryView;
use skull_graph::shader::{ShaderProgram, ShaderProgramView, ShaderStageView};
use skull_mir::cfg::relooper::{LoopCtx, LoopTest, StructuredEmit};
use skull_mir::cfg::reverse_postorder;
use skull_mir::cfg::structure::{
    self, CountedBound, CountedLoop, LoopRegion, LoopShapes, Structure,
};
use skull_mir::concrete::{ConcreteBodyPlan, ConcretePlans, ConcreteSignature};
use skull_mir::gpu_layout::{
    ArrayMemberDecl, AttrKind, SamplerKind, UniformArrayBlockPlan, UniformBlockPlan, WireKind,
    texture_slots, uniform_array_block, vertex_attrs,
};
use skull_mir::instance::Instance;
use skull_mir::intrinsics::{Emission, IntrinsicEntry, IntrinsicTable};
use skull_mir::lower::{ScalarCtx, SpecSubst};
use skull_mir::mangle::{declared_name, mangle_fn, symbol_of_key};
use skull_mir::mir::*;
use skull_mir::monoty::MonoInterner;
use skull_mir::monoty::{MonoCtor, MonoTy};
use skull_mir::shader_ir::{
    FragmentOutput, LogicalArrayGroup, LogicalSampler, LogicalTextureGroup, LogicalUniformGroup,
    LogicalVarying, LogicalVertexInput, NominalType, ShaderIR, ShaderInstance, ShaderInterfacePlan,
    ShaderStage, ShaderStage as IrStage, ShaderStageIr, UniformRole, VertexRate,
};
use skull_parser::CompilerError;
use std::collections::BTreeSet;
use std::sync::Arc;
use ustr::Ustr;

mod abi;
#[cfg(test)]
mod assertion_flip_tests;
mod classify;
mod emit_fn;
mod entry;
mod fn_analysis;
mod fn_render;
mod intrinsics;
mod library;
mod pipeline;
mod profile;
mod structured;
mod value_test;
#[cfg(test)]
mod value_test_shader;

pub use abi::*;
use classify::*;
use emit_fn::*;
pub use entry::*;
pub use intrinsics::*;
pub use library::*;
pub use pipeline::*;

/// A program's planned GLSL, kept from the profile check so that emission
/// renders the same plan instead of planning the program a second time.
pub struct ShaderPlan(ShaderPlanKind);

enum ShaderPlanKind {
    Pipeline(Box<pipeline::PipelinePlan>),
    Entry(Box<entry::EntryPlan>),
}

/// Plan a program view: the profile check's whole work, whose result is
/// what [`render_shader_plan`] emits.
pub fn plan_shader_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<ShaderPlan, CompilerError> {
    let kind = match view.program.vert {
        Some(_) => ShaderPlanKind::Pipeline(Box::new(pipeline::plan_shader_pipeline_from_view(
            ws, scx, view,
        )?)),
        None => ShaderPlanKind::Entry(Box::new(entry::plan_shader_entry_program(ws, scx, view)?)),
    };
    Ok(ShaderPlan(kind))
}

/// Render a kept plan against the interner it was planned with.
pub fn render_shader_plan(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &ShaderPlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    match &plan.0 {
        ShaderPlanKind::Pipeline(plan) => {
            pipeline::render_shader_pipeline(ws, interner, plan, profile)
        }
        ShaderPlanKind::Entry(plan) => entry::render_shader_entry(ws, interner, plan, profile),
    }
}
use profile::*;
pub use value_test::*;

/// A tier-0 GLSL type. Everything else is rejected by the profile pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GlslTy {
    Float,
    Int,
    Uint,
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
            GlslTy::Uint => "uint",
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
        "Vec2" | "Vec2f" => Some(GlslTy::Vec2),
        "Vec3" | "Vec3f" => Some(GlslTy::Vec3),
        "Vec4" | "Vec4f" => Some(GlslTy::Vec4),
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
        MonoTy::Uint => Some(GlslTy::Uint),
        MonoTy::Bool => Some(GlslTy::Bool),
        MonoTy::App {
            ctor: MonoCtor::Nominal(n),
            ..
        } => nominal_glsl_ty(n.as_str()),
        _ => None,
    }
}

fn concrete_glsl_ty(
    interner: &MonoInterner,
    id: skull_mir::monoty::MonoTyId,
    span: Span,
    what: &str,
) -> Result<GlslTy, CompilerError> {
    let ty = interner.get(id);
    mono_glsl_ty(&ty).ok_or_else(|| {
        err(
            span,
            format!(
                "shader profile: {what} type `{}` is not tier 0 (only float/int/uint/bool/Vec2/Vec3/Vec4)",
                ty.render()
            ),
        )
    })
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
        err(
            span,
            format!(
                "shader profile: {what} type is not tier 0 (only float/int/uint/bool/Vec2/Vec3/Vec4)"
            ),
        )
    };
    match ty_expr(ws, ty) {
        Some(TypeExpr::Builtin(BuiltinTy::Float)) => Ok(GlslTy::Float),
        Some(TypeExpr::Builtin(BuiltinTy::Int)) => Ok(GlslTy::Int),
        Some(TypeExpr::Builtin(BuiltinTy::Uint)) => Ok(GlslTy::Uint),
        Some(TypeExpr::Builtin(BuiltinTy::Bool)) => Ok(GlslTy::Bool),
        Some(TypeExpr::Generic(var, name)) => match subst.get(&var) {
            Some(bound) => mono_glsl_ty(bound).ok_or_else(|| {
                err(span, format!(
                    "shader profile: {what} type `{name}` is bound to `{}`, which is not tier 0 \
                    (only float/int/uint/bool/Vec2/Vec3/Vec4)",
                    bound.render()
                ))
            }),
            None => Err(reject()),
        },
        Some(TypeExpr::App { ctor: TypeCtor::Nominal(n), .. }) => nominal_glsl_ty(n.as_str())
            .ok_or_else(|| err(span, format!(
                "shader profile: {what} type `{n}` is not tier 0 (only float/int/uint/bool/Vec2/Vec3/Vec4)"
            ))),
        _ => Err(reject()),
    }
}

/// Format an `f64` literal as GLSL, always keeping a decimal point (GLSL 330 is
/// strict: `float x = 1;` is an error, so `1.0` must never render as `1`).
fn fmt_float(f: f64) -> String {
    let s = format!("{f}");
    if s.contains('.')
        || s.contains('e')
        || s.contains('E')
        || s.contains("inf")
        || s.contains("nan")
    {
        s
    } else {
        format!("{s}.0")
    }
}

// --- const globals ----------------------------------------------------------

/// The GLSL type a folded const literal renders at.
fn const_lit_ty(lit: &Lit) -> GlslTy {
    match lit {
        Lit::Int(_) => GlslTy::Int,
        Lit::Uint(_) => GlslTy::Uint,
        Lit::Bool(_) => GlslTy::Bool,
        _ => GlslTy::Float,
    }
}

/// Render a folded const literal as a GLSL expression.
fn render_const_lit(lit: &Lit) -> Option<String> {
    match lit {
        Lit::Float(f) => Some(fmt_float(*f)),
        Lit::Int(i) => Some(i.to_string()),
        Lit::Uint(i) => Some(i.to_string()),
        Lit::Bool(b) => Some(b.to_string()),
        Lit::Nil | Lit::Omitted | Lit::Str(_) => None,
    }
}

// --- profile pass ------------------------------------------------------------

/// The GLSL name of a shader instance. The symbol is target-neutral
/// ([`ShaderInstance::symbol`]); this is the seam where a backend that needed a
/// different spelling would rename, and GLSL does not.
fn glsl_name(ws: &Workspace, interner: &MonoInterner, inst: &ShaderInstance) -> String {
    inst.symbol(ws, interner)
}

/// A cosmetic-tier fold of a lowered `&&`/`||`/expression-`if` diamond back into
/// one GLSL expression. The diamond stored its
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
    /// Shared vector calls retain their MIR evaluation point instead of being
    /// expanded once per swizzle component.
    value_name: HashMap<Idx<Inst>, String>,
    /// Loop headers matching the range-`for` shape -> their counted-loop info.
    counted: HashMap<Idx<Block>, CountedLoop>,
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
    /// Entry mode: sampler/array param cells, which render AS the declared
    /// global itself (`texture(t, ..)` reads `t`) and so keep their exact
    /// spelling while every other cell renames apart from the globals.
    global_ref_cells: HashSet<Idx<Cell>>,
    /// Entry mode: global names no local or value param may take (the
    /// program's whole declared-global surface, plus written builtins).
    reserved_names: HashSet<String>,
    ignored_type_cells: HashSet<Idx<Cell>>,
    /// Cosmetic folds. A diamond's
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
    concrete_plan: Option<&'a ConcreteBodyPlan>,
    /// The INSTANCE being emitted: this body's own generic bindings. Every
    /// declared type it renders resolves through this, so `T` is `float` here
    /// and `int` in the sibling instance of the same declaration.
    subst: Option<SpecSubst>,
    /// Every direct call in this body, resolved to its callee INSTANCE by
    /// [`callee_inst_at`]. Built once so the name a call renders and the type
    /// its result takes come from one answer.
    callee_insts: HashMap<Idx<Inst>, ShaderInstance>,
}
