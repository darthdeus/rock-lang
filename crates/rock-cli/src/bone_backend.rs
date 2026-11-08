//! Bone backend integration for `rock-cli` (selected via `--backend bone`).
//!
//! `bone/` is a from-scratch bidirectional typechecker (parse -> lower to HIR ->
//! type-check). It has no codegen or runtime yet, so the commands it can serve
//! are exactly the ones that bottom out at "does this source type-check": parse,
//! compile (acts as a check), test, run, stats. Commands that need to *execute* a
//! program (frame, perf, engine, sim, ...) cannot be served by bone and return a
//! clear error from the dispatcher rather than silently doing nothing.
//!
//! Per `bone/CLAUDE.md` we do NOT modify any bone compiler source; this module
//! only *consumes* bone's public pipeline (`parse_rock` + `passes::semantic_pass`).

use crate::Result;
use bone_core::prelude::Instant;
use rock_core::CompilerError;
use rock_core::SourceFiles;
use rock_core::source_file_from_path_and_contents;
use skull::pipeline::ClosureGate;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Wall-clock split of bone's pipeline stages for one source string. bone fuses
/// lowering + type-checking into `passes::semantic_pass`, so the split we can
/// observe at the public boundary is `parse` vs `semantic` (lower+check). On an
/// early failure the stage that never ran stays zero (a parse error leaves
/// `semantic` at 0), so a sum across files attributes real work to each stage.
#[derive(Clone, Copy, Default)]
pub struct BonePhaseTimes {
    pub parse: Duration,
    /// Fused lower + check (`bone_semantic::semantic_passes::semantic_pass`). bone does not expose
    /// a lower/check boundary publicly.
    pub semantic: Duration,
    /// Cloning the (LazyLock-cached, pre-parsed) stdlib modules — a per-run
    /// harness cost incurred before `semantic_pass`, counted *within* `semantic`.
    /// Only set by the whole-workspace path (`bone_workspace_timed`).
    pub stdlib_clone: Duration,
    /// Appending the user modules onto the stdlib module list, also within
    /// `semantic` and before `semantic_pass`.
    pub stdlib_extend: Duration,
    /// The `semantic_pass` call measured from the caller, so it includes bone's
    /// own teardown (dropping the input ASTs at fn return) that bone's internal
    /// phase timers can't see. `stdlib_clone + stdlib_extend + semantic_call ==
    /// semantic`; `semantic_call == sum(bone phases) + teardown`.
    pub semantic_call: Duration,
}

impl BonePhaseTimes {
    pub fn total(&self) -> Duration {
        // stdlib_clone/extend are sub-components of `semantic`, not additive.
        self.parse + self.semantic
    }

    fn add(&mut self, other: &BonePhaseTimes) {
        self.parse += other.parse;
        self.semantic += other.semantic;
        self.stdlib_clone += other.stdlib_clone;
        self.stdlib_extend += other.stdlib_extend;
        self.semantic_call += other.semantic_call;
    }
}

/// All body-level type errors from a checked workspace, in source order
/// (`(file, offset)` — bone stores them per `BodyOwner` in hash maps, so map
/// iteration alone is nondeterministic). `semantic_pass` returning `Ok` means
/// "lowered and checked", NOT "no type errors"; every verdict-shaped consumer
/// must go through here or it will count broken bodies as passing.
pub(crate) fn body_errors(ws: &bone_semantic::inference::Workspace) -> Vec<CompilerError> {
    skull_mir::verdict::body_errors(ws)
}

/// Render every error in `errs` against `sources` and join into one report.
/// The one-shot CLI commands print ALL diagnostics (matching the LSP), not just
/// the first — a project with 5 type errors must not force a
/// fix-one-recompile-repeat loop.
pub(crate) fn render_errs(sources: &SourceFiles, errs: &[CompilerError]) -> String {
    // A diagnostic can land in an INLINED stdlib body, whose modules are parsed
    // under the loader's stdlib id and appear in no caller's source set -- the
    // span lookup then fails and the snippet plus every label is dropped. Added
    // here, the one sink all render paths share, rather than per caller.
    let mut sf = sources.clone();
    for f in rock_stdlib::stdlib_files() {
        let id = rock_stdlib::stdlib_file_id(f.name);
        sf.add_or_update_file(source_file_from_path_and_contents(
            id.clone(),
            id,
            f.contents.to_string(),
        ));
    }
    // An emit path that can only return ONE error attaches the rest as
    // siblings; expanded here for the same reason as the stdlib sources above --
    // this is the shared sink, and rendering only the primary is how a
    // twenty-site sink list got read as three.
    let expanded: Vec<CompilerError> = errs
        .iter()
        .flat_map(|e| {
            std::iter::once(CompilerError {
                siblings: Vec::new(),
                ..e.clone()
            })
            .chain(e.siblings.iter().cloned())
        })
        .collect();
    rock_core::errors::render_all(&expanded, &sf)
}

