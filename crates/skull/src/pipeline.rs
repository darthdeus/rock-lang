//! The compile pipeline: parsed modules to an emitted program, through every
//! gate a build applies, in order. A host (the command line, a browser)
//! supplies what needs a filesystem or a process -- alias tables, build
//! inputs, the lint report set, where diagnostics go -- through
//! [`CompileHost`]; it never decides whether a program compiles.

use std::sync::LazyLock;

use bone_core::identity_storage::IdentityStorageExemptions;
use bone_core::modules::NsAliases;
use bone_semantic::inference::{ComptimeInputs, Workspace};
use bone_semantic::prelude::{FxHashMap, FxHashSet, ModuleId};
use rock_core::errors::CompilerErrorLevel;
use skull_parser::{Ast, CompilerError};
use ustr::Ustr;

use crate::mir_emit::{MirEmit, RendererTarget};

/// The parsed stdlib, in load order under the loader's stdlib ids. Parsed
/// once per process and cloned per compile.
pub fn stdlib_modules() -> Vec<Ast> {
    static STDLIB: LazyLock<Vec<Ast>> = LazyLock::new(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|f| {
                let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
                skull_parser::parse_module(f.contents, id)
                    .unwrap_or_else(|e| panic!("stdlib {} must parse: {e:?}", f.name))
            })
            .collect()
    });
    STDLIB.clone()
}

/// The stdlib with `user` modules appended: the module set every
/// non-incremental compile checks.
pub fn with_stdlib(user: impl IntoIterator<Item = Ast>) -> Vec<Ast> {
    let mut modules = stdlib_modules();
    modules.extend(user);
    modules
}

/// Every recovered syntax error across `modules`, in module order. Parsing is
/// total, so a compile refuses a program with any syntax error here; the
/// editor keeps the partial tree and shows the same errors as diagnostics.
pub fn syntax_errors(modules: &[Ast]) -> Vec<CompilerError> {
    modules.iter().flat_map(|m| m.recovery_errors.iter().cloned()).collect()
}

/// Whether the semantic pass enforces the API closure itself or leaves the
/// requirements to a caller that reads them (the module migration tools).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClosureGate {
    Enforce,
    LeaveToCaller,
}

/// Lower and type-check `modules` under the host's migration exemptions.
pub fn semantic_pass(
    modules: &mut [Ast],
    aliases: &NsAliases,
    exemptions: IdentityStorageExemptions,
    gate: ClosureGate,
) -> Result<Workspace, CompilerError> {
    let ws = Workspace { identity_storage_exemptions: exemptions, ..Default::default() };
    let ws = bone_semantic::semantic_passes::semantic_pass_into(ws, modules, aliases)?;
    if gate == ClosureGate::Enforce {
        bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
    }
    Ok(ws)
}

/// The front half over an assembled module set: syntax and macro gates, the
/// semantic pass, then the syntax gate again over the expanded trees. `Err`
/// carries every diagnostic of the first failing gate; `Ok` carries the body
/// errors beside the workspace, since `semantic_pass` succeeding means
/// "lowered and checked", not "no type errors".
pub fn check_modules(
    modules: &mut [Ast],
    exemptions: IdentityStorageExemptions,
    gate: ClosureGate,
    aliases: impl FnOnce(&[Ast]) -> Result<NsAliases, CompilerError>,
) -> Result<(Workspace, Vec<CompilerError>), Vec<CompilerError>> {
    let syntax = syntax_errors(modules);
    if !syntax.is_empty() {
        return Err(syntax);
    }
    // Macro names must be unique against function names (call-position
    // ambiguity), and a macro argument label must name a parameter: both are
    // checked before expansion drops the macro defs and the labels.
    let macro_dupes = skull_parser::check_macro_name_uniqueness(modules);
    if !macro_dupes.is_empty() {
        return Err(macro_dupes);
    }
    let macro_labels = skull_parser::check_macro_call_labels(modules);
    if !macro_labels.is_empty() {
        return Err(macro_labels);
    }
    let aliases = aliases(modules).map_err(|error| vec![error])?;
    let ws = semantic_pass(modules, &aliases, exemptions, gate);
    // Expansion recovers: a macro that must not expand records the reason on
    // `recovery_errors` and leaves the call in place. That reason explains
    // any type error the unexpanded call caused, so it reports first.
    let expanded_syntax = syntax_errors(modules);
    if !expanded_syntax.is_empty() {
        return Err(expanded_syntax);
    }
    let ws = ws.map_err(|e| vec![e])?;
    let errs = skull_mir::verdict::body_errors(&ws);
    Ok((ws, errs))
}

