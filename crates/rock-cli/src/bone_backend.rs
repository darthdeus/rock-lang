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
use rock_core::CompilerError;
use rock_core::SourceFiles;
use rock_core::source_file_from_path_and_contents;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Wall-clock split of bone's pipeline stages for one source string. bone fuses
/// lowering + type-checking into `passes::semantic_pass`, so the split we can
/// observe at the public boundary is `parse` vs `semantic` (lower+check). On an
/// early failure the stage that never ran stays zero (a parse error leaves
/// `semantic` at 0), so a sum across files attributes real work to each stage.
#[derive(Clone, Copy, Default)]
pub struct BonePhaseTimes {
    pub parse: Duration,
    /// Fused lower + check (`bone_semantic::semantic_passes::semantic_pass`). bone does not expose
    /// a lower/check boundary publicly; see the compiler contract.
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
            std::iter::once(CompilerError { siblings: Vec::new(), ..e.clone() })
                .chain(e.siblings.iter().cloned())
        })
        .collect();
    rock_core::errors::render_all(&expanded, &sf)
}

/// The bone stdlib with `user` modules appended — the checkable module set every
/// non-incremental path feeds to bone. The SINGLE spot that assembles "stdlib +
/// user"; callers used to spell `bone_stdlib_modules()` + `push`/`extend` inline
/// (a dozen copies, which is why a `semantic_pass` signature tweak rippled
/// everywhere).
pub(crate) fn with_stdlib(
    user: impl IntoIterator<Item = skull_parser::Ast>,
) -> Vec<skull_parser::Ast> {
    let mut modules = bone_stdlib_modules();
    modules.extend(user);
    modules
}

/// Convert the already-resolved host package graph into bone's namespace
/// facts. Source files carry dense graph ids, so deriving this once from the
/// root package's manifest keeps v1 alias lookup tied to the same graph that
/// selected the files. Synthetic and legacy-only module sets intentionally use
/// the empty table.
pub(crate) fn aliases_for_modules(
    modules: &[skull_parser::Ast],
) -> std::result::Result<bone_core::modules::NsAliases, CompilerError> {
    let has_v1 = modules.iter().any(|module| {
        module.dialect == skull_parser::Dialect::V1 && module.package.is_some()
    });
    if !has_v1 {
        return Ok(bone_core::modules::NsAliases::default());
    }

    let root = modules.iter().find(|module| {
        module
            .package
            .as_ref()
            .is_some_and(|package| package.id == 0)
    });
    let Some(root) = root else {
        return Err(CompilerError::new_no_location(
            "modules-v1 sources have no root package file from which to load dependency aliases",
        ));
    };
    let mut manifest_dir = Path::new(root.file.as_str()).parent().map(Path::to_path_buf);
    let manifest_dir = loop {
        let Some(dir) = manifest_dir.as_ref() else {
            return Err(CompilerError::new_no_location(format!(
                "cannot find stick.toml for modules-v1 root source `{}`",
                root.file
            )));
        };
        if dir.join("stick.toml").is_file() {
            break dir.clone();
        }
        let Some(parent) = dir.parent() else {
            manifest_dir = None;
            continue;
        };
        if parent == dir {
            manifest_dir = None;
        } else {
            manifest_dir = Some(parent.to_path_buf());
        }
    };

    bone_query::package_aliases::for_manifest_dir(&manifest_dir)
        .map_err(CompilerError::new_no_location)
}

/// Run bone's front half over an already-assembled `modules` set and gate on the
/// load-verdict: the one "modules -> verified Workspace" step. `Err` carries
/// EVERY diagnostic (all syntax errors, else all type errors) so the CLI reports
/// the same set the LSP shows. A front-end signature change touches HERE, not
/// every call site.
pub fn check_modules(
    modules: &mut [skull_parser::Ast],
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    let (ws, errs) = check_modules_inner(modules, ClosureGate::Enforce)?;
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(ws)
}