/// Bone's namespace facts for a parsed module set, through the loader the
/// editor shares ([`rock_host::program_inputs::ns_aliases_for_sources`]).
pub(crate) fn aliases_for_modules(
    modules: &[skull_parser::Ast],
) -> std::result::Result<bone_core::modules::NsAliases, CompilerError> {
    rock_host::program_inputs::ns_aliases_for_sources(
        modules
            .iter()
            .map(|m| (m.file.as_str(), m.dialect, m.package.as_ref())),
    )
}

/// The host's binding of [`skull::pipeline::check_modules`]: its alias
/// tables. Returns the verified workspace; `Err`
/// carries EVERY diagnostic of the first failing gate (all syntax errors,
/// else all type errors) so the CLI reports the same set the LSP shows.
pub fn check_modules(
    modules: &mut [skull_parser::Ast],
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    let (ws, errs) = check_modules_gated(modules, ClosureGate::Enforce)?;
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(ws)
}

/// [`check_modules`] for the modules tools, which must read a workspace whose
/// bodies have errors (the migrate planner compares resolution site by site
/// and repairs what changed) and which read the public-API type closure themselves
/// (`api_closure_requirements`). Every other gate is the same; the body
/// errors come back beside the workspace instead of replacing it.
pub fn check_modules_for_tools(
    modules: &mut [skull_parser::Ast],
) -> std::result::Result<
    (bone_semantic::inference::Workspace, Vec<CompilerError>),
    Vec<CompilerError>,
> {
    check_modules_gated(modules, ClosureGate::LeaveToCaller)
}

fn check_modules_gated(
    modules: &mut [skull_parser::Ast],
    gate: ClosureGate,
) -> std::result::Result<
    (bone_semantic::inference::Workspace, Vec<CompilerError>),
    Vec<CompilerError>,
> {
    skull::pipeline::check_modules(modules, gate, aliases_for_modules)
}

/// [`check_modules`] over the stdlib + `user`: the common "I just parsed user
/// code, give me a verified workspace" path (single-file emit, project compile,
/// iter benches).
pub fn check_with_stdlib(
    user: impl IntoIterator<Item = skull_parser::Ast>,
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    check_modules(&mut skull::pipeline::with_stdlib(user).map_err(|e| vec![e])?)
}

/// A bone incremental `Session` warmed with the stdlib — the watch/hot-reload
/// counterpart to [`check_with_stdlib`]. Every `Session::new` site passes the
/// same `skull_parser::stdlib::modules()` warm set; funnel them here.
///
/// This form does NOT resolve comptime inputs; it is for type-check-only
/// sessions (bone watch, iter benches, LSP survey, tests). A session whose
/// workspace will be EMITTED constructs through [`bone_session_for_emission`].
pub(crate) fn bone_session(
    files: Vec<bone_incremental::session::SessionFile>,
) -> std::result::Result<bone_incremental::session::Session, CompilerError> {
    // The session parses its own user modules, so the alias tables are derived
    // from the same facts `aliases_for_modules` reads off a parsed set: the
    // root package's manifest, reached through the file that carries graph id 0.
    let aliases = aliases_for_session(&files)?;
    bone_incremental::session::Session::new(skull_parser::stdlib::modules()?, files, &aliases)
}

/// [`aliases_for_modules`] for a session's file list, which is not parsed yet.
/// A set with no v1 file uses the empty table, exactly as the batch path does.
///
/// Every file with a package is carried, not just the v1 ones: the manifest the
/// graph is built from is found through the ROOT package's file (graph id 0),
/// and in a mixed build -- a legacy game consuming a v1 library, which is the
/// whole point of the compatibility projection -- the root is the legacy side.
pub(crate) fn aliases_for_session(
    files: &[bone_incremental::session::SessionFile],
) -> std::result::Result<bone_core::modules::NsAliases, CompilerError> {
    rock_host::program_inputs::ns_aliases_for_sources(
        files
            .iter()
            .map(|f| (f.path.as_str(), f.dialect, f.package.as_ref())),
    )
}

