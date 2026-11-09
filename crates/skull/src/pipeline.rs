//! The compile pipeline: parsed modules to an emitted program, through every
//! gate a build applies, in order. A host (the command line, a browser)
//! supplies what needs a filesystem or a process -- alias tables, build
//! inputs, the lint report set, where diagnostics go -- through
//! [`CompileHost`]; it never decides whether a program compiles.

use bone_core::modules::NsAliases;
use bone_semantic::inference::{ComptimeInputs, Workspace};
use bone_semantic::prelude::{FxHashMap, FxHashSet, ModuleId};
use rock_core::errors::CompilerErrorLevel;
use skull_parser::stdlib::ModuleSet;
use skull_parser::{Ast, CompilerError};
use ustr::Ustr;

use crate::mir_emit::{MirEmit, RendererTarget};

/// The shared process stdlib with `user` modules after it: the module set
/// every non-incremental compile checks. A stdlib that does not parse is the
/// error ([`skull_parser::stdlib::cached`], the loader the editor shares).
pub fn with_stdlib(user: impl IntoIterator<Item = Ast>) -> Result<ModuleSet, CompilerError> {
    ModuleSet::with_stdlib(user)
}

/// Every recovered syntax error across `modules`, in module order. Parsing is
/// total, so a compile refuses a program with any syntax error here; the
/// editor keeps the partial tree and shows the same errors as diagnostics.
pub fn syntax_errors<'a>(modules: impl IntoIterator<Item = &'a Ast>) -> Vec<CompilerError> {
    modules
        .into_iter()
        .flat_map(|m| m.recovery_errors.iter().cloned())
        .collect()
}

/// Whether the semantic pass enforces the API closure itself or leaves the
/// requirements to a caller that reads them (the module migration tools).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClosureGate {
    Enforce,
    LeaveToCaller,
}

/// Lower and type-check `modules`.
pub fn semantic_pass(
    modules: &mut ModuleSet,
    aliases: &NsAliases,
    gate: ClosureGate,
) -> Result<Workspace, CompilerError> {
    let mut ws =
        bone_semantic::semantic_passes::semantic_pass_without_closure_gate(modules, aliases)?;
    if gate == ClosureGate::Enforce {
        let timer = bone_core::prelude::Instant::now();
        bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
        ws.log_time("api closure", timer);
    }
    Ok(ws)
}

/// The front half over an assembled module set: syntax and macro gates, the
/// semantic pass, then the syntax gate again over the expanded trees. `Err`
/// carries every diagnostic of the first failing gate; `Ok` carries the body
/// errors beside the workspace, since `semantic_pass` succeeding means
/// "lowered and checked", not "no type errors".
pub fn check_modules(
    modules: &mut ModuleSet,
    gate: ClosureGate,
    aliases: impl FnOnce(&ModuleSet) -> Result<NsAliases, CompilerError>,
) -> Result<(Workspace, Vec<CompilerError>), Vec<CompilerError>> {
    let gates_timer = bone_core::prelude::Instant::now();
    let syntax = syntax_errors(modules.iter());
    if !syntax.is_empty() {
        return Err(syntax);
    }
    // Macro names must be unique against function names (call-position
    // ambiguity), and a macro argument label must name a parameter: both are
    // checked before expansion drops the macro defs and the labels.
    let macro_dupes = skull_parser::check_macro_name_uniqueness(modules.iter());
    if !macro_dupes.is_empty() {
        return Err(macro_dupes);
    }
    let macro_labels = skull_parser::check_macro_call_labels(modules.iter());
    if !macro_labels.is_empty() {
        return Err(macro_labels);
    }
    let gates_time = gates_timer.elapsed();
    let aliases_timer = bone_core::prelude::Instant::now();
    let aliases = aliases(modules).map_err(|error| vec![error])?;
    let aliases_time = aliases_timer.elapsed();
    let ws = semantic_pass(modules, &aliases, gate);
    // Expansion recovers: a macro that must not expand records the reason on
    // `recovery_errors` and leaves the call in place. That reason explains
    // any type error the unexpanded call caused, so it reports first.
    let expanded_syntax = syntax_errors(modules.iter());
    if !expanded_syntax.is_empty() {
        return Err(expanded_syntax);
    }
    let mut ws = ws.map_err(|e| vec![e])?;
    ws.phase_timings.push(("gates", gates_time));
    ws.phase_timings.push(("aliases", aliases_time));
    let timer = bone_core::prelude::Instant::now();
    let errs = skull_mir::verdict::body_errors(&ws);
    ws.log_time("body verdict", timer);
    Ok((ws, errs))
}

/// Modules the lint pass treats as warm (scanned, never reported): every
/// module whose file is not one of this build's own `sources`.
pub fn warm_module_ids(ws: &Workspace, sources: &FxHashMap<Ustr, &str>) -> FxHashSet<ModuleId> {
    ws.source_modules()
        .filter(|m| !sources.contains_key(&m.file))
        .map(|m| m.id)
        .collect()
}

/// Split lint findings into the `Error`s that refuse the program and the
/// softer tiers that only report.
pub fn split_findings(
    found: Vec<bone_lints::Finding>,
) -> (Vec<CompilerError>, Vec<bone_lints::Finding>) {
    let (errors, soft): (Vec<_>, Vec<_>) = found
        .into_iter()
        .partition(|f| f.level() == CompilerErrorLevel::Error);
    (errors.into_iter().map(|f| f.error).collect(), soft)
}