/// [`check_modules`] for the modules tools, which must read a workspace whose
/// bodies have errors (the migrate planner compares resolution site by site
/// and repairs what changed) and which read the spec 11 closure themselves
/// (`api_closure_requirements`). Every other gate is the same; the body
/// errors come back beside the workspace instead of replacing it.
pub fn check_modules_for_tools(
    modules: &mut [skull_parser::Ast],
) -> std::result::Result<(bone_semantic::inference::Workspace, Vec<CompilerError>), Vec<CompilerError>> {
    check_modules_inner(modules, ClosureGate::LeaveToCaller)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClosureGate {
    Enforce,
    LeaveToCaller,
}

fn check_modules_inner(
    modules: &mut [skull_parser::Ast],
    gate: ClosureGate,
) -> std::result::Result<(bone_semantic::inference::Workspace, Vec<CompilerError>), Vec<CompilerError>> {
    let syntax = syntax_errors(modules);
    if !syntax.is_empty() {
        return Err(syntax);
    }
    // Macro names must be unique against function names (call-position ambiguity)
    // -- checked before `semantic_pass` expands the macro defs away.
    let macro_dupes = skull_parser::check_macro_name_uniqueness(modules);
    if !macro_dupes.is_empty() {
        return Err(macro_dupes);
    }
    // A macro argument label must name one of that macro's parameters -- also
    // before expansion, which is what drops the labels.
    let macro_labels = skull_parser::check_macro_call_labels(modules);
    if !macro_labels.is_empty() {
        return Err(macro_labels);
    }
    let aliases = aliases_for_modules(modules).map_err(|error| vec![error])?;
    let ws = match gate {
        ClosureGate::Enforce => semantic_pass_with_aliases(modules, &aliases),
        ClosureGate::LeaveToCaller => {
            semantic_pass_without_closure_gate(modules, &aliases)
        }
    };
    // Expansion RECOVERS: a macro that must not expand (item-macro `mod`/`use`)
    // records the reason on `recovery_errors` and leaves the call in place, so
    // the syntax gate runs again over the expanded trees. The pass's own
    // verdict comes second -- the expansion reason explains any type error the
    // unexpanded call caused. `Session`/LSP still never run this gate.
    let expanded_syntax = syntax_errors(modules);
    if !expanded_syntax.is_empty() {
        return Err(expanded_syntax);
    }
    let ws = ws.map_err(|e| vec![e])?;
    let errs = body_errors(&ws);
    Ok((ws, errs))
}

/// Every recovered syntax error across `modules`, in module order. The
/// compile-path gate: parsing is TOTAL (returns the best-effort tree +
/// `recovery_errors`), so the CLI compile/test paths refuse a program with any
/// syntax error HERE rather than at the parse boundary. This keeps the strict
/// compile verdict while the interactive `Session`/LSP -- which never run this
/// gate -- keep the partial tree and surface the same errors as diagnostics.
fn syntax_errors(modules: &[skull_parser::Ast]) -> Vec<CompilerError> {
    modules.iter().flat_map(|m| m.recovery_errors.iter().cloned()).collect()
}

/// [`check_modules`] over the stdlib + `user`: the common "I just parsed user
/// code, give me a verified workspace" path (single-file emit, project compile,
/// iter benches).
pub fn check_with_stdlib(
    user: impl IntoIterator<Item = skull_parser::Ast>,
) -> std::result::Result<bone_semantic::inference::Workspace, Vec<CompilerError>> {
    check_modules(&mut with_stdlib(user))
}

/// A bone incremental `Session` warmed with the stdlib — the watch/hot-reload
/// counterpart to [`check_with_stdlib`]. Every `Session::new` site passes the
/// same `bone_stdlib_modules()` warm set; funnel them here.
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
    bone_incremental::session::Session::new_with_identity_storage_exemptions(bone_stdlib_modules(), files, &aliases, crate::registry::registered().identity_storage_exemptions)
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
    let stubs: Vec<skull_parser::Ast> = files
        .iter()
        .filter(|f| f.package.is_some())
        .map(|f| {
            let mut ast = skull_parser::Ast::default();
            ast.file = ustr::Ustr::from(f.path.as_str());
            ast.dialect = f.dialect;
            ast.package = f.package.clone();
            ast.memory_class = f.memory_class;
            ast.global_policy = f.global_policy;
            ast
        })
        .collect();
    aliases_for_modules(&stubs)
}

