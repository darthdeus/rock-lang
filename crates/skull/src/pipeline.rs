//! The compile pipeline: parsed modules to an emitted program.

use bone_core::modules::NsAliases;
use bone_semantic::inference::{ComptimeInputs, Workspace};
use bone_semantic::prelude::{FxHashMap, FxHashSet, ModuleId};
use rock_core::errors::CompilerErrorLevel;
use skull_parser::stdlib::ModuleSet;
use skull_parser::{Ast, CompilerError};
use ustr::Ustr;

use crate::mir_emit::{MirEmit, RendererTarget};

/// Parse the process stdlib on another thread, so [`with_stdlib`] finds it
/// ready.
pub fn warm_stdlib() {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(|| {
        let _ = skull_parser::stdlib::cached();
    });
}

/// The shared process stdlib with `user` modules after it.
pub fn with_stdlib(user: impl IntoIterator<Item = Ast>) -> Result<ModuleSet, CompilerError> {
    ModuleSet::with_stdlib(user)
}

/// Every recovered syntax error across `modules`, in module order.
pub fn syntax_errors<'a>(modules: impl IntoIterator<Item = &'a Ast>) -> Vec<CompilerError> {
    modules
        .into_iter()
        .flat_map(|m| m.recovery_errors.iter().cloned())
        .collect()
}

/// Lower and type-check `modules`.
pub fn semantic_pass(
    modules: &mut ModuleSet,
    aliases: &NsAliases,
) -> Result<Workspace, CompilerError> {
    let ws = bone_semantic::semantic_passes::semantic_pass_without_closure_gate(modules, aliases)?;
    apply_closure_gate(ws)
}

fn apply_closure_gate(mut ws: Workspace) -> Result<Workspace, CompilerError> {
    let timer = bone_core::prelude::Instant::now();
    bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
    ws.log_time("api closure", timer);
    Ok(ws)
}

