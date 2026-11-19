//! Neither [`ShaderIR`] nor [`ShaderInterfacePlan`] may name a physical layout:
//! no byte offsets, binding indices, texture units, `std140` or target type
//! spellings. Those are a target's answer, stated in its own ABI plan.
use std::fmt::Write as _;
use ustr::Ustr;

/// Lives on the neutral side because it is a target property, and the graph
/// layer must not depend on a backend to spell its own target axes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum GlslProfile {
    #[default]
    Core330,
}

use crate::gpu_layout::{ArrayMemberDecl, AttrKind, SamplerKind, WireKind};
use crate::instance::Instance;
use crate::mangle::{symbol_of_key, type_symbol};
use crate::mir::{FuncRef, Lit, MirFunc};
use crate::monoty::MonoInterner;
use bone_core::workspace::Workspace;
use std::sync::Arc;

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

/// The reachability node, body identity and emitted name all come off this one
/// value, so two instantiations of one generic helper are two functions.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ShaderInstance {
    pub inst: Instance,
}

impl ShaderInstance {
    pub fn new(inst: Instance) -> Self {
        ShaderInstance { inst }
    }

    pub fn plain(fref: FuncRef) -> Self {
        ShaderInstance {
            inst: Instance::base(fref),
        }
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

    /// Target-neutral: a backend that needs a different spelling renames from here
    /// rather than re-deriving it.
    pub fn symbol(&self, ws: &Workspace, interner: &crate::monoty::MonoInterner) -> String {
        self.inst.symbol(ws, interner)
    }
}

#[derive(Clone)]
pub struct ShaderStageIr {
    pub stage: ShaderStage,
    pub entry: ShaderInstance,
    pub body: MirFunc,
    /// Post-order (callees before callers): GLSL has no forward declarations.
    pub order: Vec<ShaderInstance>,
    pub bodies: rustc_hash::FxHashMap<ShaderInstance, MirFunc>,
    pub const_globals: rustc_hash::FxHashMap<Ustr, Lit>,
}

impl ShaderStageIr {
    /// Deduped by set, since a generic helper's repeats are not neighbours; emission
    /// order is kept so the reload list is stable run to run.
    pub fn declarations(&self) -> Vec<FuncRef> {
        let mut seen: rustc_hash::FxHashSet<(u32, u32)> = rustc_hash::FxHashSet::default();
        let mut out = Vec::new();
        for f in self
            .order
            .iter()
            .map(|i| i.fref())
            .chain(std::iter::once(self.entry.fref()))
        {
            if seen.insert((f.module.raw(), f.func.into_raw().into_u32())) {
                out.push(f);
            }
        }
        out
    }
}

/// Published on an emission so a second backend renders these bodies rather
/// than re-walking the entry.
#[derive(Clone)]
pub struct ShaderIR {
    /// The arena that minted every `Instance` here; a target emitter must use this
    /// same handle for specialization symbols and substitutions.
    pub interner: Arc<MonoInterner>,
    pub stages: Vec<ShaderStageIr>,
}

impl ShaderIR {
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

/// `Engine` is one group per program, and both stages must name the same type.
/// A `Stage` group is one per stage: a vertex entry is reused across fragments.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UniformRole {
    Engine,
    Stage(ShaderStage),
}

/// `key` is the DefKey that lookups and fingerprints read; `symbol` is what
/// shader text and wires spell, and keeps a `@nomangle` declared name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NominalType {
    pub key: Ustr,
    pub symbol: Ustr,
}

impl NominalType {
    pub fn of(ws: &Workspace, key: Ustr) -> Self {
        Self {
            key,
            symbol: type_symbol(ws, key),
        }
    }
}

/// No offsets: a member's place is the target's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalUniformGroup {
    pub role: UniformRole,
    pub struct_name: NominalType,
    pub members: Vec<(Ustr, WireKind)>,
}

