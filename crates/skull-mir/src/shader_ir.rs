//! The TARGET-NEUTRAL shader spine (`docs/monomorphization-plan.md` Stage 3
//! first slice, `docs/shader-binding-layout-plan.md` § Backend portability).
//!
//! Two objects, and the split between them is the point:
//!
//! - [`ShaderIR`] is what a program's concrete instances lower TO: per stage,
//!   the entry instance and the helper instances it reaches, in emission order,
//!   with their lowered MIR. Bodies, not bindings.
//! - [`ShaderInterfacePlan`] is what a program's signature MEANS: which logical
//!   stages exist, what flows between them, and which logical resources they
//!   read. Roles and neutral kinds, never a physical answer.
//!
//! **Neither may name a physical layout.** No byte offsets, no binding indices,
//! no texture units, no `std140`, no GLSL/WGSL/MSL type spellings. Those are a
//! TARGET's answer to this interface, and a target states them in its own ABI
//! plan (`skull_codegen_glsl::ShaderAbiPlan` is the first). The kinds here are
//! [`AttrKind`] / [`WireKind`] / [`SamplerKind`] -- scalar kind plus arity, the
//! vocabulary `crate::gpu_layout` already publishes for exactly this reason.
//!
//! The test that keeps the split honest is a HOSTILE second target: a synthetic
//! ABI derivation that assigns different bindings and units from the SAME
//! interface plan. If that cannot produce a different physical answer, the
//! interface plan was a GLSL plan wearing another name.

use ustr::Ustr;

use crate::gpu_layout::{ArrayMemberDecl, AttrKind, SamplerKind, WireKind};
use crate::lower::SpecKey;
use crate::mangle::spec_fn_name;
use crate::mir::{FuncRef, MirFunc};
use bone_semantic::inference::Workspace;

/// A logical pipeline stage. Compute joins this enum rather than arriving as a
/// GLSL qualifier or a string (plan Stage 3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ShaderStage {
    Vertex,
    Fragment,
}

impl ShaderStage {
    pub fn name(self) -> &'static str {
        match self {
            ShaderStage::Vertex => "vertex",
            ShaderStage::Fragment => "fragment",
        }
    }
}

/// **One shader INSTANCE**: a declaration plus the instantiation a caller
/// reached it at.
///
/// The reachability node, the body key and the emitted name all come off this
/// one value, so two instantiations of one generic helper are two functions and
/// cannot be confused for one. An all-`None` key is the non-generic case and
/// keeps the declaration's own name ([`spec_fn_name`] returns `None` for it).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ShaderInstance {
    pub fref: FuncRef,
    pub key: SpecKey,
}

impl ShaderInstance {
    /// The declaration's own shape -- an entry, or a helper reached with no
    /// instantiation.
    pub fn plain(fref: FuncRef) -> Self {
        ShaderInstance { fref, key: Vec::new() }
    }

    pub fn subst(&self, ws: &Workspace) -> crate::lower::SpecSubst {
        crate::lower::spec_subst_from_key(ws, self.fref, &self.key)
    }

    /// The symbol this instance is emitted under. Target-neutral: it is the
    /// mangled declaration plus the instantiation, and a backend that needs a
    /// different spelling renames from here rather than re-deriving it.
    pub fn symbol(&self, ws: &Workspace) -> String {
        let f = &ws.module(self.fref.module).functions[self.fref.func];
        spec_fn_name(ws, f, &self.key).unwrap_or_else(|| crate::mangle::mangle_fn(ws, f))
    }
}

/// One stage's lowered bodies, in emission order.
#[derive(Clone)]
pub struct ShaderStageIr {
    pub stage: ShaderStage,
    pub entry: ShaderInstance,
    /// The entry's own lowered body.
    pub body: MirFunc,
    /// Helper instances in post-order (callees before callers), and their
    /// bodies. Post-order is a property of the call graph, not of a backend --
    /// GLSL happens to need it because it has no forward declarations.
    pub order: Vec<ShaderInstance>,
    pub bodies: rustc_hash::FxHashMap<ShaderInstance, MirFunc>,
}

impl ShaderStageIr {
    /// The DECLARATIONS this stage's bodies came from -- what a file watcher
    /// reloads. Two instances of one generic helper share one declaration.
    pub fn declarations(&self) -> Vec<FuncRef> {
        let mut out: Vec<FuncRef> = self.order.iter().map(|i| i.fref).collect();
        out.push(self.entry.fref);
        out.dedup();
        out
    }
}