/// The session files for a discovered project: each file's text plus the
/// dialect and owning package the package graph stamped on it. A session
/// re-parses on every edit, so it needs the facts, not a one-shot stamp.
pub(crate) fn session_files(sources: &SourceFiles) -> Vec<bone_incremental::session::SessionFile> {
    sources
        .files
        .iter()
        .map(|source| bone_incremental::session::SessionFile::from_source(&source.borrow()))
        .collect()
}

/// [`bone_session`] plus comptime-input stamping against `anchor`'s enclosing
/// project -- the session counterpart of `comptime::stamp_workspace`, so every
/// emitting session flow (skull watch, LiveProject, the incremental profilers)
/// resolves declared inputs once per SESSION BUILD at construction. The stamp
/// survives `update_file` (the Session mutates its ws in place); a full
/// session rebuild constructs a new one through here and so re-resolves.
pub(crate) fn bone_session_for_emission(
    files: Vec<bone_incremental::session::SessionFile>,
    anchor: &std::path::Path,
) -> std::result::Result<bone_incremental::session::Session, CompilerError> {
    let mut session = bone_session(files)?;
    let inputs = crate::comptime::inputs_for_entry_path(anchor)
        .and_then(|inputs| {
            crate::comptime::resolve_checked_inputs(&session.ws, inputs, |file| {
                session.files.get(file).map(|source| source.text.clone())
            })
        })
        .map_err(|e| {
            CompilerError::new_no_location(format!("build var resolution failed: {e:#}"))
        })?;
    session.ws.comptime_inputs = inputs;
    Ok(session)
}

/// Run bone's full pipeline once, timing each stage separately. This is the one
/// place the bone calls live; everything else (check, test, profile,
/// `backend-stats`) layers on top so the timing split is consistent. Timed with
/// plain `Instant`s at the call boundaries — no coupling to bone, which stays
/// uninstrumented.
pub fn bone_pipeline_timed(
    src: &str,
) -> (std::result::Result<(), Vec<CompilerError>>, BonePhaseTimes) {
    let mut times = BonePhaseTimes::default();

    let t = Instant::now();
    let module = match skull_parser::parse_module(src, ustr::Ustr::from("<parse_rock>")) {
        Ok(m) => m,
        Err(e) => {
            times.parse = t.elapsed();
            return (Err(vec![e]), times);
        }
    };
    times.parse = t.elapsed();

    let t = Instant::now();
    let res = check_with_stdlib([module]).map(|_| ());
    times.semantic = t.elapsed();
    (res, times)
}

/// Parse every loaded *user* file into a bone module (timed) and identify the
/// entry module (`main.rock`, else the first file). Cross-file references only
/// resolve when the whole project is type-checked as one workspace, so this is
/// the parse step shared by the whole-project bone (`bone_workspace_timed`) and
/// skull (`skull_workspace_timed`) timers and by `skull_assemble_project`.
///
/// A `ModuleId` (a file's first-item span file) appearing twice would silently
/// merge in bone's by-id map and mispair arena indices, so duplicates are
/// rejected here rather than producing wrong results downstream.
pub(crate) fn parse_user_modules(
    sources: &SourceFiles,
) -> std::result::Result<(Vec<skull_parser::Ast>, ustr::Ustr, Duration), Vec<CompilerError>> {
    use rayon::prelude::*;
    let t = Instant::now();
    // Parse every module in parallel — each parse is independent (own arenas; the
    // only shared state, `Ustr` interning, is concurrent-safe). `par_iter`
    // preserves order, so the dedup/entry scan below sees files in the same order
    // the old serial loop did. Carry each module's id-path Ustr alongside its Ast.
    let parsed: Vec<(skull_parser::Ast, ustr::Ustr)> = sources
        .files
        .par_iter()
        .map(|file| {
            let f = file.borrow();
            let id = ustr::Ustr::from(f.file.to_id_path().as_str());
            let mut module = skull_parser::parse_module_with_dialect(&f.contents, id, f.dialect)
                .map_err(|e| vec![e])?;
            module.package = f.package.clone();
            module.global_policy = f.global_policy;
            Ok((module, id))
        })
        .collect::<std::result::Result<Vec<_>, Vec<CompilerError>>>()?;

    // Sequential post-pass: reject duplicate module ids and pick the entry file.
    let mut modules = Vec::with_capacity(parsed.len());
    let mut user_file: Option<ustr::Ustr> = None;
    let mut first_file: Option<ustr::Ustr> = None;
    let mut seen: HashMap<ustr::Ustr, ustr::Ustr> = HashMap::new();
    for (module, id) in parsed {
        // The module's file is the stable identity the workspace's interner maps
        // to a dense ModuleId after semantic_pass. Carry it here, resolve the id
        // from `ws.module_ids` once the workspace exists (see `entry_module_id`).
        let mfile = module.file;
        if let Some(prev) = seen.insert(mfile, id) {
            return Err(vec![CompilerError::new_no_location(format!(
                "bone: duplicate module id `{}` in project assembly (also `{}`)",
                mfile, prev
            ))]);
        }
        if first_file.is_none() {
            first_file = Some(mfile);
        }
        let ids = id.as_str();
        if ids.ends_with("/main.rock") || ids.ends_with("\\main.rock") || ids == "main.rock" {
            user_file = Some(mfile);
        }
        modules.push(module);
    }

    // Compile gate: parsing is total, so a project with any syntax error is
    // refused here (the whole-project bone/skull compile funnel) -- the
    // interactive Session keeps the partial tree instead.
    let syntax = skull::pipeline::syntax_errors(&modules);
    if !syntax.is_empty() {
        return Err(syntax);
    }

    let entry = user_file.or(first_file).ok_or_else(|| {
        vec![CompilerError::new_no_location(
            "bone: project has no .rock files",
        )]
    })?;
    Ok((modules, entry, t.elapsed()))
}

