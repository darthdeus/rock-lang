//! Target-neutral shader populations realized from the shared instance graph.

use std::collections::BTreeSet;
use std::sync::Arc;

use skull_mir::concrete::ConcretePlans;
use skull_mir::mir::FuncRef;
use skull_mir::shader_ir::ShaderStage;

use crate::instance_graph::Instance;

/// One registered shader program: a `(vertex, fragment)` pair, or the
/// single-argument fullscreen form whose vertex stage the compiler supplies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShaderProgram {
    pub vert: Option<FuncRef>,
    pub frag: FuncRef,
}

/// The graph-owned population for one shader stage of one registered program.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShaderStageView {
    pub stage: ShaderStage,
    pub entry: Instance,
    pub retained: BTreeSet<Instance>,
    pub helpers: BTreeSet<Instance>,
    pub builtins: BTreeSet<Instance>,
    pub order: Vec<Instance>,
    pub dependencies: Vec<FuncRef>,
}

/// The graph-owned populations for one registered shader program. `stages` is
/// in registration order: vertex first when present, fragment second.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShaderProgramView {
    pub program: ShaderProgram,
    pub stages: Vec<ShaderStageView>,
    pub concrete_plans: Arc<ConcretePlans>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ComputeProgram {
    pub entry: FuncRef,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ComputeProgramView {
    pub program: ComputeProgram,
    pub stage: ShaderStageView,
    pub concrete_plans: Arc<ConcretePlans>,
}

/// The graph-owned population for an explicit-root GLSL function library.
/// Unlike [`ShaderProgramView`], this has no registered shader stages or
/// interface contract: the caller supplies ordinary function roots and the
/// backend only packages their retained definitions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShaderLibraryView {
    pub roots: Vec<Instance>,
    pub retained: BTreeSet<Instance>,
    pub builtins: BTreeSet<Instance>,
    pub order: Vec<Instance>,
    pub dependencies: Vec<FuncRef>,
    pub concrete_plans: Arc<ConcretePlans>,
}
