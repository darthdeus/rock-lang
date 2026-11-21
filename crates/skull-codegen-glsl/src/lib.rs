//! Emit GLSL libraries and shader programs from concrete MIR.
//! Shader values remain first-class instead of using Lua's scalar-leaf ABI.

pub use skull_mir::shader_ir::GlslProfile;

use bone_core::hir::{AnnoValue, BuiltinTy, Function, StructDef, TyId, TypeCtor, TypeExpr};
use bone_core::prelude::{FnList, ModuleId, Span};
use bone_core::workspace::Workspace;
use comfy_la_arena::Idx;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use skull_graph::instance_graph::{InstanceGraph, SiteOutcome};
use skull_graph::shader::ShaderLibraryView;
use skull_graph::shader::{ShaderProgramView, ShaderStageView};
use skull_mir::cfg::relooper::{LoopCtx, LoopTest, StructuredEmit};
use skull_mir::cfg::reverse_postorder;
use skull_mir::cfg::structure::{self, CountedBound, CountedLoop, LoopRegion, Structure};
use skull_mir::concrete::{ConcreteBodyPlan, ConcretePlans, ConcreteSignature};
use skull_mir::gpu_layout::{
    ArrayMemberDecl, AttrKind, SamplerKind, TextureSlots, UniformArrayBlockPlan, UniformBlockPlan,
    WireKind, texture_slots, uniform_array_block, vertex_attrs,
};
use skull_mir::instance::Instance;
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
#[cfg(test)]
#[path = "../tests/workspace.rs"]
mod fixture_workspace;
mod fn_analysis;
mod fn_render;
mod intrinsics;
mod library;
mod pipeline;
mod profile;
mod structured;
mod value_test;

pub use abi::*;
use classify::*;
use emit_fn::*;
pub use entry::*;
pub use intrinsics::*;
pub use library::*;
use pipeline::*;
use profile::*;
pub use value_test::*;

/// A program's planned GLSL, kept from the profile check so that emission
/// renders the same plan instead of planning the program a second time.
pub struct ShaderPlan {
    interface: ShaderInterfacePlan,
    abi: ShaderAbiPlan,
    concrete_plans: Arc<ConcretePlans>,
    interner: Arc<MonoInterner>,
    vertex: Option<VertexStage>,
    fragment: (StagePlan, StageResources),
}

struct VertexStage {
    plan: StagePlan,
    attr_params: Vec<usize>,
    resources: StageResources,
}

pub fn plan_shader_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
) -> Result<ShaderPlan, CompilerError> {
    if view.program.vert.is_some() {
        plan_shader_pipeline_from_view(ws, scx, view)
    } else {
        plan_shader_entry_program(ws, scx, view)
    }
}

pub fn render_shader_plan(
    ws: &Workspace,
    interner: &MonoInterner,
    plan: &ShaderPlan,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let (frag, _) = &plan.fragment;
    let vertex = plan
        .vertex
        .as_ref()
        .map(|stage| emit_vertex_stage(ws, interner, plan, stage))
        .transpose()?;
    let pixel = emit_fragment_stage(ws, interner, plan)?;
    let version = profile_header(profile);
    let source = match vertex {
        Some(vertex) => format!(
            "{version}\n\n#ifdef VERTEX\n\n{vertex}\n#endif\n\n#ifdef PIXEL\n\n{pixel}\n#endif\n"
        ),
        None => format!(
            "{version}\n\n#ifdef PIXEL\n\n{pixel}\n#endif\n\n#ifdef VERTEX\n\n{FULLSCREEN_VERTEX}\n#endif\n"
        ),
    };
    let dependencies = plan.vertex.as_ref().map_or_else(
        || frag.dependencies.clone(),
        |vert| {
            vert.plan
                .dependencies
                .iter()
                .chain(&frag.dependencies)
                .copied()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect()
        },
    );
    let stages = plan
        .vertex
        .iter()
        .map(|vert| vert.plan.ir.clone())
        .chain([frag.ir.clone()])
        .collect();
    Ok(ShaderEmission {
        source,
        dependencies,
        interface: plan.interface.clone(),
        abi: plan.abi.clone(),
        ir: ShaderIR {
            interner: Arc::clone(&plan.interner),
            stages,
        },
    })
}