/// Resolve the entry module's `ModuleId` from a checked workspace. The entry is
/// identified by file path (`parse_user_modules`/`skull_assemble_project`); the
/// workspace's `module_ids` interner is the single path->id authority, so the id
/// is only knowable after `semantic_pass` has registered every module.
pub fn entry_module_id(
    ws: &bone_semantic::inference::Workspace,
    file: ustr::Ustr,
) -> bone_semantic::prelude::ModuleId {
    ws.module_ids[&file]
}

/// Whole-project bone timing: parse all user files, then type-check them
/// together with the stdlib as ONE workspace (the only way cross-file references
/// resolve). Unlike `bone_pipeline_timed` (one file in isolation), this is what
/// `backend-stats` uses so the bone column measures the same whole-project work
/// a real compile does — a per-file loop would reject every file that names a
/// sibling.
pub(crate) fn bone_workspace_timed(
    sources: &SourceFiles,
) -> (
    std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>>,
    BonePhaseTimes,
) {
    let mut times = BonePhaseTimes::default();
    let (user_modules, _mid, parse) = match parse_user_modules(sources) {
        Ok(v) => v,
        Err(e) => return (Err(e), times),
    };
    times.parse = parse;

    let t_sem = Instant::now();
    let t = Instant::now();
    let mut modules = match skull_parser::stdlib::modules() {
        Ok(modules) => modules,
        Err(error) => return (Err(vec![error]), times),
    };
    times.stdlib_clone = t.elapsed();
    let t = Instant::now();
    modules.extend(user_modules);
    times.stdlib_extend = t.elapsed();
    let aliases = match aliases_for_modules(&modules) {
        Ok(aliases) => aliases,
        Err(error) => return (Err(vec![error]), times),
    };
    // Time the call from out here so bone's teardown (dropping the input ASTs at
    // fn return) is included — bone's internal phase timers stop before it.
    let t_call = Instant::now();
    let res = semantic_pass_with_aliases(&mut modules, &aliases);
    times.semantic_call = t_call.elapsed();
    times.semantic = t_sem.elapsed();
    // The checked Workspace carries bone's own per-phase split (`phase_timings`)
    // for backend-stats and the symbol/loc counts for the [COMPILE] stats line.
    // `Ok(ws)` here means "lowered and checked", NOT "no type errors" -- body
    // errors live per-`BodyOwner` on the workspace, so every verdict-shaped
    // consumer must run `body_errors` on `ws`.
    (res.map_err(|e| vec![e]), times)
}

/// Render bone `CompilerError`s through rock's ariadne pipeline.
///
/// `parse_rock` always stamps spans with the file id `<parse_rock>` (it takes no
/// path), so the rendered `SourceFiles` must register the contents under that
/// same id for the span lookup to resolve — the real on-disk `path` is only used
/// in the human-facing header line we print separately.
fn render_source_errs(src: &str, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles::empty();
    sf.add_or_update_file(source_file_from_path_and_contents(
        "<parse_rock>".to_string(),
        "<parse_rock>".to_string(),
        src.to_string(),
    ));
    render_errs(&sf, errs)
}