/// What a compile needs from its host.
/// A boundary the host may measure.
pub enum Phase<'a> {
    /// The entry source and the stdlib are parsed ([`compile_source`] only).
    Parsed,
    /// The program type-checked.
    Checked(&'a Workspace),
    /// Build inputs are stamped and lints passed; emission starts.
    Prepared,
}

pub trait CompileHost {
    /// Dependency alias tables for the module set.
    fn aliases(&mut self, modules: &ModuleSet) -> Result<NsAliases, CompilerError>;
    /// A pipeline boundary was reached; hosts that measure record it.
    fn phase(&mut self, _phase: Phase<'_>) {}
    /// The build inputs (`build_var` values, `embed_string` files), resolved
    /// once the program has type-checked.
    fn comptime_inputs(&mut self) -> Result<ComptimeInputs, CompilerError>;

    /// Resolve compiler-derived inputs against the checked workspace. Hosts
    /// without deferred declarations retain the ordinary input contract.
    fn checked_comptime_inputs(
        &mut self,
        _ws: &Workspace,
    ) -> Result<ComptimeInputs, CompilerError> {
        self.comptime_inputs()
    }

    /// The lint findings for this build's own files. The host chooses the
    /// report set and the closed-world flag; [`warm_module_ids`] and
    /// `bone_lints::lint_workspace` compute them.
    fn lint_findings(&mut self, ws: &Workspace) -> Vec<bone_lints::Finding>;
    /// Present the findings that do not refuse the program.
    fn report_soft_findings(&mut self, soft: Vec<bone_lints::Finding>);
    /// Emit the checked program. The host selects the roots: a test build
    /// roots at its tests, every other build at the program entry. The
    /// pipeline keeps one check-lint-emit order and never branches on kind.
    fn emit(
        &mut self,
        ws: &Workspace,
        entry: ModuleId,
        target: EmitTarget,
    ) -> Result<MirEmit, CompilerError> {
        crate::mir_emit::FullEmit::one_shot()
            .project_root(target.project_root)
            .renderer(target.renderer)
            .run(ws, entry)
            .map(|(emit, _cache)| emit)
    }
}

/// Where emission prunes and which renderer it targets.
#[derive(Clone, Copy)]
pub struct EmitTarget<'a> {
    /// A project build's directory: modules outside it are libraries and
    /// prune to what the project reaches. `None` for a single source.
    pub project_root: Option<&'a str>,
    pub renderer: RendererTarget,
}

impl EmitTarget<'_> {
    pub const SINGLE_SOURCE: EmitTarget<'static> = EmitTarget {
        project_root: None,
        renderer: RendererTarget::Gl,
    };
}

/// The stage a compile stopped at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Parse,
    Check,
    Comptime,
    Lint,
    Emit,
}

#[derive(Debug)]
pub struct CompileFailure {
    pub stage: Stage,
    pub errors: Vec<CompilerError>,
}

impl CompileFailure {
    fn at(stage: Stage, errors: Vec<CompilerError>) -> Self {
        Self { stage, errors }
    }
}

pub struct Compiled {
    pub ws: Workspace,
    pub entry: ModuleId,
    pub emission: MirEmit,
}

/// Compile one source file against the stdlib.
pub fn compile_source(
    file: &str,
    src: &str,
    target: EmitTarget<'_>,
    host: &mut dyn CompileHost,
) -> Result<Compiled, CompileFailure> {
    let user = skull_parser::parse_module(src, Ustr::from(file))
        .map_err(|e| CompileFailure::at(Stage::Parse, vec![e]))?;
    let entry = user.file;
    let modules = with_stdlib([user]).map_err(|e| CompileFailure::at(Stage::Parse, vec![e]))?;
    host.phase(Phase::Parsed);
    compile_modules(modules, entry, target, host)
}

/// Compile an assembled module set (stdlib included) whose program starts in
/// the module parsed from `entry`.
pub fn compile_modules(
    mut modules: ModuleSet,
    entry: Ustr,
    target: EmitTarget<'_>,
    host: &mut dyn CompileHost,
) -> Result<Compiled, CompileFailure> {
    let (mut ws, body_errors) =
        check_modules(&mut modules, ClosureGate::Enforce, |m| host.aliases(m))
            .map_err(|errors| CompileFailure::at(Stage::Check, errors))?;
    if !body_errors.is_empty() {
        return Err(CompileFailure::at(Stage::Check, body_errors));
    }
    host.phase(Phase::Checked(&ws));
    // The workspace owns everything lints and emission read; the parse trees
    // would otherwise stay live through the emission peak.
    drop(modules);
    ws.comptime_inputs = host
        .checked_comptime_inputs(&ws)
        .map_err(|e| CompileFailure::at(Stage::Comptime, vec![e]))?;
    let entry = ws.module_ids[&entry];
    // Lints are a compiler pass: an `Error` finding refuses the program
    // through the same path a type error takes.
    let (lint_errors, soft) = split_findings(host.lint_findings(&ws));
    host.report_soft_findings(soft);
    if !lint_errors.is_empty() {
        return Err(CompileFailure::at(Stage::Lint, lint_errors));
    }
    host.phase(Phase::Prepared);
    let emission = host
        .emit(&ws, entry, target)
        .map_err(|e| CompileFailure::at(Stage::Emit, vec![e]))?;
    Ok(Compiled {
        ws,
        entry,
        emission,
    })
}