/// Syntax and macro gates, the semantic pass, then the syntax gate again over
/// the expanded trees. `Ok` carries the body errors beside the workspace.
pub fn check_modules(
    modules: &mut ModuleSet,
    aliases: impl FnOnce(&ModuleSet) -> Result<NsAliases, CompilerError>,
) -> Result<(Workspace, Vec<CompilerError>), Vec<CompilerError>> {
    let gates_timer = bone_core::prelude::Instant::now();
    let syntax = syntax_errors(modules.iter());
    if !syntax.is_empty() {
        return Err(syntax);
    }
    let gates_time = gates_timer.elapsed();
    let aliases_timer = bone_core::prelude::Instant::now();
    let aliases = match aliases(modules) {
        Ok(aliases) => aliases,
        Err(error) => {
            let macro_errors = macro_gates(&modules.iter().collect::<Vec<_>>());
            return Err(if macro_errors.is_empty() {
                vec![error]
            } else {
                macro_errors
            });
        }
    };
    let aliases_time = aliases_timer.elapsed();
    let ws =
        bone_semantic::semantic_passes::semantic_pass_after_gate(modules, &aliases, &macro_gates);
    let ws = match ws {
        Err(bone_semantic::semantic_passes::PassFailure::Gate(errors)) => return Err(errors),
        Err(bone_semantic::semantic_passes::PassFailure::Pass(error)) => Err(error),
        Ok(ws) => apply_closure_gate(ws),
    };
    // A macro that must not expand leaves the call in place and records why; that
    // reason explains the type errors it causes, so it reports first.
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

/// Runs before expansion removes the macro definitions it checks.
fn macro_gates(modules: &[&Ast]) -> Vec<CompilerError> {
    skull_parser::check_macro_name_uniqueness(modules.iter().copied())
}

/// Modules the lint pass treats as warm (scanned, never reported): every
/// module whose file is not one of this build's own `sources`.
pub fn warm_module_ids(ws: &Workspace, sources: &FxHashMap<Ustr, &str>) -> FxHashSet<ModuleId> {
    ws.source_modules()
        .filter(|m| !sources.contains_key(&m.file))
        .map(|m| m.id)
        .collect()
}

/// Split lint findings into refusing `Error`s and the tiers that only report.
pub fn split_findings(
    found: Vec<bone_lints::Finding>,
) -> (Vec<CompilerError>, Vec<bone_lints::Finding>) {
    let (errors, soft): (Vec<_>, Vec<_>) = found
        .into_iter()
        .partition(|f| f.level() == CompilerErrorLevel::Error);
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

pub trait CompileHost {
    /// Dependency alias tables for the module set.
    fn aliases(&mut self, modules: &ModuleSet) -> Result<NsAliases, CompilerError>;
    /// A pipeline boundary was reached; hosts that measure record it.
    fn phase(&mut self, _phase: Phase<'_>) {}
    /// The build inputs (`build_var` values, `embed_string` files), resolved
    /// once the program has type-checked.
    fn comptime_inputs(&mut self) -> Result<ComptimeInputs, CompilerError>;

    fn checked_comptime_inputs(
        &mut self,
        _ws: &Workspace,
    ) -> Result<ComptimeInputs, CompilerError> {
        self.comptime_inputs()
    }

    /// The lint findings for this build's own files.
    fn lint_findings(&mut self, ws: &Workspace) -> Vec<bone_lints::Finding>;
    /// [`Self::checked_comptime_inputs`] and [`Self::lint_findings`] for one
    /// checked workspace.
    /// Neither reads the other's output, so a host may run them side by side.
    fn checked_inputs_and_lints(
        &mut self,
        ws: &Workspace,
    ) -> (
        Result<ComptimeInputs, CompilerError>,
        Vec<bone_lints::Finding>,
    ) {
        match self.checked_comptime_inputs(ws) {
            Ok(inputs) => (Ok(inputs), self.lint_findings(ws)),
            Err(error) => (Err(error), Vec::new()),
        }
    }
    /// Present the findings that do not refuse the program.
    fn report_soft_findings(&mut self, soft: Vec<bone_lints::Finding>);
    fn emit(
        &mut self,
        ws: &Workspace,
        entry: ModuleId,
        target: EmitTarget,
    ) -> Result<MirEmit, CompilerError> {
        emit_program(ws, entry, target)
    }
}

/// [`CompileHost::emit`]'s program emission, for hosts that wrap it.
pub fn emit_program(
    ws: &Workspace,
    entry: ModuleId,
    target: EmitTarget,
) -> Result<MirEmit, CompilerError> {
    let mut emit = crate::mir_emit::FullEmit::one_shot()
        .without_fingerprint()
        .project_root(target.project_root)
        .renderer(target.renderer);
    if target.global_accessors {
        emit = emit.with_global_accessors();
    }
    if target.attribution {
        emit = emit.with_attribution();
    }
    emit.run(ws, entry).map(|(emit, cache)| {
        crate::mir_emit::par::drop_in_background(cache);
        emit
    })
}

#[derive(Clone, Copy)]
pub struct EmitTarget<'a> {
    /// A project build's directory; `None` for a single source.
    pub project_root: Option<&'a str>,
    pub renderer: RendererTarget,
    /// Emit reflection accessors for unboxed mutable globals.
    pub global_accessors: bool,
    /// Collect body-phase attribution ([`crate::mir_emit::FullEmit::with_attribution`]).
    pub attribution: bool,
}

impl EmitTarget<'_> {
    pub const SINGLE_SOURCE: EmitTarget<'static> = EmitTarget {
        global_accessors: false,
        project_root: None,
        renderer: RendererTarget::Gl,
        attribution: false,
    };
}

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

/// Compile an assembled module set (stdlib included) starting at `entry`.
pub fn compile_modules(
    mut modules: ModuleSet,
    entry: Ustr,
    target: EmitTarget<'_>,
    host: &mut dyn CompileHost,
) -> Result<Compiled, CompileFailure> {
    let (mut ws, body_errors) = check_modules(&mut modules, |m| host.aliases(m))
        .map_err(|errors| CompileFailure::at(Stage::Check, errors))?;
    if !body_errors.is_empty() {
        return Err(CompileFailure::at(Stage::Check, body_errors));
    }
    host.phase(Phase::Checked(&ws));
    crate::mir_emit::par::drop_in_background(modules);
    let (inputs, findings) = host.checked_inputs_and_lints(&ws);
    ws.comptime_inputs = inputs.map_err(|e| CompileFailure::at(Stage::Comptime, vec![e]))?;
    let entry = ws.module_ids[&entry];
    let (lint_errors, soft) = split_findings(findings);
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