pub fn emit_shader_with_view(
    ws: &Workspace,
    scx: &ScalarCtx,
    view: &ShaderProgramView,
    profile: GlslProfile,
) -> Result<ShaderEmission, CompilerError> {
    let plan = plan_shader_with_view(ws, scx, view)?;
    render_shader_plan(ws, scx.interner(), &plan, profile)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GlslTy {
    Float,
    Int,
    Uint,
    Bool,
    Vec2,
    Vec3,
    Vec4,
    Bvec3,
    Mat4,
    Sampler2D,
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

fn ty_expr(ws: &Workspace, ty: TyId) -> Option<TypeExpr> {
    match ty {
        TyId::Static(m, idx) => Some(ws.module(m).ty_arena[idx]),
        TyId::Local(_) => None,
    }
}

fn builtin_glsl_ty(ty: BuiltinTy) -> Option<GlslTy> {
    match ty {
        BuiltinTy::Float => Some(GlslTy::Float),
        BuiltinTy::Int => Some(GlslTy::Int),
        BuiltinTy::Uint => Some(GlslTy::Uint),
        BuiltinTy::Bool => Some(GlslTy::Bool),
        _ => None,
    }
}

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
        Some(TypeExpr::Builtin(ty)) => builtin_glsl_ty(ty).ok_or_else(reject),
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

fn const_lit_ty(lit: &Lit) -> GlslTy {
    match lit {
        Lit::Int(_) => GlslTy::Int,
        Lit::Uint(_) => GlslTy::Uint,
        Lit::Bool(_) => GlslTy::Bool,
        _ => GlslTy::Float,
    }
}

fn render_const_lit(lit: &Lit) -> Option<String> {
    match lit {
        Lit::Float(f) => Some(fmt_float(*f)),
        Lit::Int(i) => Some(i.to_string()),
        Lit::Uint(i) => Some(i.to_string()),
        Lit::Bool(b) => Some(b.to_string()),
        Lit::Nil | Lit::Omitted | Lit::Str(_) => None,
    }
}

#[derive(Clone, Copy)]
struct FoldedDiamond {
    cond: Idx<Inst>,
    then_val: Idx<Inst>,
    else_val: Idx<Inst>,
    kind: FoldKind,
    result_ty: GlslTy,
}

#[derive(Clone, Copy, PartialEq)]
enum FoldKind {
    And,
    Or,
    Ternary,
}

struct FnEmit<'a> {
    ws: &'a Workspace,
    interner: &'a MonoInterner,
    f: &'a MirFunc,
    ret_ty: Option<GlslTy>,
    cell_name: HashMap<Idx<Cell>, String>,
    cell_ty: HashMap<Idx<Cell>, GlslTy>,
    value_name: HashMap<Idx<Inst>, String>,
    counted: HashMap<Idx<Block>, CountedLoop>,
    general: HashMap<Idx<Block>, LoopRegion>,
    structure: Structure,
    active: HashSet<Idx<Block>>,
    out: String,
    indent: usize,
    visited: HashSet<Idx<Block>>,
    global_cells: HashSet<Idx<Cell>>,
    uniform_fields: HashMap<Ustr, GlslTy>,
    global_ref_cells: HashSet<Idx<Cell>>,
    reserved_names: HashSet<String>,
    ignored_type_cells: HashSet<Idx<Cell>>,
    folded_cond: HashMap<Idx<Cell>, FoldedDiamond>,
    folded_block: HashMap<Idx<Block>, Idx<Cell>>,
    folded_bound: HashMap<Idx<Cell>, Idx<Inst>>,
    main_out: Option<MainOut>,
    /// The fn's name span -- body-level emit errors anchor here (MIR insts carry
    /// no spans, so per-expression precision is not available).
    fn_span: Span,
    concrete_plan: &'a ConcreteBodyPlan,
}