/// Collapse a bone error message into a short, groupable bucket. A 489-file run
/// otherwise produces hundreds of unique multi-KB `Debug` dumps (each
/// "Unsupported top level item Item { .. }" is the full AST node); this keeps the
/// histogram and per-file lines readable while preserving the discriminating
/// detail (which item kind / which type ctor / which missing function).
pub fn categorize(msg: &str) -> String {
    // First identifier-ish token before a `{`, `(`, or whitespace.
    let head = |s: &str| -> String {
        s.trim()
            .split(|c: char| c == '{' || c == '(' || c.is_whitespace())
            .find(|t| !t.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let after = |s: &str, marker: &str| -> Option<String> {
        s.split_once(marker).map(|(_, rest)| head(rest))
    };

    if let Some(rest) = msg.strip_prefix("Unsupported top level item") {
        return format!(
            "unsupported top-level item: {}",
            after(rest, "kind: ").unwrap_or_default()
        );
    }
    if let Some(rest) = msg.strip_prefix("Unsupported type expr") {
        return format!("unsupported type expr: {}", head(rest));
    }
    if msg.starts_with("unsupported function body type") {
        return format!(
            "unsupported function body: {}",
            after(msg, "body: ").unwrap_or_default()
        );
    }
    if let Some(name) = msg
        .strip_prefix("Function ")
        .and_then(|r| r.strip_suffix(" doesn't exist"))
    {
        return format!("missing function/operator: {}", name);
    }
    if let Some(rest) = msg.strip_prefix("unsupported statement:") {
        return format!("unsupported statement: {}", head(rest));
    }
    if let Some(rest) = msg.strip_prefix("unsupported expr") {
        return format!("unsupported expr: {}", head(rest));
    }
    if let Some(got) = after(msg, ", got ") {
        // "Only ident-based fn calls are supported, got FieldAccess(..)"
        return format!("unsupported call form: {}", got);
    }

    // Fallback: first line of the message, char-truncated (byte slicing would
    // risk splitting a UTF-8 boundary).
    let line = msg.lines().next().unwrap_or(msg);
    if line.chars().count() > 70 {
        format!("{}...", line.chars().take(70).collect::<String>())
    } else {
        line.to_string()
    }
}

/// `rock compile`/`run --backend bone`. Bone has no codegen, so the most useful
/// behavior is a type-check: succeed silently (exit 0) when it type-checks, print
/// the rendered error and exit 1 otherwise.
///
/// A directory (or a single file that belongs to a `stick.toml` project) is
/// loaded and type-checked as ONE workspace via `bone_workspace_timed` — the only
/// way cross-file references resolve. A bare single file keeps the isolated
/// single-source pipeline. This makes `rock compile <project> --backend bone`
/// (e.g. another project) a whole-project typecheck, which is the workload `just
/// hyperfine-bone` benchmarks.
///
/// `iters` reuses the same profiling loop the Lua `compile --iters N` uses:
/// run the pipeline N times and report the mean per-stage (parse/lower/check)
/// timing on the `PROFILE` log target. `iters <= 1` reports a single timing.
pub fn bone_check_single(path: &str, command: &str, iters: usize) -> Result<()> {
    if Path::new(path).is_dir() {
        return bone_check_project(path, command, iters);
    }

    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "BONE", "cannot read {}: {}", path, e);
            crate::invocation_log::exit(1);
        }
    };

    let iters = iters.max(1);
    let mut agg = BonePhaseTimes::default();
    let mut first_err: Option<Vec<CompilerError>> = None;
    for _ in 0..iters {
        let (res, times) = bone_pipeline_timed(&src);
        agg.add(&times);
        if first_err.is_none() {
            if let Err(errs) = res {
                first_err = Some(errs);
            }
        }
    }

    if let Some(errs) = first_err {
        eprintln!("{}", render_source_errs(&src, &errs));
        crate::invocation_log::exit(1);
    }

    log::info!(target: "BONE", "{} type-checks ({})", path, command);
    report_phase_timing(path, &agg, iters);
    Ok(())
}

