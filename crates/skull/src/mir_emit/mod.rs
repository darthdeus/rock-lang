//! Whole-workspace Lua emit: the cell-model IR (`skull-mir`) lowered and
//! printed by `skull-codegen-lua`, assembled into a complete runnable Lua chunk
//! together with the IR-agnostic load-time helpers (runtime prelude,
//! value-struct cdefs, enum singletons, type tables).
//!
//! A definition that fails to lower rejects the compile, stdlib or not.
//! inline(lua) bodies emit verbatim,
//! externs are pre-existing Lua globals (no def), coroutines lower via the
//! `skull-transform` stackless pass, decorators via `lower_decorator`.
//!
//! Global inits are emitted in transitive-dependency order (`direct_refs` ->
//! `transitive_global_reads` -> `sort_global_init_lines`): a global read through
//! a call chain still inits first, mirroring rock's `global_dependencies`.

use bone_core::prelude::Instant;
use bone_semantic::hir::{BodyOwner, Expr};
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, ModuleId, is_stdlib_module_file};
use rustc_hash::{FxHashMap, FxHashSet};
use skull_codegen_lua::{CodegenCtx, emit_function, mangle_fn};
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
#[cfg(test)]
mod tests;
mod unit;
mod validate;

pub use assemble::*;
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

/// O(1) container lengths and a dependency total folded by the existing
/// production dependency-index loop. No diagnostic graph walk is needed.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompileCounts {
    pub concrete_plans: usize,
    pub chunks: usize,
    pub dependency_edges: Option<usize>,
}

/// Containers released before one-shot assembly finishes. Live builds retain them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompileReleaseStats {
    pub graph_instances: usize,
    pub chunks: usize,
    pub concrete_plans: usize,
    pub retained_dependency_edges: usize,
    pub retained_static_keys: usize,
}

/// The assembled Lua program and what was emitted into it.
pub struct MirEmit {
    pub release_stats: CompileReleaseStats,
    pub compile_counts: CompileCounts,
    pub lua: String,
    /// Allocation-capable operations registered by final Lua emission, with
    /// residual call edges and compiler-owned per-frame lifecycle routes.
    pub allocation_report: crate::allocation_report::AllocationReport,
    /// Artifact-owned callable identities and their self-validating census.
    pub code_artifact: std::sync::Arc<skull_mir::code_identity::CodeArtifact>,
    /// The compiler-owned lifecycle inventory staged by this artifact.
    pub lifecycle_plan: skull_mir::lifecycle::LifecyclePlan,
    /// The mutable-global layout fingerprint the program opens its storage with.
    pub global_layout: String,
    /// The compiler-owned definition inventory included in the exact Artifact
    /// identity and published to the runtime.
    pub definition_plan: skull_mir::definition::DefinitionPlan,
    /// The semantic identity of this exact emitted Artifact. It is computed
    /// before the backend payload leaves the compiler.
    pub artifact_fingerprint: String,
    /// Coarse per-stage time for the emit, in run order. `setup:` entries are
    /// direct wall-clock measurements; the remaining entries cover HIR->MIR
    /// lowering, MIR cleanup, Lua emission, and sequential assembly. The latter
    /// are per-unit elapsed timers that overlap under parallel emit and become
    /// wall-clock measurements under `sequential`.
    pub phases: Vec<(&'static str, std::time::Duration)>,
    /// ABI-plan memo counters over the emitting context's lifetime (== this
    /// emission for a full build, cumulative across edits on the incremental
    /// paths). Counts, so load-immune: `hits() > 0` on any real corpus is the
    /// memo's non-vacuity witness. Surfaced by `backend-stats`.
    pub abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    /// Body-oriented attribution for the current emission. Counters are reduced
    /// from per-body results after the parallel fanout, so the hot passes do not
    /// use shared atomics.
    pub attribution: EmitAttribution,
    /// Shared-arena traffic split at the setup/fanout boundary (full builds;
    /// zeroed on paths without a fresh setup phase). The `*_final - *_setup`
    /// deltas are the post-setup traffic a frozen read view would serve.
    pub arena_traffic: ArenaTraffic,
    /// The three independent setup lanes and their production populations.
    /// The overlap bits come from the lanes' actual execution intervals, not
    /// from a wall-clock estimate.
    pub setup_lanes: SetupLaneAttribution,
    /// Operational graph work and repeated source-fact populations from the
    /// graph used by this emission.
    pub graph_census: crate::instance_graph::RunStats,
    /// Concrete-plan ownership and assembly populations from this emission.
    pub concrete_stats: crate::concrete_body::ConcreteBuildStats,
    /// Whether this emission defines the game entry point `main`, so the caller
    /// can append `__rock_main_wrapper(main)`.
    ///
    /// Reported by the emitter rather than recovered from the text: a caller
    /// that searched `lua` for `function main(` silently stopped finding it the
    /// moment callee binding moved the body inside a factory, and the game booted to a chunk that defined everything and ran
    /// nothing.
    pub emits_main: bool,
    /// Every shader this artifact registers, as the source the registration
    /// carries, in registration order. Shared with the emit cache, which
    /// keeps them across incremental builds.
    pub shaders: std::sync::Arc<[ShaderSource]>,
}

/// One registered shader's emitted source.
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

/// Emit the whole workspace once. `user_mid` is the entry module. Registered
/// shaders emit Core330 for desktop OpenGL. See [`FullEmit`] for the other
/// build kinds and options.
pub fn emit_workspace_mir(ws: &Workspace, user_mid: ModuleId) -> Result<MirEmit, CompilerError> {
    FullEmit::one_shot()
        .run(ws, user_mid)
        .map(|(emit, _cache)| emit)
}

/// Full emit: populate the chunk cache for every retained ordinary/coroutine
/// body, then assemble. Byte-identical to the pre-cache emit. Returns the cache
/// so a later [`emit_incremental`] can reuse the unchanged chunks.
///
/// The per-body chunk emit runs in PARALLEL ([`par_map_with`], one
/// [`InlineCache`](skull_transform::InlineCache) per rayon worker) by default;
/// `--features sequential` forces single-threaded iteration. The cache is keyed by
/// [`FuncRef`], so the (nondeterministic) insertion order never reaches the output
/// — `assemble` walks modules in source order and pulls by key.
pub fn emit_full(
    ws: &Workspace,
    user_mid: ModuleId,
) -> Result<(MirEmit, SkullEmitCache), CompilerError> {
    FullEmit::program().run(ws, user_mid)
}

/// One full emit: the build kind it starts from, refined by the builder methods.
#[derive(Clone, Copy)]
pub struct FullEmit<'a> {
    build: FullBuild<'a>,
    project_root: Option<&'a str>,
    renderer_target: RendererTarget,
    shapes: ShapeOverrides,
    mutation: BodyMutation,
}