/// The session files for a discovered project: each file's text plus the
/// dialect and owning package the package graph stamped on it. A session
/// re-parses on every edit, so it needs the facts, not a one-shot stamp.
pub(crate) fn session_files(sources: &SourceFiles) -> Vec<bone_incremental::session::SessionFile> {
    sources
        .files
        .iter()
        .map(|source| {
            let source = source.borrow();
            let path = source.file.to_id_path();
            crate::compile_measurements::source(&path, source.contents.len());
            bone_incremental::session::SessionFile {
                path,
                text: source.contents.clone(),
                dialect: source.dialect,
                package: source.package.clone(),
                memory_class: source.memory_class,
                global_policy: source.global_policy,
            }
        })
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
    session.ws.comptime_inputs = crate::comptime::inputs_for_entry_path(anchor)
        .map_err(|e| CompilerError::new_no_location(format!("build var resolution failed: {e:#}")))?;
    Ok(session)
}

/// Run bone's full pipeline once, timing each stage separately. This is the one
/// place the bone calls live; everything else (check, test, profile,
/// `backend-stats`) layers on top so the timing split is consistent. Timed with
/// plain `Instant`s at the call boundaries — no coupling to bone, which stays
/// uninstrumented.
pub fn bone_pipeline_timed(src: &str) -> (std::result::Result<(), Vec<CompilerError>>, BonePhaseTimes) {
    let stats = crate::compile_measurements::Scope::begin("<parse_rock>");
    crate::compile_measurements::source("<parse_rock>", src.len());
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

    crate::compile_measurements::phase("parse");
    let t = Instant::now();
    let res = check_with_stdlib([module]).map(|_| ());
    times.semantic = t.elapsed();

    crate::compile_measurements::phase("stdlib+semantic");
    if res.is_ok() { stats.success(); }
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
    let parsed: Vec<(skull_parser::Ast, ustr::Ustr, usize)> = sources
        .files
        .par_iter()
        .map(|file| {
            let f = file.borrow();
            let id = ustr::Ustr::from(f.file.to_id_path().as_str());
            let mut module =
                skull_parser::parse_module_with_dialect(&f.contents, id, f.dialect)
                    .map_err(|e| vec![e])?;
            module.package = f.package.clone();
            module.memory_class = f.memory_class;
            module.global_policy = f.global_policy;
            Ok((module, id, f.contents.len()))
        })
        .collect::<std::result::Result<Vec<_>, Vec<CompilerError>>>()?;

    // Sequential post-pass: reject duplicate module ids and pick the entry file.
    let mut modules = Vec::with_capacity(parsed.len());
    let mut user_file: Option<ustr::Ustr> = None;
    let mut first_file: Option<ustr::Ustr> = None;
    let mut seen: HashMap<ustr::Ustr, ustr::Ustr> = HashMap::new();
    for (module, id, bytes) in parsed {
        crate::compile_measurements::source(id.as_str(), bytes);
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
    let syntax = syntax_errors(&modules);
    if !syntax.is_empty() {
        return Err(syntax);
    }

    let entry = user_file
        .or(first_file)
        .ok_or_else(|| vec![CompilerError::new_no_location("bone: project has no .rock files")])?;
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
    let stats = crate::compile_measurements::Scope::for_sources(sources);
    let mut times = BonePhaseTimes::default();
    let (user_modules, _mid, parse) = match parse_user_modules(sources) {
        Ok(v) => v,
        Err(e) => return (Err(e), times),
    };
    times.parse = parse;

    let t_sem = Instant::now();
    let t = Instant::now();
    let mut modules = bone_stdlib_modules();
    times.stdlib_clone = t.elapsed();
    let t = Instant::now();
    modules.extend(user_modules);
    times.stdlib_extend = t.elapsed();
    crate::compile_measurements::phase("parse+stdlib");
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

    crate::compile_measurements::phase("semantic");
    if let Ok(ws) = &res {
        crate::compile_measurements::timings("bone", &ws.phase_timings);
        stats.success();
    }
    // The checked Workspace carries bone's own per-phase split (`phase_timings`)
    // for backend-stats and the symbol/loc counts for the [COMPILE] stats line.
    // `Ok(ws)` here means "lowered and checked", NOT "no type errors" -- body
    // errors live per-`BodyOwner` on the workspace, so every verdict-shaped
    // consumer must run `body_errors` on `ws`.
    (res.map_err(|e| vec![e]), times)
}

/// The parsed stdlib modules, so test files can resolve stdlib symbols (`print`,
/// `__op_add`, `assert_eq`, `vec_new`, `JsonValue`, ...). Iterates the single
/// shared `stdlib_files()` list real rock embeds (`unified.rock`, `json.rock`, and
/// anything added later) so bone stays in stdlib parity instead of hand-rolling
/// its own subset. Parsed once and cloned per check.
///
/// bone keys modules by file (`Ast.file`, interned to a dense `ModuleId`), so
/// each stdlib file uses the loader-owned id shared with the LSP. (Re-checked
/// per file for now; caching a checked workspace is a later optimization.)
pub(crate) fn bone_stdlib_modules() -> Vec<skull_parser::Ast> {
    use std::sync::LazyLock;
    static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|f| {
                let id = ustr::Ustr::from(
                    rock_stdlib::stdlib_file_id(f.name).as_str(),
                );
                skull_parser::parse_module(f.contents, id)
                    .unwrap_or_else(|e| panic!("stdlib {} must parse: {e:?}", f.name))
            })
            .collect()
    });
    STDLIB.clone()
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