/// `rock compile`/`run --backend bone <project>` — load the whole project and
/// type-check every file together as one bone workspace. Discovery goes through
/// the SAME `path_to_source_spec` -> `discover_sources` path the Lua `rock
/// compile` uses (stick.toml + dep resolution, `#script` files excluded from
/// deps), so the file set bone checks is exactly the one a real compile would —
/// this is the workload `just hyperfine-bone` benchmarks. `iters` runs the
/// workspace check N times and reports the mean per-stage timing. On the first
/// failure, render it against the loaded sources and exit 1.
fn bone_check_project(path: &str, command: &str, iters: usize) -> Result<()> {
    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;

    let iters = iters.max(1);
    let mut agg = BonePhaseTimes::default();
    let mut last_ws: Option<bone_semantic::inference::Workspace> = None;
    let mut first_err: Option<Vec<CompilerError>> = None;
    for _ in 0..iters {
        let (res, times) = bone_workspace_timed(&sources);
        agg.add(&times);
        match res {
            // `Ok(ws)` only means the workspace lowered and checked; body type
            // errors live per-`BodyOwner` on `ws`, so collect them before
            // treating the run as a pass.
            Ok(ws) => {
                if first_err.is_none() {
                    let errs = body_errors(&ws);
                    if !errs.is_empty() {
                        first_err = Some(errs);
                    }
                }
                last_ws = Some(ws);
            }
            Err(errs) => {
                if first_err.is_none() {
                    first_err = Some(errs);
                }
            }
        }
    }

    if let Some(errs) = first_err {
        eprintln!("{}", render_errs(&sources, &errs));
        crate::invocation_log::exit(1);
    }

    log::info!(target: "BONE", "{} type-checks ({})", path, command);

    // rock-style [COMPILE] stats line. bone has no codegen or output-write
    // phase, so `done`/`compile` coincide -- the full parse + type-check
    // (`agg.total()`, meaned over iters), no output path.
    let ws = last_ws.expect("a successful iteration must have set last_ws");
    let compile = Duration::from_secs_f64(agg.total().as_secs_f64() / iters as f64);
    log_compile_stats(&sources, &ws, compile, compile, None);

    if iters > 1 {
        report_phase_timing(path, &agg, iters);
    }
    Ok(())
}

/// LoC stats for the [COMPILE] line: the discovered user files plus the stdlib,
/// matching the file set the Lua backend's tokenizer counts (so `lines`/`loc per
/// sec` line up across backends). Only `files`/`total_lines`/`bytes` are filled
/// -- the code/comment/blank split needs tokens and isn't shown in the headline.
fn bone_loc_stats(sources: &SourceFiles) -> rock_core::CompileStats {
    let mut stats = rock_core::CompileStats::default();
    for file in &sources.files {
        let f = file.borrow();
        stats.files += 1;
        stats.total_lines += f.contents.lines().count();
        stats.bytes += f.contents.len();
    }
    for f in rock_stdlib::stdlib_files().iter() {
        stats.files += 1;
        stats.total_lines += f.contents.lines().count();
        stats.bytes += f.contents.len();
    }
    stats
}

/// Emit the rock-style `[COMPILE] done in ...` stats line for a bone-workspace
/// compile -- shared by `--backend bone` (type-check only) and `--backend skull`
/// (type-check + codegen). Mirrors the Lua backend headline in
/// `unified_compile.rs`: symbols, total lines, loc/sec over the discovered user
/// files plus stdlib (the same set the Lua tokenizer counts, so figures line up
/// across backends). `compile` is the parse + check (+ codegen) core; `total`
/// adds any output write (== `compile` when the caller writes nothing here);
/// `output`, when set, is appended as `-> <path>`. `symbols` is bone's count of
/// top-level definitions (fns/structs/enums/newtypes/globals) across all modules.
/// Debug level: `rock run` on a project would otherwise print it before the
/// program's own output; `--verbose` or `ROCK_LOG=debug` shows it.
pub(crate) fn log_compile_stats(
    sources: &SourceFiles,
    ws: &bone_semantic::inference::Workspace,
    compile: Duration,
    total: Duration,
    output: Option<&str>,
) {
    let stats = bone_loc_stats(sources);
    let symbols: usize = ws
        .modules
        .iter()
        .map(|m| {
            m.functions.len() + m.structs.len() + m.enums.len() + m.newtypes.len() + m.globals.len()
        })
        .sum();
    let compile_ms = compile.as_secs_f64() * 1000.0;
    let total_ms = total.as_secs_f64() * 1000.0;
    match output {
        Some(out) => log::debug!(target: "COMPILE",
            "done in {:.2}ms (compile {:.2}ms, {} symbols, {} lines, {}) -> {}",
            total_ms, compile_ms, symbols, stats.total_lines, stats.format_throughput(compile), out),
        None => log::debug!(target: "COMPILE",
            "done in {:.2}ms (compile {:.2}ms, {} symbols, {} lines, {})",
            total_ms, compile_ms, symbols, stats.total_lines, stats.format_throughput(compile)),
    }
}

