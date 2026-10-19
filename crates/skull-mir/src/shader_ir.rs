//! The TARGET-NEUTRAL shader spine.
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
use std::fmt::Write as _;

/// Which GLSL dialect an artifact packages: the `#version`
/// line plus the builtin/precision differences that follow from it. `Core330`
/// is the desktop default; `Es300` is the mobile GLES target.
///
/// It lives on the NEUTRAL side because it is a property of the TARGET, not of
/// the emitter: `skull_graph::target_projection::ShaderTargetSpec` names it as
/// one axis of a target spec, and the graph layer must not depend on a backend
/// to spell its own axes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum GlslProfile {
    #[default]
    Core330,
    Es300,
}

use crate::gpu_layout::{ArrayMemberDecl, AttrKind, SamplerKind, WireKind};
use crate::instance::Instance;
use crate::mangle::{symbol_of_key, type_symbol};
use crate::mir::{FuncRef, Lit, MirFunc};
use crate::monoty::MonoInterner;
use bone_semantic::inference::Workspace;
use std::sync::Arc;

/// A logical pipeline stage. Compute joins this enum rather than arriving as a
/// GLSL qualifier or a string (plan Stage 3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ShaderStage {
    Vertex,
    Fragment,
    Compute,
}

impl ShaderStage {
    pub fn name(self) -> &'static str {
        match self {
            ShaderStage::Vertex => "vertex",
            ShaderStage::Fragment => "fragment",
            ShaderStage::Compute => "compute",
        }
    }
}

/// The scalar vocabulary needed by the first compiler-authored compute lane.
/// It is a logical value kind, not a target storage or byte-layout decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ComputeValueKind {
    F32,
    I32,
    U32,
    /// A storage element whose access is governed by WGSL atomic rules.
    /// Atomicity is an interface property, not a use-site reinterpretation of
    /// an ordinary `u32` resource.
    AtomicU32,
    Bool,
}

impl ComputeValueKind {
    fn code(self) -> u32 {
        match self {
            ComputeValueKind::F32 => 0,
            ComputeValueKind::I32 => 1,
            ComputeValueKind::U32 => 2,
            ComputeValueKind::AtomicU32 => 3,
            ComputeValueKind::Bool => 4,
        }
    }
}

/// Whether a compute resource is one value or an indexable homogeneous array.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ComputeResourceShape {
    Scalar(ComputeValueKind),
    Array(ComputeValueKind),
}

impl ComputeResourceShape {
    fn code(self) -> (u32, ComputeValueKind) {
        match self {
            ComputeResourceShape::Scalar(kind) => (0, kind),
            ComputeResourceShape::Array(kind) => (1, kind),
        }
    }
}

/// The logical address space of a compute resource. Physical bindings and
/// layout remain the target ABI's responsibility.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ComputeAddressSpace {
    Uniform,
    Storage,
}

impl ComputeAddressSpace {
    fn name(self) -> &'static str {
        match self {
            ComputeAddressSpace::Uniform => "uniform",
            ComputeAddressSpace::Storage => "storage",
        }
    }
}

/// Access promised by a compute resource declaration.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ComputeAccess {
    Read,
    Write,
    ReadWrite,
}

impl ComputeAccess {
    fn name(self) -> &'static str {
        match self {
            ComputeAccess::Read => "read",
            ComputeAccess::Write => "write",
            ComputeAccess::ReadWrite => "read_write",
        }
    }
}

/// Builtins a compute body may consume. Their target spellings are emitter
/// concerns; this list is the logical invocation contract.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ComputeBuiltin {
    GlobalInvocationId,
    LocalInvocationId,
    WorkgroupId,
    LocalInvocationIndex,
    NumWorkgroups,
}