/// Collect every `.rock` file at `path` (a single file yields itself; a
/// directory is walked recursively). Mirrors the discovery the Lua backend's
/// `rock test` uses so the same paths select the same files.
pub(crate) fn collect_rock_files(path: &str) -> Result<Vec<String>> {
    let p = Path::new(path);
    if !p.exists() {
        anyhow::bail!("Path does not exist: {}", path);
    }
    if p.is_file() {
        return Ok(vec![path.to_string()]);
    }

    rock_host::project::find_rock_files_at_path(p)
}

/// `rock parse --backend bone` — run each file through bone's front end
/// (`parse_rock` + `semantic_pass`, which lowers to HIR and type-checks; bone
/// no longer exposes a lowering-only entry). "parse" here means "bone accepts
/// this file". Exits non-zero on the first failure, matching the Lua backend's
/// `do_parse`.
pub fn bone_parse(path: &str) -> Result<()> {
    let files = collect_rock_files(path)?;
    for file in &files {
        let src = fs_err::read_to_string(file)?;
        let module = match skull_parser::parse_module(&src, ustr::Ustr::from("<parse_rock>")) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{}", render_source_errs(&src, &[e]));
                crate::ablation_report::exit_with_census(1);
            }
        };
        // No stdlib here (this path checks a single module in isolation).
        if let Err(errs) = check_modules(&mut [module]) {
            eprintln!("{}", render_source_errs(&src, &errs));
            crate::ablation_report::exit_with_census(1);
        }
    }
    Ok(())
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
            crate::ablation_report::exit_with_census(1);
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
        crate::ablation_report::exit_with_census(1);
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
        crate::ablation_report::exit_with_census(1);
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
        Some(out) => log::info!(target: "COMPILE",
            "done in {:.2}ms (compile {:.2}ms, {} symbols, {} lines, {}) -> {}",
            total_ms, compile_ms, symbols, stats.total_lines, stats.format_throughput(compile), out),
        None => log::info!(target: "COMPILE",
            "done in {:.2}ms (compile {:.2}ms, {} symbols, {} lines, {})",
            total_ms, compile_ms, symbols, stats.total_lines, stats.format_throughput(compile)),
    }
}