/// `rock watch --backend bone` — live whole-project typecheck loop backed by
/// an incremental `bone_semantic::Session`: the stdlib is lowered + inferred once at
/// startup and never again; each change re-checks the user modules through
/// `Session::update_file` (content hashes make untouched files free). Every
/// event re-discovers sources and diffs against the session's tracked set, so
/// file adds/deletes need no per-event path bookkeeping. A type error is
/// rendered and the loop keeps running, unlike the one-shot commands which
/// exit 1; a failed update (parse/macro/lowering error) keeps serving the
/// previous good workspace.
pub fn bone_watch(path: &str) -> Result<()> {
    // None until a Session::new succeeds: an unparseable file at startup is
    // rendered and retried on the next change, matching the old batch loop's
    // keep-running behavior.
    let mut session: Option<bone_incremental::session::Session> = None;

    let check = |label: &str, session: &mut Option<bone_incremental::session::Session>| {
        let source_spec = crate::common::path_to_source_spec(path);
        let sources = match crate::unified_compile::discover_sources(&source_spec) {
            Ok(s) => s,
            Err(e) => {
                log::error!(target: "WATCH", "bone: source discovery failed: {e}");
                return;
            }
        };
        let files = session_files(&sources);

        let t = Instant::now();
        let mut reinferred = 0usize;
        let mut update_failed = false;

        match session {
            None => match bone_session(files) {
                Ok(s) => *session = Some(s),
                Err(e) => {
                    eprintln!("{}", e.render(&sources));
                    log::error!(target: "WATCH",
                        "bone {label}: session init failed in {:.1}ms",
                        t.elapsed().as_secs_f64() * 1000.0);
                    return;
                }
            },
            Some(s) => {
                for f in &files {
                    match s.update_file(&f.path, &f.text) {
                        Ok(result) => reinferred += result.reinferred.len(),
                        Err(e) => {
                            update_failed = true;
                            eprintln!("{}", e.render(&sources));
                        }
                    }
                }
                let gone: Vec<String> = s
                    .tracked_paths()
                    .into_iter()
                    .filter(|p| !files.iter().any(|f| f.path == *p))
                    .map(|p| p.to_string())
                    .collect();
                for file_path in gone {
                    if let Err(e) = s.remove_file(&file_path) {
                        update_failed = true;
                        eprintln!("{}", e.render(&sources));
                    }
                }
            }
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        let Some(s) = session else { return };
        let errs = s.diagnostics();
        if update_failed {
            log::error!(target: "WATCH",
                "bone {label}: update failed in {ms:.1}ms - serving last good state");
        } else if errs.is_empty() {
            log::info!(target: "WATCH",
                "bone {label}: {} files type-check in {ms:.1}ms ({reinferred} bodies re-inferred)",
                sources.files.len());
        } else {
            for e in &errs {
                eprintln!("{}", e.render(&sources));
            }
            log::error!(target: "WATCH",
                "bone {label}: {} body error(s) in {ms:.1}ms ({reinferred} bodies re-inferred)",
                errs.len());
        }
    };

    check("initial", &mut session);

    crate::run_compile_watch_loop(
        vec![std::path::PathBuf::from(path)],
        &format!("bone: watching {path} for changes"),
        |_event| check("recheck", &mut session),
    )
}

/// Log a per-stage timing breakdown on the `PROFILE` target (the same target
/// the Lua `--iters` loop uses). `agg` is the SUM over `iters` runs; we divide
/// to report the mean.
fn report_phase_timing(label: &str, agg: &BonePhaseTimes, iters: usize) {
    let iters = iters.max(1);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / iters as f64;
    if iters > 1 {
        log::info!(target: "PROFILE",
            "bone {}: {:.3}ms/iter = parse {:.3} + semantic {:.3} (mean over {} iters)",
            label, ms(agg.total()), ms(agg.parse), ms(agg.semantic), iters);
    } else {
        log::info!(target: "PROFILE",
            "bone {}: {:.3}ms = parse {:.3} + semantic {:.3}",
            label, ms(agg.total()), ms(agg.parse), ms(agg.semantic));
    }
}

/// Trim a long absolute path to something readable in the per-file test line,
/// keeping the last two components (`dir/file.rock`).
pub fn short_path(path: &str) -> String {
    let p = Path::new(path);
    let file = p.file_name().and_then(|s| s.to_str()).unwrap_or(path);
    match p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|s| s.to_str())
    {
        Some(dir) => format!("{}/{}", dir, file),
        None => file.to_string(),
    }
}