impl<'a> FullEmit<'a> {
    fn new(build: FullBuild<'a>) -> Self {
        Self {
            build,
            project_root: None,
            renderer_target: RendererTarget::Gl,
            shapes: ShapeOverrides::default(),
            mutation: BodyMutation::None,
        }
    }

    /// A program that may be reloaded in place. The returned cache serves
    /// later [`emit_incremental`] and [`emit_overlay`] builds.
    pub fn program() -> Self {
        Self::new(FullBuild::Program)
    }

    /// A program emitted once. Its graph, plans and chunks are released during
    /// assembly, so the returned cache cannot serve a later build.
    pub fn one_shot() -> Self {
        Self::new(FullBuild::OneShotProgram)
    }

    /// A package's `rock test` program, rooted at its `tests`. See
    /// [`BuildKind::PackageTest`].
    pub fn package_test(tests: &'a [FuncRef]) -> Self {
        Self::new(FullBuild::PackageTest(tests))
    }

    /// Sim receipt emission keeps the shared workspace but roots only selected
    /// sims, their registry dependencies, main and the global roots.
    pub fn selected_sims(sims: &'a [String]) -> Self {
        Self::new(FullBuild::SelectedSims(sims))
    }

    /// Scope the build to a PROJECT directory: modules outside it (a
    /// stick.toml dependency such as a host library) are libraries and prune
    /// down to what the project reaches. See [`is_library_module`].
    pub fn project_root(mut self, project_root: Option<&'a str>) -> Self {
        self.project_root = project_root;
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

    #[cfg(test)]
    fn mutation(mut self, mutation: BodyMutation) -> Self {
        self.mutation = mutation;
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

/// What an emitted program is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildKind {
    /// A game, tool or single-file program; it may be reloaded in place.
    Program,
    /// A package's `rock test` program. Each test child loads it once and never
    /// reloads it. It runs no sims, so it registers none.
    PackageTest,
}

/// Emitted-shape overrides for one emit, for a test that must drive two
/// shapes in one test binary. `Default` -- `None` everywhere -- is the
/// production path.
#[derive(Clone, Copy, Default)]
pub struct ShapeOverrides {
    /// `Some(false)` emits a body that breaks a LuaJIT parse-time frame cap
    /// instead of rejecting it. Not a production switch and not readable from
    /// the environment: the boundary-agreement test needs the Lua of a rejected
    /// body so it can ask the bundled LuaJIT whether that Lua really fails to
    /// load (`skull_codegen_lua::CodegenCtx::set_frame_limits`).
    pub frame_limits: Option<bool>,
    /// Overrides `ROCK_SKULL_LINE_DIRECTIVES`
    /// (`skull_codegen_lua::CodegenCtx::set_line_directives`).
    pub line_directives: Option<bool>,
}

/// Absolutize `project_root` so it can be path-component-matched against module file
/// paths, which are absolute for a project build.
///
/// Rejects a root that matches NO module. The caller passing a relative or stale
/// directory would otherwise silently classify the project's own sources as
/// library modules -- pruning MORE than intended, with no signal. That is the
/// failure this whole guard exists for: a path predicate that quietly matches
/// nothing looks exactly like one that works.
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