/// `rock iter-compile <project> --iters N` — build the bone workspace ONCE
/// (parse + lower + infer) and then re-run only the inference phase N times. The
/// loop body is exactly the "infer functions" phase of `semantic_pass`
/// (`ws.modules -> check_module`).
///
/// Parsing/lowering are hoisted out of the loop, so a sampling profiler
/// (`samply record -- rock iter-compile <project> --iters N`) attributes nearly
/// all samples to inference (`check_function` and below) instead of the parser.
/// Use a large N (e.g. 1000-5000) so the process runs long enough for the
/// sampler to collect statistically meaningful data.
///
/// The loop runs in parallel by default (reproducing the real workload, incl.
/// per-thread intern/lock contention). Built with `--features sequential` it
/// runs single-threaded with `.iter()` — matching bone's prelude shim — for a
/// clean serial profile with no rayon plumbing frames.
pub fn bone_iter_inference(path: &str, iters: usize, warmup: usize) -> Result<()> {
    #[cfg(not(feature = "sequential"))]
    use rayon::prelude::*;

    let source_spec = crate::common::path_to_source_spec(path);
    let sources = crate::unified_compile::discover_sources(&source_spec)?;

    // Build the workspace once: parse + expand + desugar + lower + infer.
    let ws = {
        let (user_modules, _mid, _parse) = match parse_user_modules(&sources) {
            Ok(v) => v,
            Err(errs) => {
                eprintln!("{}", render_errs(&sources, &errs));
                crate::ablation_report::exit_with_census(1);
            }
        };
        // Bench harness: build the workspace once for the inference loop; no
        // load-verdict gate (we time inference, not acceptance).
        let mut modules = with_stdlib(user_modules);
        let aliases = match aliases_for_modules(&modules) {
            Ok(aliases) => aliases,
            Err(e) => {
                eprintln!("{}", e.render(&sources));
                crate::ablation_report::exit_with_census(1);
            }
        };
        match semantic_pass_with_aliases(&mut modules, &aliases) {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("{}", e.render(&sources));
                crate::ablation_report::exit_with_census(1);
            }
        }
    };

    let iters = iters.max(1);

    // One inference pass over all modules. Returns wall time in ms.
    let one_pass = || -> f64 {
        let t = Instant::now();
        #[cfg(not(feature = "sequential"))]
        let res: std::result::Result<(), CompilerError> = ws
            .modules
            .par_iter()
            .try_for_each(|module| bone_semantic::semantic_passes::check_module(&ws, module.id, module).map(|_| ()));
        #[cfg(feature = "sequential")]
        let res: std::result::Result<(), CompilerError> = ws
            .modules
            .iter()
            .try_for_each(|module| bone_semantic::semantic_passes::check_module(&ws, module.id, module).map(|_| ()));
        if let Err(e) = res {
            eprintln!("{}", e.render(&sources));
            crate::ablation_report::exit_with_census(1);
        }
        t.elapsed().as_secs_f64() * 1000.0
    };

    // Untimed warmup: ramp CPU frequency + prime caches/branch predictor before
    // sampling, so the recorded distribution reflects steady state, not cold start.
    for _ in 0..warmup {
        one_pass();
    }

    // Sample every iteration individually so we can report the real distribution
    // of a single inference pass (mean/p50/p99/stddev), not just the loop average.
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        samples.push(one_pass());
    }

    let n = samples.len() as f64;
    let total: f64 = samples.iter().sum();
    let mean = total / n;
    let var = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
    let stddev = var.sqrt();
    let cv = if mean > 0.0 { stddev / mean * 100.0 } else { 0.0 };
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| -> f64 {
        let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx]
    };
    let (min, max) = (sorted[0], sorted[sorted.len() - 1]);

    log::info!(target: "PROFILE",
        "bone iter-inference {}: {} iters ({} warmup) x {} modules",
        path, iters, warmup, ws.modules.len());
    log::info!(target: "PROFILE",
        "  mean {:.4}  p50 {:.4}  p99 {:.4}  min {:.4}  max {:.4}  stddev {:.4} (cv {:.2}%)  [ms/iter]",
        mean, pct(50.0), pct(99.0), min, max, stddev, cv);
    Ok(())
}

/// A signature edit: insert a dummy first param into `fn <name>` so its arity
/// (interface) changes -- the alternation original<->edited drives the
/// Interface-tier re-inference each pass. `None` if the fn/parens aren't found.
fn sig_edit(text: &str, name: &str) -> Option<String> {
    let fpos = text.find(&format!("fn {name}"))?;
    let paren = text[fpos..].find('(')? + fpos;
    let after = paren + 1;
    let close = text[paren..].find(')')? + paren;
    let insert = if text[after..close].trim().is_empty() {
        "__reinfer_x: int"
    } else {
        "__reinfer_x: int, "
    };
    let mut out = String::with_capacity(text.len() + insert.len());
    out.push_str(&text[..after]);
    out.push_str(insert);
    out.push_str(&text[after..]);
    Some(out)
}