/// The batch semantic pass with the API-closure gate the build enforces.
pub fn semantic_pass_with_aliases(
    modules: &mut [skull_parser::Ast],
    aliases: &bone_core::modules::NsAliases,
) -> std::result::Result<bone_semantic::inference::Workspace, CompilerError> {
    skull::pipeline::semantic_pass(modules, aliases, ClosureGate::Enforce)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A program is REJECTED if the check fails (syntax, semantic-pass, or any
    // collected body/interface error).
    fn rejected(src: &str) -> bool {
        let module =
            skull_parser::parse_module(src, ustr::Ustr::from("<verdict_test>")).expect("parse");
        check_with_stdlib([module]).is_err()
    }

    // An unknown type in INTERFACE position (signature / global / field) lowers to
    // `Error` + a soft `interface_errors` entry under total lowering, so the pass
    // returns Ok. The CLI verdict (the gate for `rock compile/test/run --backend
    // bone|skull`) must still REJECT it -- otherwise skull emits Lua over the
    // error type. Regression for the verdict-path body_errors dropping
    // interface_errors.
    #[test]
    fn interface_type_errors_are_rejected_by_the_verdict() {
        assert!(
            rejected("fn f(x: Nonexistent) -> int { 1 }"),
            "unknown param type"
        );
        assert!(
            rejected("global gg: Nonexistent = 0"),
            "unknown global type"
        );
        assert!(
            rejected("struct S { x: Nonexistent }"),
            "unknown field type"
        );
        // sanity: a clean program is NOT rejected.
        assert!(
            !rejected("fn ok(x: int) -> int { x }"),
            "clean program must pass"
        );
    }

    // `const` globals are read-only: a whole-global rebind (`=` or `+=`) is a
    // compile error; a mutable `global` write is fine. The bone fixture
    // `const_readonly.rock` covers the plain `=` form in the bare (no-stdlib)
    // harness; this pins the compound `+=` form (which desugars to a stdlib
    // `__op_add` call the bare harness can't resolve) plus the mutable controls.
    #[test]
    fn const_globals_are_read_only() {
        assert!(
            rejected("const FOO: int = 5\nfn f() { FOO = 6 }"),
            "rebind const"
        );
        assert!(
            rejected("const FOO: int = 5\nfn f() { FOO += 1 }"),
            "compound-assign const"
        );
        // controls: mutable globals stay writable.
        assert!(
            !rejected("global BAR: int = 5\nfn f() { BAR = 6 }"),
            "mutable global write"
        );
        assert!(
            !rejected("global BAR: int = 5\nfn f() { BAR += 1 }"),
            "mutable global compound"
        );
    }

    // A diagnostic can land in an INLINED stdlib body, whose modules are parsed
    // under the loader's stdlib id and belong to no project's source set.
    // Without the registration in `render_errs` the header still prints and the
    // snippet plus every label is silently dropped. Renderer-level on purpose: a
    // lowering change can move where a rejection LANDS -- which is how the
    // end-to-end fixture that used to cover this went vacuous -- but it cannot
    // move the span-to-source lookup this asserts.
    #[test]
    fn a_diagnostic_spanned_into_stdlib_renders_its_snippet() {
        const NEEDLE: &str = "fn Vec<T>.push(";
        let unified = rock_stdlib::stdlib_files()
            .iter()
            .find(|f| f.name == "unified.rock")
            .expect("unified.rock is a stdlib module");
        let offset = unified
            .contents
            .find(NEEDLE)
            .expect("needle still in unified.rock");
        let head = &unified.contents[..offset];
        let line = head.matches('\n').count();
        let col = offset - head.rfind('\n').map_or(0, |i| i + 1);
        let span = rock_core::Span {
            file: ustr::Ustr::from(rock_stdlib::stdlib_file_id("unified.rock").as_str()),
            line_range: (line, line),
            col_range: (col, col + NEEDLE.len()),
            offset_range: (offset, offset + NEEDLE.len()),
        };
        let err = CompilerError::new_with_label(span, "boxed here".to_string());
        let rendered = render_errs(&SourceFiles::empty(), &[err]);

        assert!(
            rendered.contains(NEEDLE),
            "the stdlib source line must render, not be dropped:\n{rendered}"
        );
        assert!(
            rendered.contains("boxed here"),
            "the label must render:\n{rendered}"
        );
    }

    #[test]
    fn cli_warm_stdlib_uses_loader_file_ids() {
        let expected: Vec<_> = rock_stdlib::stdlib_files()
            .iter()
            .map(|f| rock_stdlib::stdlib_file_id(f.name))
            .collect();
        let session = bone_session(Vec::new()).expect("CLI warm session must build");
        let actual: Vec<_> = session
            .ws
            .modules
            .iter()
            .filter(|module| session.warm_mids.contains(&module.id))
            .map(|module| module.file.as_str().to_owned())
            .collect();
        assert_eq!(
            actual, expected,
            "CLI and LSP must warm the same stdlib file ids"
        );
    }
}