/// **A program's concrete bodies, per stage** -- what its instances lower TO.
///
/// The half of a shader program a non-GLSL backend needs beside
/// [`ShaderInterfacePlan`]: typed MIR under each instance's own substitution,
/// in emission order, with no target in it. Published on an emission so a
/// second backend renders these rather than re-walking the entry.
#[derive(Clone)]
pub struct ShaderIR {
    pub stages: Vec<ShaderStageIr>,
}

impl ShaderIR {
    /// Every DECLARATION the program's bodies came from -- the hot-reload set.
    pub fn declarations(&self) -> Vec<FuncRef> {
        let mut out: Vec<FuncRef> = self.stages.iter().flat_map(|s| s.declarations()).collect();
        out.sort_by_key(|f| (f.module.raw(), f.func.into_raw().into_u32()));
        out.dedup();
        out
    }
}

/// Why a uniform group exists, which is what decides where a target puts it.
///
/// `Engine` is one group per PROGRAM and both stages must name the same type:
/// it is engine-owned and fixed, bound once per pass. A `Stage` group is one
/// per stage, because the stages declare different types and a vertex entry is
/// reused across several fragment entries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UniformRole {
    Engine,
    Stage(ShaderStage),
}

/// A uniform group: its role, the struct that declares it, and its members in
/// declaration order with neutral kinds. No offsets -- a member's PLACE is the
/// target's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalUniformGroup {
    pub role: UniformRole,
    pub struct_name: Ustr,
    pub members: Vec<(Ustr, WireKind)>,
}

/// A stage's `UniformArray<T>` members. Separate from the scalar group for a
/// LIFETIME reason rather than a typing one -- see `ShaderAbiPlan`'s note on
/// whole-block uploads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalArrayGroup {
    pub stage: ShaderStage,
    /// The stage entry the group belongs to, which is what names it.
    pub owner: Ustr,
    pub members: Vec<ArrayMemberDecl>,
}

/// The program's samplers, in the order the group declares them. ORDER is
/// logical; the unit each one lands on is the target's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalTextureGroup {
    pub struct_name: Option<Ustr>,
    pub samplers: Vec<(Ustr, SamplerKind)>,
}

/// How often a vertex input advances.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VertexRate {
    PerVertex,
    PerInstance,
}

/// One vertex input struct and its attributes in declaration order. The
/// LOCATION each attribute takes is the target's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalVertexInput {
    pub struct_name: Ustr,
    pub rate: VertexRate,
    pub attrs: Vec<(Ustr, AttrKind)>,
}

/// A value passed from the vertex stage to the fragment stage.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalVarying {
    pub name: Ustr,
    pub kind: WireKind,
    /// Declared `@flat`: the value is not interpolated. A property of the
    /// value, which every target must honour somehow; how it SAYS so is the
    /// target's answer.
    pub flat: bool,
}

/// **What a shader program's interface MEANS, with no target in it.**
///
/// Everything here is a role, a name, an order or a neutral kind. A target
/// turns it into bindings, offsets, locations and units; two targets may
/// legitimately answer differently, which is what makes this worth separating
/// from the answer.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ShaderInterfacePlan {
    pub stages: Vec<ShaderStage>,
    pub vertex_inputs: Vec<LogicalVertexInput>,
    pub varyings: Vec<LogicalVarying>,
    /// Fragment outputs in declaration order, or empty for the single implicit
    /// colour output.
    pub outputs: Vec<(Ustr, WireKind)>,
    pub uniform_groups: Vec<LogicalUniformGroup>,
    pub array_groups: Vec<LogicalArrayGroup>,
    pub textures: Option<LogicalTextureGroup>,
}

impl ShaderInterfacePlan {
    pub fn uniform_group(&self, role: UniformRole) -> Option<&LogicalUniformGroup> {
        self.uniform_groups.iter().find(|g| g.role == role)
    }

    pub fn array_group(&self, stage: ShaderStage) -> Option<&LogicalArrayGroup> {
        self.array_groups.iter().find(|g| g.stage == stage)
    }

    /// The sampler names in declaration order, which is the order a target
    /// assigns units in. Empty when the program declares no group.
    pub fn sampler_names(&self) -> Vec<Ustr> {
        self.textures.as_ref().map(|t| t.samplers.iter().map(|(n, _)| *n).collect()).unwrap_or_default()
    }
}
