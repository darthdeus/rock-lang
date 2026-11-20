use bone_core::prelude::Instant;
use bone_semantic::hir::{BodyOwner, Expr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, ModuleId, is_stdlib_module_file};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_codegen_lua::{CodegenCtx, mangle_fn};
use skull_mir::decorator::{DecoratorTable, SimValues, decorator_symbol};
use skull_mir::global_storage::{StaticBaseline, static_cell_key};
use skull_mir::instance::Instance;
use skull_mir::mir::{FuncRef, Inst, MirFunc, Term};
use skull_mir::reach::PendingInit;
use skull_parser::CompilerError;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::Duration;
use ustr::Ustr;

mod assemble;
mod attribution;
mod body;
mod cache;
mod chunk;
mod decorators;
mod full;
mod global_init;
mod instances;
mod overlay;
pub(crate) mod par;
mod roots;
mod shaders;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;
mod unit;
mod validate;

pub(crate) use assemble::*;
pub use attribution::*;
pub use body::*;
pub use cache::*;
pub use global_init::*;
pub use overlay::*;
pub use roots::*;

use chunk::*;
use decorators::*;
use full::*;
use instances::*;
use par::*;
use shaders::*;
use unit::*;
use validate::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct CompileCounts {
    pub concrete_plans: usize,
    pub chunks: usize,
    pub dependency_edges: Option<usize>,
}

/// The line after which a host attaches its own global directory fields.
pub const GLOBALS_BRIDGE_LINE: &str = "__rock_globals_install_bridge()\n";

pub struct MirEmit {
    pub compile_counts: CompileCounts,
    pub lua: String,
    /// The byte offset in `lua` just past [`GLOBALS_BRIDGE_LINE`]. `None` for
    /// an overlay, which has no bridge, and for text rewritten after assembly.
    pub globals_bridge_end: Option<usize>,
    pub allocation_report: crate::allocation_report::AllocationReport,
    pub code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    pub lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    /// The mutable-global layout fingerprint the program opens its storage with.
    pub global_layout: String,
    pub definition_plan: skull_mir::definition::DefinitionPlan,
    /// `None` when built with [`FullEmit::without_fingerprint`].
    pub artifact_fingerprint: Option<String>,
    pub phases: Vec<(&'static str, std::time::Duration)>,
    pub attribution: EmitAttribution,
    /// Whether the host should append the generated main call.
    pub emits_main: bool,
    /// Every registered shader, in registration order.
    pub shaders: std::sync::Arc<[ShaderSource]>,
}

#[derive(Clone, Debug)]
pub struct ShaderSource {
    pub key: String,
    pub language: ShaderLanguage,
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderLanguage {
    Glsl,
}

pub fn emit_workspace_mir(ws: &Workspace, user_mid: ModuleId) -> Result<MirEmit, CompilerError> {
    FullEmit::one_shot()
        .with_attribution()
        .run(ws, user_mid)
        .map(|(emit, _cache)| emit)
}

/// Emit all required definitions and retain chunks for incremental edits.
pub fn emit_full(
    ws: &Workspace,
    user_mid: ModuleId,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    FullEmit::program().run(ws, user_mid)
}

#[derive(Clone, Copy)]
pub struct FullEmit<'a> {
    build: FullBuild<'a>,
    project_root: Option<&'a str>,
    renderer_target: RendererTarget,
    shapes: ShapeOverrides,
    fingerprint: bool,
    global_accessors: bool,
    attribution: bool,
}

impl<'a> FullEmit<'a> {
    fn new(build: FullBuild<'a>) -> Self {
        Self {
            build,
            project_root: None,
            renderer_target: RendererTarget::Gl,
            shapes: ShapeOverrides::default(),
            fingerprint: true,
            global_accessors: false,
            attribution: false,
        }
    }

    /// A program that may be reloaded in place. The returned cache serves
    /// later [`emit_overlay`] builds.
    pub fn program() -> Self {
        Self::new(FullBuild::Program)
    }

    /// A program emitted once. Its graph, plans and chunks are released during
    /// assembly, so the returned cache cannot serve a later build.
    pub fn one_shot() -> Self {
        Self::new(FullBuild::OneShotProgram)
    }

    /// A package's `rock test` program, rooted at its `tests`.
    pub fn package_test(tests: &'a [FuncRef]) -> Self {
        Self::new(FullBuild::PackageTest(tests))
    }

    /// Sim receipt emission keeps the shared workspace but roots only selected
    /// sims, their registry dependencies, main and the global roots.
    pub fn selected_sims(sims: &'a [String]) -> Self {
        Self::new(FullBuild::SelectedSims(sims))
    }

    /// Scope the build to a project directory: modules outside it are
    /// libraries and prune down to what the project reaches.
    pub fn project_root(mut self, project_root: Option<&'a str>) -> Self {
        self.project_root = project_root;
        self
    }

    /// Publish getters and setters for unboxed mutable globals in the host directory.
    pub fn with_global_accessors(mut self) -> Self {
        self.global_accessors = true;
        self
    }

    /// Skip the artifact fingerprint. Only a host that writes the program once
    /// and never reloads or replays it can do without.
    pub fn without_fingerprint(mut self) -> Self {
        self.fingerprint = false;
        self
    }

    /// Fold every emitted body's attribution record into [`MirEmit::attribution`].
    pub fn with_attribution(mut self) -> Self {
        self.attribution = true;
        self
    }

    pub fn renderer(mut self, renderer_target: RendererTarget) -> Self {
        self.renderer_target = renderer_target;
        self
    }

    /// Explicit emitted shapes, for tests that must emit several shapes in
    /// one process.
    pub fn shapes(mut self, shapes: ShapeOverrides) -> Self {
        self.shapes = shapes;
        self
    }

    pub fn run(
        self,
        ws: &Workspace,
        user_mid: ModuleId,
    ) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
        let _allocation_environment = crate::allocation_report::environment_scope();
        emit_full_build(ws, user_mid, self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererTarget {
    Gl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildKind {
    /// A game, tool or single-file program; it may be reloaded in place.
    Program,
    /// A package's `rock test` program. Each test child loads it once and never
    /// reloads it. It runs no sims, so it registers none.
    PackageTest,
}

/// Emitted-shape overrides for a test that drives two shapes in one binary.
#[derive(Clone, Copy, Default)]
pub struct ShapeOverrides {
    /// `Some(false)` emits a body that breaks a LuaJIT parse-time frame cap
    /// instead of rejecting it, so a test can ask the bundled LuaJIT whether
    /// that Lua really fails to load.
    pub frame_limits: Option<bool>,
    /// Overrides `ROCK_SKULL_LINE_DIRECTIVES`
    /// (`skull_codegen_lua::CodegenCtx::set_line_directives`).
    pub line_directives: Option<bool>,
}

/// Rejects a root that matches no module: a relative or stale path would
/// otherwise silently treat the project's own sources as prunable libraries.
fn canonical_project_root(
    ws: &Workspace,
    project_root: Option<&str>,
) -> Result<Option<String>, CompilerError> {
    let Some(root) = project_root else {
        return Ok(None);
    };
    let abs = std::fs::canonicalize(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root.to_string());
    let matches = ws
        .modules
        .iter()
        .any(|m| !is_library_module(m.file.as_str(), Some(&abs)));
    if !matches {
        return Err(CompilerError::new_no_location(format!(
            "skull: project root `{abs}` matches no module file -- every project source would \
             be classified as a library and pruned. This is a caller bug, not a user error."
        )));
    }
    Ok(Some(abs))
}