impl ComputeBuiltin {
    fn name(self) -> &'static str {
        match self {
            ComputeBuiltin::GlobalInvocationId => "global_invocation_id",
            ComputeBuiltin::LocalInvocationId => "local_invocation_id",
            ComputeBuiltin::WorkgroupId => "workgroup_id",
            ComputeBuiltin::LocalInvocationIndex => "local_invocation_index",
            ComputeBuiltin::NumWorkgroups => "num_workgroups",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ComputeWorkgroupSize {
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

impl ComputeWorkgroupSize {
    pub fn is_valid(self) -> bool {
        self.x != 0 && self.y != 0 && self.z != 0
    }
}

/// How a runtime dispatch supplies its extent. The contract carries no
/// backend command object or binding number.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ComputeDispatchContract {
    Runtime { dimensions: u8 },
    Fixed { x: u32, y: u32, z: u32 },
}

impl ComputeDispatchContract {
    pub fn is_valid(self) -> bool {
        match self {
            ComputeDispatchContract::Runtime { dimensions } => (1..=3).contains(&dimensions),
            ComputeDispatchContract::Fixed { x, y, z } => x != 0 && y != 0 && z != 0,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct LogicalComputeResource {
    pub name: Ustr,
    pub shape: ComputeResourceShape,
    pub address_space: ComputeAddressSpace,
    pub access: ComputeAccess,
}

/// One compiler-owned workgroup-memory declaration. It is not a bindable
/// resource: the neutral plan retains its element kind and fixed population so
/// a target cannot reconstruct it from source text or a second side table.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct LogicalComputeWorkgroup {
    pub name: Ustr,
    pub kind: ComputeValueKind,
    pub size: u32,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ComputeInterfacePlan {
    pub resources: Vec<LogicalComputeResource>,
    pub workgroup: Vec<LogicalComputeWorkgroup>,
    pub builtins: Vec<ComputeBuiltin>,
    pub workgroup_size: ComputeWorkgroupSize,
    pub dispatch: ComputeDispatchContract,
}

impl ComputeInterfacePlan {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.workgroup_size.is_valid() {
            return Err("compute workgroup size must be non-zero");
        }
        if !self.dispatch.is_valid() {
            return Err("compute dispatch contract is invalid");
        }
        if self.resources.iter().any(|resource| {
            resource.address_space == ComputeAddressSpace::Uniform
                && resource.access != ComputeAccess::Read
        }) {
            return Err("uniform compute resources are read-only");
        }
        if self.resources.iter().any(|resource| {
            let atomic = matches!(
                resource.shape,
                ComputeResourceShape::Array(ComputeValueKind::AtomicU32)
                    | ComputeResourceShape::Scalar(ComputeValueKind::AtomicU32)
            );
            atomic
                && (resource.address_space != ComputeAddressSpace::Storage
                    || resource.shape != ComputeResourceShape::Array(ComputeValueKind::AtomicU32)
                    || resource.access != ComputeAccess::ReadWrite)
        }) {
            return Err("atomic compute resources must be read_write storage arrays");
        }
        if self.workgroup.iter().any(|memory| {
            memory.size == 0 || matches!(memory.kind, ComputeValueKind::AtomicU32)
        }) {
            return Err("compute workgroup arrays must have a positive non-atomic element kind");
        }
        Ok(())
    }
}

/// **One shader INSTANCE**: a declaration plus the instantiation a caller
/// reached it at.
///
/// The reachability node, the body identity and the emitted name all come off
/// this one value, so two instantiations of one generic helper are two
/// functions and cannot be confused for one.
///
/// It IS the [`Instance`] every other emitter keys on -- a shader instance is
/// an emitted body, one checked definition per instantiation, which is what
/// `unchecked_generic` being deleted rests on. The wrapper survives only to
/// carry GLSL's own vocabulary at the type level.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ShaderInstance {
    pub inst: Instance,
}

impl ShaderInstance {
    pub fn new(inst: Instance) -> Self {
        ShaderInstance { inst }
    }

    /// The declaration's own shape -- an entry, or a helper reached with no
    /// instantiation.
    pub fn plain(fref: FuncRef) -> Self {
        ShaderInstance { inst: Instance::base(fref) }
    }

    pub fn fref(&self) -> FuncRef {
        self.inst.func
    }

    pub fn subst(
        &self,
        ws: &Workspace,
        interner: &crate::monoty::MonoInterner,
    ) -> crate::lower::SpecSubst {
        self.inst.subst(ws, interner)
    }

    /// The symbol this instance is emitted under. Target-neutral: it is the
    /// mangled declaration plus the instantiation, and a backend that needs a
    /// different spelling renames from here rather than re-deriving it.
    pub fn symbol(&self, ws: &Workspace, interner: &crate::monoty::MonoInterner) -> String {
        self.inst.symbol(ws, interner)
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
    /// Literals proven by the target-neutral shader planner for immutable
    /// globals read by this stage.
    pub const_globals: rustc_hash::FxHashMap<Ustr, Lit>,
}

impl ShaderStageIr {
    /// The DECLARATIONS this stage's bodies came from -- what a file watcher
    /// reloads. Two instances of one generic helper share one declaration.
    ///
    /// Deduped by SET, not by `Vec::dedup`: emission order interleaves a
    /// generic helper's instantiations with whatever each of them reached, so
    /// the repeats are not neighbours and an adjacent-only dedup left them in.
    /// Emission ORDER is preserved for the survivors, because a reload list
    /// that reordered itself run to run is a diff for no reason.
    pub fn declarations(&self) -> Vec<FuncRef> {
        let mut seen: rustc_hash::FxHashSet<(u32, u32)> = rustc_hash::FxHashSet::default();
        let mut out = Vec::new();
        for f in self.order.iter().map(|i| i.fref()).chain(std::iter::once(self.entry.fref())) {
            if seen.insert((f.module.raw(), f.func.into_raw().into_u32())) {
                out.push(f);
            }
        }
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
    /// The arena that minted every `Instance` in these stages. A target emitter
    /// must use this same handle for specialization symbols and substitutions.
    pub interner: Arc<MonoInterner>,
    pub stages: Vec<ShaderStageIr>,
}

impl ShaderIR {
    /// Every DECLARATION the program's bodies came from -- the hot-reload set.
    /// Deduped across stages: a helper both stages reach is one file to reload.
    pub fn declarations(&self) -> Vec<FuncRef> {
        let mut seen: rustc_hash::FxHashSet<(u32, u32)> = rustc_hash::FxHashSet::default();
        let mut out = Vec::new();
        for f in self.stages.iter().flat_map(|s| s.declarations()) {
            if seen.insert((f.module.raw(), f.func.into_raw().into_u32())) {
                out.push(f);
            }
        }
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

/// A nominal type an interface names. `key` is its DefKey, which lookups and
/// every fingerprint read; `symbol` is what shader text and every wire spell.
/// Both come from [`NominalType::of`], so the symbol is always
/// [`type_symbol`]'s: a `@nomangle` type keeps its declared name here exactly
/// as it does on the Lua side.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NominalType {
    pub key: Ustr,
    pub symbol: Ustr,
}

impl NominalType {
    pub fn of(ws: &Workspace, key: Ustr) -> Self {
        Self { key, symbol: type_symbol(ws, key) }
    }
}

/// A uniform group: its role, the struct that declares it, and its members in
/// declaration order with neutral kinds. No offsets -- a member's PLACE is the
/// target's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalUniformGroup {
    pub role: UniformRole,
    pub struct_name: NominalType,
    pub members: Vec<(Ustr, WireKind)>,
}

/// The identity of a logical uniform value group. Physical offsets, padding,
/// bindings and block size are deliberately absent.
pub fn semantic_uniform_group_fingerprint(
    struct_name: Ustr,
    members: &[(Ustr, WireKind)],
) -> u64 {
    let mut hash = 0xa5a5_5a5a_c3c3_3c3cu64;
    for byte in struct_name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for (name, kind) in members {
        for byte in name.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= u64::from(kind.code());
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash ^ (hash >> 53)) & ((1 << 53) - 1)
}

impl LogicalUniformGroup {
    pub fn semantic_fingerprint(&self) -> u64 {
        semantic_uniform_group_fingerprint(self.struct_name.key, &self.members)
    }
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

impl LogicalArrayGroup {
    /// The group's name in every artifact wire: the runtime joins the semantic
    /// interface's array packets to a target's physical block by it.
    pub fn symbol(&self) -> Ustr {
        symbol_of_key(self.owner)
    }

    /// The physical block every target declares for the group. Per stage
    /// entry, so a program's two array blocks cannot collide; a clash with a
    /// user `@uniform` group is rejected by the target, never renamed around.
    pub fn block_name(&self) -> Ustr {
        Ustr::from(&format!("{}_Arrays", self.symbol()))
    }
}

/// The program's `@textures` group: its samplers in the order the group
/// declares them. ORDER is logical; the unit each one lands on is the target's
/// answer, and it is COMPILER-assigned -- which is the whole difference from a
/// bare sampler ([`ShaderInterfacePlan::bare_samplers`]).
///
/// `stages` is which stages READ the group. One group per program, but a
/// resource is declared per stage on every target with separate stage units,
/// so "the program has it" is not enough to emit from -- a stage that samples
/// and was not listed emits a reference to an identifier it never declared.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalTextureGroup {
    pub struct_name: Option<NominalType>,
    pub samplers: Vec<(Ustr, SamplerKind)>,
    pub stages: Vec<ShaderStage>,
}

/// A loose sampler and the stage that declares it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalSampler {
    pub name: Ustr,
    pub kind: SamplerKind,
    pub stage: ShaderStage,
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
    pub struct_name: NominalType,
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
    pub uniform_groups: Vec<LogicalUniformGroup>,
    pub array_groups: Vec<LogicalArrayGroup>,
    pub textures: Option<LogicalTextureGroup>,
    /// Loose `Sampler2D` params, outside any `@textures` group, each with the
    /// stage that declares it.
    ///
    /// Kept APART from `textures` because the difference is not cosmetic: a
    /// grouped sampler's unit is compiler-assigned and published in the ABI
    /// plan, while a bare one keeps a unit the CPU picked. Folding them into
    /// one list would tell the engine it owns units it does not. A program may
    /// not mix the two, which is a rejection rather than a precedence.
    pub bare_samplers: Vec<LogicalSampler>,
    /// The logical compute contract when `stages` contains `Compute`.
    pub compute: Option<ComputeInterfacePlan>,
    /// How the fragment stage's colour output is shaped.
    pub fragment_output: FragmentOutput,
    /// The fragment entry's output struct carries a `FragDepth: float` field
    /// (window-space depth in [0,1]). It is not a colour output, so it is
    /// absent from `fragment_output` and from the semantic wire: the engine
    /// binds nothing for it, and its colour-attachment count must not see it.
    pub frag_depth: bool,
}

/// The fragment stage's output shape.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum FragmentOutput {
    /// One implicit colour output of this kind. `F32x4` is the ordinary case;
    /// an integer target (R32I picking) returns `I32`.
    Single(WireKind),
    /// MRT: one named output per field, in declaration order.
    Mrt(Vec<(Ustr, WireKind)>),
    /// The stage has no colour output of its own (a vertex-only interface).
    #[default]
    None,
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

    /// Every sampler `stage` reads, grouped ones first (in group order), then
    /// its bare ones. THE per-stage resource list: a stage declares exactly
    /// this, and types its bare-global reads from exactly this.
    pub fn stage_samplers(&self, stage: ShaderStage) -> Vec<(Ustr, SamplerKind)> {
        let mut out: Vec<(Ustr, SamplerKind)> = self
            .textures
            .iter()
            .filter(|t| t.stages.contains(&stage))
            .flat_map(|t| t.samplers.iter().copied())
            .collect();
        out.extend(
            self.bare_samplers.iter().filter(|s| s.stage == stage).map(|s| (s.name, s.kind)),
        );
        out
    }

    /// A stable identity for the logical interface plus the geometry form the
    /// caller is allowed to submit. This deliberately hashes names, roles,
    /// order and neutral kinds only; target bindings, offsets, sizes and
    /// source text are not part of the semantic interface.
    pub fn semantic_fingerprint(&self, fullscreen: bool) -> u64 {
        let payload = self.semantic_payload(fullscreen);
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in payload.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let folded = hash ^ (hash >> 53);
        folded & ((1 << 53) - 1)
    }

    /// Serialize the compiler-owned logical interface for a runtime
    /// registration. It is intentionally a separate wire from every target's
    /// physical ABI wire; the runtime never parses GLSL or WGSL to recover it.
    pub fn semantic_wire(&self, fullscreen: bool) -> String {
        format!(
            "SHADER_INTERFACE_V1\nfingerprint {}\n{}end\n",
            self.semantic_fingerprint(fullscreen),
            self.semantic_payload(fullscreen)
        )
    }

    /// Type names are written as their emitted symbols: the runtime joins a
    /// uniform group to its live physical block by name, and every target's
    /// physical wire names blocks by symbol. Fingerprints keep the logical key.
    fn semantic_payload(&self, fullscreen: bool) -> String {
        let mut out = String::new();
        writeln!(out, "fullscreen {}", if fullscreen { 1 } else { 0 }).unwrap();

        writeln!(out, "stages {}", self.stages.len()).unwrap();
        for stage in &self.stages {
            writeln!(out, "stage {}", stage.name()).unwrap();
        }

        writeln!(out, "vertex_inputs {}", self.vertex_inputs.len()).unwrap();
        for input in &self.vertex_inputs {
            let rate = match input.rate {
                VertexRate::PerVertex => "vertex",
                VertexRate::PerInstance => "instance",
            };
            writeln!(out, "vertex_input {} {} {}", input.struct_name.symbol, rate, input.attrs.len()).unwrap();
            for (name, kind) in &input.attrs {
                writeln!(out, "attr {} {}", name, attr_kind_code(*kind)).unwrap();
            }
        }

        writeln!(out, "varyings {}", self.varyings.len()).unwrap();
        for varying in &self.varyings {
            writeln!(out, "varying {} {} {}", varying.name, varying.kind.code(), if varying.flat { 1 } else { 0 }).unwrap();
        }

        writeln!(out, "groups {}", self.uniform_groups.len()).unwrap();
        for group in &self.uniform_groups {
            writeln!(out, "group {} {} {} {}", uniform_role_name(group.role), group.struct_name.symbol, group.semantic_fingerprint(), group.members.len()).unwrap();
            for (name, kind) in &group.members {
                writeln!(out, "member {} {}", name, kind.code()).unwrap();
            }
        }

        writeln!(out, "arrays {}", self.array_groups.len()).unwrap();
        for group in &self.array_groups {
            writeln!(out, "array {} {} {}", group.stage.name(), group.symbol(), group.members.len()).unwrap();
            for member in &group.members {
                writeln!(out, "array_member {} {} {}", member.name, member.elem.code(), member.capacity).unwrap();
            }
        }

        match &self.compute {
            Some(compute) => {
                writeln!(out, "compute some").unwrap();
                writeln!(out, "compute_workgroup {} {} {}", compute.workgroup_size.x, compute.workgroup_size.y, compute.workgroup_size.z).unwrap();
                match compute.dispatch {
                    ComputeDispatchContract::Runtime { dimensions } => {
                        writeln!(out, "compute_dispatch runtime {dimensions}").unwrap();
                    }
                    ComputeDispatchContract::Fixed { x, y, z } => {
                        writeln!(out, "compute_dispatch fixed {x} {y} {z}").unwrap();
                    }
                }
                writeln!(out, "compute_builtins {}", compute.builtins.len()).unwrap();
                for builtin in &compute.builtins {
                    writeln!(out, "compute_builtin {}", builtin.name()).unwrap();
                }
                writeln!(out, "compute_resources {}", compute.resources.len()).unwrap();
                for resource in &compute.resources {
                    let (shape, kind) = resource.shape.code();
                    writeln!(out, "compute_resource {} {} {} {} {}", resource.name, resource.address_space.name(), resource.access.name(), shape, kind.code()).unwrap();
                }
                writeln!(out, "compute_workgroup_arrays {}", compute.workgroup.len()).unwrap();
                for memory in &compute.workgroup {
                    writeln!(out, "compute_workgroup_array {} {} {}", memory.name, memory.kind.code(), memory.size).unwrap();
                }
            }
            None => {}
        }

        match &self.textures {
            Some(group) => {
                let struct_name = group.struct_name.map(|name| name.symbol.as_str()).unwrap_or("none");
                writeln!(out, "textures {} {}", struct_name, group.samplers.len()).unwrap();
                for (name, kind) in &group.samplers {
                    writeln!(out, "texture {} {}", name, sampler_kind_code(*kind)).unwrap();
                }
                writeln!(out, "texture_stages {}", group.stages.len()).unwrap();
                for stage in &group.stages {
                    writeln!(out, "texture_stage {}", stage.name()).unwrap();
                }
            }
            None => {
                writeln!(out, "textures none 0").unwrap();
                writeln!(out, "texture_stages 0").unwrap();
            }
        }

        writeln!(out, "bare_samplers {}", self.bare_samplers.len()).unwrap();
        for sampler in &self.bare_samplers {
            writeln!(out, "bare_sampler {} {} {}", sampler.stage.name(), sampler.name, sampler_kind_code(sampler.kind)).unwrap();
        }

        match &self.fragment_output {
            FragmentOutput::None => writeln!(out, "output none").unwrap(),
            FragmentOutput::Single(kind) => writeln!(out, "output single {}", kind.code()).unwrap(),
            FragmentOutput::Mrt(outputs) => {
                writeln!(out, "output mrt {}", outputs.len()).unwrap();
                for (name, kind) in outputs {
                    writeln!(out, "output_member {} {}", name, kind.code()).unwrap();
                }
            }
        }
        out
    }
}

fn uniform_role_name(role: UniformRole) -> &'static str {
    match role {
        UniformRole::Engine => "engine",
        UniformRole::Stage(ShaderStage::Vertex) => "vertex",
        UniformRole::Stage(ShaderStage::Fragment) => "fragment",
        UniformRole::Stage(ShaderStage::Compute) => "compute",
    }
}

fn sampler_kind_code(kind: SamplerKind) -> u32 {
    match kind {
        SamplerKind::Float => 0,
        SamplerKind::Int => 1,
    }
}

fn attr_kind_code(kind: AttrKind) -> u32 {
    match kind {
        AttrKind::F32 => 0,
        AttrKind::F32x2 => 1,
        AttrKind::F32x3 => 2,
        AttrKind::F32x4 => 3,
        AttrKind::I32 => 4,
    }
}