/// Add a defaulted field to `struct <name>`'s body -- an interface change (the
/// struct's shape shifts, so its interface_snapshot moves and the cascade
/// re-lowers its dependents) that keeps existing `Struct { .. }` literals valid
/// (the default fills the new field), so the profile isolates re-lower cost from
/// missing-field constructor errors.
/// Byte offset of the `struct <name>` declaration, requiring a word boundary
/// after the name so `Mob` doesn't match `struct MobSpawner` (substring).
fn struct_decl_pos(text: &str, name: &str) -> Option<usize> {
    let needle = format!("struct {name}");
    let mut from = 0;
    while let Some(rel) = text[from..].find(&needle) {
        let pos = from + rel;
        let after = pos + needle.len();
        let boundary = text[after..]
            .chars()
            .next()
            .map_or(true, |c| !c.is_alphanumeric() && c != '_');
        if boundary {
            return Some(pos);
        }
        from = after;
    }
    None
}

fn field_edit(text: &str, name: &str) -> Option<String> {
    let spos = struct_decl_pos(text, name)?;
    let brace = text[spos..].find('{')? + spos;
    let after = brace + 1;
    let insert = "\n    __cascade_probe: int = 0,";
    let mut out = String::with_capacity(text.len() + insert.len());
    out.push_str(&text[..after]);
    out.push_str(insert);
    out.push_str(&text[after..]);
    Some(out)
}

/// `rock iter-cascade <project>` -- tight loop benching the Interface-tier
/// RE-LOWER cascade: a field edit on a high-fan-in TYPE re-lowers every module
/// whose interface references it. The re-lower sibling of `iter-reinfer` (which
/// edits a fn signature and only re-infers). Auto-picks the project struct with
/// the most `dependents_iface` entries; `ITER_CASCADE_TYPE=<name>` overrides.
pub fn bone_iter_cascade(path: &str, iters: usize, warmup: usize) -> Result<()> {
    use std::path::PathBuf;

    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(path));
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let files = session_files(&sources);
    let mut session =
        bone_session(files.clone()).map_err(|e| anyhow::anyhow!("session warm: {}", e.message))?;

    let proj_marker = format!(
        "{}/",
        Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );

    // Target the project struct with the most INTERFACE dependents (the edit that
    // re-lowers the widest closure).
    let tname = if let Ok(t) = std::env::var("ITER_CASCADE_TYPE") {
        t
    } else {
        let mut best = (String::new(), 0usize);
        for module in session.ws.modules.iter() {
            let file = module.file.as_str();
            if session.warm_mids.contains(&module.id) || !file.contains(&proj_marker) {
                continue;
            }
            for name in module.struct_by_name.keys() {
                let c = session.dependents_iface.get(name).map(|s| s.len()).unwrap_or(0);
                if c > best.1 {
                    best = (name.as_str().to_string(), c);
                }
            }
        }
        if best.0.is_empty() {
            anyhow::bail!("no project struct with interface dependents under `{path}`");
        }
        best.0
    };
    let iface_deps = session.dependents_iface.get(&ustr::Ustr::from(&tname)).map(|s| s.len()).unwrap_or(0);

    let (target, orig) = files
        .iter()
        .find(|f| f.path.contains(&proj_marker) && struct_decl_pos(&f.text, &tname).is_some())
        .map(|f| (f.path.clone(), f.text.clone()))
        .ok_or_else(|| anyhow::anyhow!("no project file defines `struct {tname}`"))?;
    let edited =
        field_edit(&orig, &tname).ok_or_else(|| anyhow::anyhow!("no field edit for `struct {tname}`"))?;

    let iters = iters.max(1);
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    let mut last_relowered_mods = 0usize;
    let mut last_reinferred = 0usize;
    let mut last_tier = String::new();
    for i in 0..(warmup + iters) {
        let text = if i % 2 == 0 { &edited } else { &orig };
        let t = Instant::now();
        let upd = session
            .update_file(&target, text)
            .map_err(|e| anyhow::anyhow!("update_file: {}", e.message))?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if i >= warmup {
            samples.push(ms);
            last_relowered_mods = upd
                .relowered
                .iter()
                .map(|(mid, _)| *mid)
                .collect::<rustc_hash::FxHashSet<_>>()
                .len();
            last_reinferred = upd.reinferred.len();
            last_tier = format!("{:?}", upd.tier);
        }
    }

    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];
    let target_name = Path::new(&target)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    eprintln!("=== Bone Iter-Cascade: {path} ===");
    eprintln!(
        "field edit on `struct {tname}` ({target_name}) | {iface_deps} interface dependents | tier {last_tier}"
    );
    eprintln!(
        "  per pass: {last_relowered_mods} modules re-lowered, {last_reinferred} bodies re-inferred | {iters} iters ({warmup} warmup)\n"
    );
    eprintln!(
        "  update_file [ms]:  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}",
        mean,
        pct(50.0),
        pct(99.0),
        sorted[0],
        sorted[sorted.len() - 1]
    );
    Ok(())
}

