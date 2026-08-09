//! Target-neutral shader populations realized from the shared instance graph.

use std::collections::BTreeSet;

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
}