/// Modules the lint pass treats as warm (scanned, never reported): every
/// module whose file is not one of this build's own `sources`.
pub fn warm_module_ids(ws: &Workspace, sources: &FxHashMap<Ustr, &str>) -> FxHashSet<ModuleId> {
    ws.modules
        .iter()
        .filter(|m| m.id != ModuleId::builtin() && !sources.contains_key(&m.file))
        .map(|m| m.id)
        .collect()
}

/// Split lint findings into the `Error`s that refuse the program and the
/// softer tiers that only report.
pub fn split_findings(
    found: Vec<bone_lints::Finding>,
) -> (Vec<CompilerError>, Vec<bone_lints::Finding>) {
    let (errors, soft): (Vec<_>, Vec<_>) =
        found.into_iter().partition(|f| f.level() == CompilerErrorLevel::Error);
    (errors.into_iter().map(|f| f.error).collect(), soft)
}

/// A boundary the host may measure.
pub enum Phase<'a> {
    /// The entry source and the stdlib are parsed ([`compile_source`] only).
    Parsed,
    /// The program type-checked.
    Checked(&'a Workspace),
    /// Build inputs are stamped and lints passed; emission starts.
    Prepared,
}

/// What a compile needs from its host.
pub trait CompileHost {
    /// Migration exemptions the semantic pass applies.
    fn identity_storage_exemptions(&self) -> IdentityStorageExemptions {
        IdentityStorageExemptions::default()
    }
    /// Dependency alias tables for the module set.
    fn aliases(&mut self, modules: &[Ast]) -> Result<NsAliases, CompilerError>;
    /// The build inputs (`build_var` values, `embed_string` files), resolved
    /// once the program has type-checked.
    fn comptime_inputs(&mut self) -> Result<ComptimeInputs, CompilerError>;
    /// The lint findings for this build's own files. The host chooses the
    /// report set and the closed-world flag; [`warm_module_ids`] and
    /// `bone_lints::lint_workspace` compute them.
    fn lint_findings(&mut self, ws: &Workspace) -> Vec<bone_lints::Finding>;
    /// Present the findings that do not refuse the program.
    fn report_soft_findings(&mut self, soft: Vec<bone_lints::Finding>);
    fn phase(&mut self, _phase: Phase<'_>) {}
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
    pub const SINGLE_SOURCE: EmitTarget<'static> =
        EmitTarget { project_root: None, renderer: RendererTarget::Gl };
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
    let modules = with_stdlib([user]);
    host.phase(Phase::Parsed);
    compile_modules(modules, entry, target, host)
}

/// Compile an assembled module set (stdlib included) whose program starts in
/// the module parsed from `entry`.
pub fn compile_modules(
    mut modules: Vec<Ast>,
    entry: Ustr,
    target: EmitTarget<'_>,
    host: &mut dyn CompileHost,
) -> Result<Compiled, CompileFailure> {
    let exemptions = host.identity_storage_exemptions();
    let (mut ws, body_errors) =
        check_modules(&mut modules, exemptions, ClosureGate::Enforce, |m| host.aliases(m))
            .map_err(|errors| CompileFailure::at(Stage::Check, errors))?;
    if !body_errors.is_empty() {
        return Err(CompileFailure::at(Stage::Check, body_errors));
    }
    host.phase(Phase::Checked(&ws));
    // The workspace owns everything lints and emission read; the parse trees
    // would otherwise stay live through the emission peak.
    drop(modules);
    ws.comptime_inputs =
        host.comptime_inputs().map_err(|e| CompileFailure::at(Stage::Comptime, vec![e]))?;
    let entry = ws.module_ids[&entry];
    // Lints are a compiler pass: an `Error` finding refuses the program
    // through the same path a type error takes.
    let (lint_errors, soft) = split_findings(host.lint_findings(&ws));
    host.report_soft_findings(soft);
    if !lint_errors.is_empty() {
        return Err(CompileFailure::at(Stage::Lint, lint_errors));
    }
    host.phase(Phase::Prepared);
    let emission = crate::mir_emit::emit_workspace_mir_in_for_renderer(
        &ws,
        entry,
        target.project_root,
        target.renderer,
    )
    .map_err(|e| CompileFailure::at(Stage::Emit, vec![e]))?;
    Ok(Compiled { ws, entry, emission })
}