/// Physical offsets, padding, bindings and block size are deliberately absent.
pub fn semantic_uniform_group_fingerprint(struct_name: Ustr, members: &[(Ustr, WireKind)]) -> u64 {
    let mut hash = 0xa5a5_5a5a_c3c3_3c3cu64;
    for byte in bone_core::modules::artifact_name(struct_name.as_str()).as_bytes() {
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

/// Separate from the scalar group for a lifetime reason (whole-block uploads),
/// not a typing one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalArrayGroup {
    pub stage: ShaderStage,
    pub owner: Ustr,
    pub members: Vec<ArrayMemberDecl>,
}

impl LogicalArrayGroup {
    /// The runtime joins the semantic interface's array packets to a target's
    /// physical block by this name.
    pub fn symbol(&self) -> Ustr {
        symbol_of_key(self.owner)
    }

    /// Per stage entry, so a program's two array blocks cannot collide; a clash
    /// with a user `@uniform` group is rejected by the target, never renamed.
    pub fn block_name(&self) -> Ustr {
        Ustr::from(&format!("{}_Arrays", self.symbol()))
    }
}

/// Sampler order is logical; the compiler assigns units. `stages` lists the
/// readers because a stage that samples unlisted would reference an undeclared
/// identifier.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalTextureGroup {
    pub struct_name: Option<NominalType>,
    pub samplers: Vec<(Ustr, SamplerKind)>,
    pub stages: Vec<ShaderStage>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalSampler {
    pub name: Ustr,
    pub kind: SamplerKind,
    pub stage: ShaderStage,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VertexRate {
    PerVertex,
    PerInstance,
}

/// The location each attribute takes is the target's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalVertexInput {
    pub struct_name: NominalType,
    pub rate: VertexRate,
    pub attrs: Vec<(Ustr, AttrKind)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogicalVarying {
    pub name: Ustr,
    pub kind: WireKind,
    /// Not interpolated: every target must honour it; how it says so is its own.
    pub flat: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ShaderInterfacePlan {
    pub stages: Vec<ShaderStage>,
    pub vertex_inputs: Vec<LogicalVertexInput>,
    pub varyings: Vec<LogicalVarying>,
    pub uniform_groups: Vec<LogicalUniformGroup>,
    pub array_groups: Vec<LogicalArrayGroup>,
    pub textures: Option<LogicalTextureGroup>,
    /// Kept apart from `textures`: a grouped sampler's unit is compiler-assigned,
    /// while a bare one keeps a unit the CPU picked. A program may not mix them.
    pub bare_samplers: Vec<LogicalSampler>,
    pub fragment_output: FragmentOutput,
    /// Not a colour output, so absent from `fragment_output` and the semantic
    /// wire; the colour-attachment count must not see it.
    pub frag_depth: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum FragmentOutput {
    /// An integer target (R32I picking) returns `I32`.
    Single(WireKind),
    /// MRT: one named output per field, in declaration order.
    Mrt(Vec<(Ustr, WireKind)>),
    #[default]
    None,
}

impl ShaderInterfacePlan {
    pub fn uniform_group(&self, role: UniformRole) -> Option<&LogicalUniformGroup> {
        self.uniform_groups.iter().find(|g| g.role == role)
    }

    /// Declaration order, which is the order a target assigns units in.
    pub fn sampler_names(&self) -> Vec<Ustr> {
        self.textures
            .as_ref()
            .map(|t| t.samplers.iter().map(|(n, _)| *n).collect())
            .unwrap_or_default()
    }

    /// Grouped samplers first (in group order), then bare ones. A stage declares
    /// exactly this and types its bare-global reads from it.
    pub fn stage_samplers(&self, stage: ShaderStage) -> Vec<(Ustr, SamplerKind)> {
        let mut out: Vec<(Ustr, SamplerKind)> = self
            .textures
            .iter()
            .filter(|t| t.stages.contains(&stage))
            .flat_map(|t| t.samplers.iter().copied())
            .collect();
        out.extend(
            self.bare_samplers
                .iter()
                .filter(|s| s.stage == stage)
                .map(|s| (s.name, s.kind)),
        );
        out
    }

    /// Hashes names, roles, order and neutral kinds only; target bindings,
    /// offsets, sizes and source text are not part of the semantic interface.
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

    /// A separate wire from every target's physical ABI wire; the runtime never
    /// parses GLSL or WGSL to recover it.
    pub fn semantic_wire(&self, fullscreen: bool) -> String {
        format!(
            "SHADER_INTERFACE_V1\nfingerprint {}\n{}end\n",
            self.semantic_fingerprint(fullscreen),
            self.semantic_payload(fullscreen)
        )
    }

    /// Type names are written as emitted symbols: the runtime joins a uniform group
    /// to its physical block by name. Fingerprints keep the logical key.
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
            writeln!(
                out,
                "vertex_input {} {} {}",
                input.struct_name.symbol,
                rate,
                input.attrs.len()
            )
            .unwrap();
            for (name, kind) in &input.attrs {
                writeln!(out, "attr {} {}", name, attr_kind_code(*kind)).unwrap();
            }
        }

        writeln!(out, "varyings {}", self.varyings.len()).unwrap();
        for varying in &self.varyings {
            writeln!(
                out,
                "varying {} {} {}",
                varying.name,
                varying.kind.code(),
                if varying.flat { 1 } else { 0 }
            )
            .unwrap();
        }

        writeln!(out, "groups {}", self.uniform_groups.len()).unwrap();
        for group in &self.uniform_groups {
            writeln!(
                out,
                "group {} {} {} {}",
                uniform_role_name(group.role),
                group.struct_name.symbol,
                group.semantic_fingerprint(),
                group.members.len()
            )
            .unwrap();
            for (name, kind) in &group.members {
                writeln!(out, "member {} {}", name, kind.code()).unwrap();
            }
        }

        writeln!(out, "arrays {}", self.array_groups.len()).unwrap();
        for group in &self.array_groups {
            writeln!(
                out,
                "array {} {} {}",
                group.stage.name(),
                group.symbol(),
                group.members.len()
            )
            .unwrap();
            for member in &group.members {
                writeln!(
                    out,
                    "array_member {} {} {}",
                    member.name,
                    member.elem.code(),
                    member.capacity
                )
                .unwrap();
            }
        }

        match &self.textures {
            Some(group) => {
                let struct_name = group
                    .struct_name
                    .map(|name| name.symbol.as_str())
                    .unwrap_or("none");
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
            writeln!(
                out,
                "bare_sampler {} {} {}",
                sampler.stage.name(),
                sampler.name,
                sampler_kind_code(sampler.kind)
            )
            .unwrap();
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