/// `rock iter-reinfer <project>` -- tight loop benching the Interface-tier
/// re-inference path: a signature edit on a high-fan-out fn reinfers every
/// dependent body. Warms a Session, then alternates a signature edit (a dummy
/// param) and times `update_file`, reporting the reinferred-body count + the
/// per-edit timing distribution. `ITER_REINFER_FN=<name>` targets the fn; else
/// the project fn with the most dependent modules is auto-picked. A tight body
/// for samply (`samply record -- rock iter-reinfer project --iters 500`).
pub fn bone_iter_reinfer(path: &str, iters: usize, warmup: usize) -> Result<()> {
    use std::path::PathBuf;

    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(path));
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let files = session_files(&sources);
    let mut session =
        bone_session(files.clone()).map_err(|e| anyhow::anyhow!("session warm: {}", e.message))?;

    let proj_marker = format!(
        "{}/",
        Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );

    let fname = if let Ok(f) = std::env::var("ITER_REINFER_FN") {
        f
    } else {
        // Auto-pick the project fn whose name has the most dependent modules
        // (the highest-fan-out interface edit).
        let mut best = (String::new(), 0usize);
        for module in session.ws.modules.iter() {
            let file = module.file.as_str();
            if session.warm_mids.contains(&module.id) || !file.contains(&proj_marker) {
                continue;
            }
            for name in module.functions_by_name.keys() {
                let c = session.dependents.get(name).map(|s| s.len()).unwrap_or(0);
                if c > best.1 {
                    best = (name.as_str().to_string(), c);
                }
            }
        }
        if best.0.is_empty() {
            anyhow::bail!("no project fn with dependents under `{path}`");
        }
        best.0
    };

    let (target, orig) = files
        .iter()
        .find(|f| f.path.contains(&proj_marker) && f.text.contains(&format!("fn {fname}")))
        .map(|f| (f.path.clone(), f.text.clone()))
        .ok_or_else(|| anyhow::anyhow!("no project file defines `fn {fname}`"))?;
    let edited =
        sig_edit(&orig, &fname).ok_or_else(|| anyhow::anyhow!("no signature edit for `fn {fname}`"))?;

    let iters = iters.max(1);
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    let mut last_reinferred = 0usize;
    let mut last_tier = String::new();
    let mut last_phases: Vec<(&'static str, Duration)> = Vec::new();
    for i in 0..(warmup + iters) {
        let text = if i % 2 == 0 { &edited } else { &orig };
        let t = Instant::now();
        let upd = session
            .update_file(&target, text)
            .map_err(|e| anyhow::anyhow!("update_file: {}", e.message))?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let phases = session.workspace().phase_timings.clone();
        if i >= warmup {
            samples.push(ms);
            last_reinferred = upd.reinferred.len();
            last_tier = format!("{:?}", upd.tier);
            last_phases = phases;
        }
    }

    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];
    let target_name = Path::new(&target)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    eprintln!("=== Bone Iter-Reinfer: {path} ===");
    eprintln!(
        "signature edit on `{fname}` ({target_name}) | tier {last_tier} | {last_reinferred} bodies reinferred/pass | {iters} iters ({warmup} warmup)\n"
    );
    eprintln!(
        "  update_file per pass [ms]:  mean {:.3}  p50 {:.3}  p99 {:.3}  min {:.3}  max {:.3}",
        mean,
        pct(50.0),
        pct(99.0),
        sorted[0],
        sorted[sorted.len() - 1]
    );
    eprintln!("  last-pass phases (edited module's lower+infer; dependent loop is the residual):");
    for (lbl, d) in &last_phases {
        eprintln!("    {lbl}: {:.3} ms", d.as_secs_f64() * 1000.0);
    }
    let known: f64 = last_phases.iter().map(|(_, d)| d.as_secs_f64() * 1000.0).sum();
    eprintln!(
        "    (dependent check_module loop + parse/classify, residual: {:.3} ms)",
        (mean - known).max(0.0)
    );
    Ok(())
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
    match p.parent().and_then(|d| d.file_name()).and_then(|s| s.to_str()) {
        Some(dir) => format!("{}/{}", dir, file),
        None => file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A program is REJECTED if the check fails (syntax, semantic-pass, or any
    // collected body/interface error).
    fn rejected(src: &str) -> bool {
        let module = skull_parser::parse_module(src, ustr::Ustr::from("<verdict_test>"))
            .expect("parse");
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
        assert!(rejected("fn f(x: Nonexistent) -> int { 1 }"), "unknown param type");
        assert!(rejected("global gg: Nonexistent = 0"), "unknown global type");
        assert!(rejected("struct S { x: Nonexistent }"), "unknown field type");
        // sanity: a clean program is NOT rejected.
        assert!(!rejected("fn ok(x: int) -> int { x }"), "clean program must pass");
    }

    // `const` globals are read-only: a whole-global rebind (`=` or `+=`) is a
    // compile error; a mutable `global` write is fine. The bone fixture
    // `const_readonly.rock` covers the plain `=` form in the bare (no-stdlib)
    // harness; this pins the compound `+=` form (which desugars to a stdlib
    // `__op_add` call the bare harness can't resolve) plus the mutable controls.
    #[test]
    fn const_globals_are_read_only() {
        assert!(rejected("const FOO: int = 5\nfn f() { FOO = 6 }"), "rebind const");
        assert!(rejected("const FOO: int = 5\nfn f() { FOO += 1 }"), "compound-assign const");
        // controls: mutable globals stay writable.
        assert!(!rejected("global BAR: int = 5\nfn f() { BAR = 6 }"), "mutable global write");
        assert!(!rejected("global BAR: int = 5\nfn f() { BAR += 1 }"), "mutable global compound");
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
        let offset = unified.contents.find(NEEDLE).expect("needle still in unified.rock");
        let head = &unified.contents[..offset];
        let line = head.matches('\n').count();
        let col = offset - head.rfind('\n').map_or(0, |i| i + 1);
        let span = rock_core::Span {
            file: ustr::Ustr::from(
                rock_stdlib::stdlib_file_id("unified.rock").as_str(),
            ),
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
        assert!(rendered.contains("boxed here"), "the label must render:\n{rendered}");
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
        assert_eq!(actual, expected, "CLI and LSP must warm the same stdlib file ids");
    }
}

/// Batch and incremental compiles share the host's migration policy.
pub(crate) fn semantic_pass_without_closure_gate(
    modules: &mut [skull_parser::Ast],
    aliases: &bone_core::modules::NsAliases,
) -> std::result::Result<bone_semantic::inference::Workspace, CompilerError> {
    let ws = bone_semantic::inference::Workspace {
        identity_storage_exemptions: crate::registry::registered().identity_storage_exemptions,
        ..Default::default()
    };
    bone_semantic::semantic_passes::semantic_pass_into(ws, modules, aliases)
}

pub fn semantic_pass_with_aliases(
    modules: &mut [skull_parser::Ast],
    aliases: &bone_core::modules::NsAliases,
) -> std::result::Result<bone_semantic::inference::Workspace, CompilerError> {
    let ws = semantic_pass_without_closure_gate(modules, aliases)?;
    bone_semantic::semantic_passes::pass_validate_api_closure(&ws)?;
    Ok(ws)
}
