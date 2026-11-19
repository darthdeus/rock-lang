use crate::instance_graph::BoundaryClass;
use bone_core::hir::FnBackend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderTargetSpec {
    Glsl(skull_mir::shader_ir::GlslProfile),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTargetSpec {
    pub backend: FnBackend,
    pub foreign_symbols: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    pub cpu: CpuTargetSpec,
    pub shader: Option<ShaderTargetSpec>,
}

const GLSL_330: Option<ShaderTargetSpec> = Some(ShaderTargetSpec::Glsl(
    skull_mir::shader_ir::GlslProfile::Core330,
));

pub const LUAJIT_DESKTOP: TargetSpec = TargetSpec {
    cpu: CpuTargetSpec {
        backend: FnBackend::Lua,
        foreign_symbols: true,
    },
    shader: GLSL_330,
};

pub const JAVASCRIPT_WEB: TargetSpec = TargetSpec {
    cpu: CpuTargetSpec {
        backend: FnBackend::Js,
        foreign_symbols: false,
    },
    shader: GLSL_330,
};

pub fn supports(target: &CpuTargetSpec, class: BoundaryClass) -> bool {
    class != BoundaryClass::Extern || target.foreign_symbols
}
