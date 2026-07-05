//! The bone-backed LSP path: one consolidated facade over the `bone`
//! typechecker, selected by `ROCK_LSP_BACKEND=bone` / `--backend bone`.
//!
//! The rock-semantic path is untouched. Each `*_workspace` handler gets a
//! single early branch into the matching method here; the persistent server
//! holds one [`BoneWorkspace`] alongside its rock-semantic state. Everything
//! bone can't answer yet lives in [`gaps`] -- that is the "fill in what's
//! missing on bone" surface. See `docs/lsp-bone-migration-plan.md`.
//!
//! Consumes bone's public pipeline (`bone_incremental::session::Session`, `bone_semantic::query`)
//! plus this crate's IDE-navigation queries (`crate::query`). bone source stays
//! the engine author's; this crate is Claude-owned.

pub mod gaps;
mod macro_refs;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, Diagnostic, DiagnosticSeverity,
    DocumentHighlight, InsertTextFormat,
    DocumentSymbol, Hover,
    HoverContents, InlayHint, InlayHintKind, InlayHintLabel, Location, MarkupContent, MarkupKind,
    ParameterInformation, ParameterLabel, Position, Range, SemanticToken, SemanticTokenModifier,
    SemanticTokenType,
    SemanticTokens, SignatureHelp, SignatureInformation, SymbolInformation, SymbolKind, TextEdit,
    Uri, WorkspaceEdit,
};

use bone_semantic::inference::{self, Gamma};
use bone_semantic::prelude::{BodyInference, InferenceResult, ModuleId, Span, TyId, Workspace};
use bone_incremental::session::{Session, UpdateTier};
use rock_syntax::CompilerError;
use ustr::Ustr;

use crate::query::{clean_match, fz_boundary, SymCat};

/// Re-parse a tracked user file from its stored text. The Session no longer
/// retains the AST (it is transient -- the per-body HIR is the incremental
/// store), so the IDE queries that need surface syntax (typed-hole reports,
/// macro-param references) re-parse on demand. These are interactive, one-shot
/// queries, not a hot loop, so a parse per call is fine. Returns the raw
/// (un-cooked) AST, matching what the queries consumed when it was stored.
fn reparse_user_file(uf: &bone_incremental::session::UserFile, path: &str) -> Option<skull_parser::Ast> {
    skull_parser::parse_module(&uf.text, Ustr::from(path)).ok()
}

// ---------------------------------------------------------------------------
// Warm stdlib (parsed once, cloned into every session)
// ---------------------------------------------------------------------------

/// The real stdlib, parsed through skull-parser (bone's front end) with a
/// per-file id (`<bone-stdlib:NAME>`), so warm modules never collide with user
/// module ids and are visibly distinguishable in diagnostics. Parsing with the
/// id stamps every span, so bone's `ModuleId` (the first item's `span.file`)
/// lands on it directly. Parsed once per process.
fn warm_stdlib() -> Vec<skull_parser::Ast> {
    static WARM: OnceLock<Vec<skull_parser::Ast>> = OnceLock::new();
    WARM.get_or_init(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .filter_map(|f| {
                let id = Ustr::from(format!("<bone-stdlib:{}>", f.name).as_str());
                match skull_parser::parse_module(f.contents, id) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        // A broken stdlib breaks every bone compile; surface it
                        // loudly rather than silently dropping the module.
                        log::error!("bone LSP: stdlib `{}` failed to parse: {e:?}", f.name);
                        None
                    }
                }
            })
            .collect()
    })
    .clone()
}

// ---------------------------------------------------------------------------
// BoneWorkspace: one Session per stick-project root
// ---------------------------------------------------------------------------

/// Per-root bone state: the incremental session plus the latest text of every
/// tracked file (needed because bone queries are keyed on byte offsets, but the
/// session does not retain raw text).
struct BoneProject {
    session: Session,
    texts: HashMap<String, String>,
    /// Per-file unfilled-hole diagnostics, cached across edits. `hole_diagnostics_for`
    /// REPARSES a file, so recomputing it for every tracked file on every keystroke
    /// reparses the whole compilation unit (~130 files on mag) when only the edited
    /// file + its reinferred dependents can have changed holes. Invalidated for
    /// exactly those files in `update`.
    hole_cache: HashMap<String, Vec<Diagnostic>>,
    /// Type-reachability index for type-directed completion paths. A pure
    /// function of the type graph (struct fields + fn signatures), so it is
    /// rebuilt only on a non-`BodyOnly` edit and reused across body keystrokes.
    /// `None` until the first build (in `update`).
    reach: Option<crate::reachability::ReachabilityIndex>,
    /// Rendered function signatures (completion `detail`), keyed by name with
    /// first-overload-wins to match `crate::query::completion`'s dedup order. Like
    /// `reach`, a pure function of the type graph (signatures): a `BodyOnly`
    /// edit cannot change any signature, so this survives body keystrokes and
    /// is rebuilt only on a non-`BodyOnly` edit. Without it, completion
    /// re-renders every workspace signature on every keystroke -- the dominant
    /// per-request cost. `None` until the first build (in `update`).
    fn_sigs: Option<HashMap<ustr::Ustr, String>>,
    /// Precomputed call-through index (fused-query completion, `rejtanog ->
    /// reject_target(..)`). Like `reach`/`fn_sigs`, a pure function of the type
    /// graph (fn names + first-param types, enum variants, reach-from-globals),
    /// so it survives `BodyOnly` keystrokes and is rebuilt only on a
    /// non-`BodyOnly` edit. Without it, `call_through_param_completions` rebuilt
    /// an O(types*globals) reachability expansion on EVERY keystroke (the
    /// 20-50ms per-keystroke stall). `None` until the first build (in `update`).
    call_through: Option<CallThroughIndex>,
}

/// All bone-backed LSP state for the running server. Holds one [`Session`] per
/// stick-project root, created lazily on first edit. Multi-root publish-authority
/// routing (the rock-semantic path's diamond handling) is intentionally not
/// modeled here yet -- documented gap, closes as bone grows.
#[derive(Default)]
pub struct BoneWorkspace {
    projects: HashMap<PathBuf, BoneProject>,
    /// Project roots already batch-indexed from disk. The first `update` for a
    /// file in a `stick.toml` project bulk-loads its siblings so cross-file
    /// symbols resolve; later edits are incremental and must NOT re-load (that
    /// would clobber unsaved buffers).
    loaded_roots: std::collections::HashSet<PathBuf>,
    /// Verbatim editor path -> canonical path. Project discovery keys modules
    /// by the canonicalized path (`project_sources`), but LSP clients send the
    /// URI-decoded spelling (`C:\...` not `\\?\C:\...`; `/./` segments), so
    /// every public entry canonicalizes its `path` to this form first. Cached
    /// because `canonicalize` is a filesystem syscall and `update` is the
    /// keystroke hot path -- `update` warms the cache so the same-keystroke
    /// read queries hit it syscall-free.
    path_canon: HashMap<String, String>,
}

/// The "fill in missing match arms" edit: a text replacement (the generated
/// `EnumName::Variant => {}` arms) and how many variants it adds.
pub struct MatchArmsFill {
    pub range: Range,
    pub new_text: String,
    pub missing: usize,
}

/// The "fill struct fields" edit: a text replacement that inserts `field: ?`
/// for each missing required field, and how many fields it adds.
pub struct StructFieldsFill {
    pub range: Range,
    pub new_text: String,
    pub missing: usize,
}

/// The "Generate function" edit: a pure insertion (zero-width range) of a stub
/// `fn name(...) { ? }` after the enclosing function, plus the function name for
/// the action title.
pub struct GenerateFunctionEdit {
    pub range: Range,
    pub new_text: String,
    pub fn_name: String,
}

/// Canonicalize `path` WITHOUT Windows' `\\?\` extended-length prefix, falling
/// back to the input verbatim if it doesn't resolve (an unsaved buffer).
///
/// `fs_err::canonicalize` (= `std::fs::canonicalize`) returns the verbatim
/// `\\?\C:\...` form on Windows. That prefix disables path normalization in
/// downstream Windows APIs (breaking compilation-unit file discovery) and never
/// string-matches a path produced any other way -- and module lookup here is an
/// exact `Ustr` compare (`user_module_for_path`). Stripping it keeps every path
/// in the plain `C:\...` form so discovery, the editor's path, and the module
/// keys all agree. No-op on Unix (no prefix to strip). Mirrors the `dunce`
/// crate; kept dependency-free since the LSP only ever sees normal file paths.
pub fn canonicalize_lenient(path: &str) -> String {
    let canon = match fs_err::canonicalize(path) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return path.to_string(),
    };
    if let Some(unc) = canon.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(drive) = canon.strip_prefix(r"\\?\") {
        drive.to_string()
    } else {
        canon
    }
}

impl BoneWorkspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Canonicalize `path` (collapse `/./`, resolve to the OS canonical form),
    /// falling back to the input verbatim if it doesn't exist on disk (a
    /// not-yet-saved buffer). Idempotent: discovery and `root_for`/`stick_root`
    /// already canonicalize, so re-canonicalizing here yields the same key.
    fn canon_str(path: &str) -> String {
        canonicalize_lenient(path)
    }

    /// `canon_str` with memoization -- for the `&mut self` mutators on the
    /// keystroke hot path, so a file is `canonicalize`d once (didOpen) rather
    /// than per edit.
    fn canon_cached(&mut self, path: &str) -> String {
        if let Some(c) = self.path_canon.get(path) {
            return c.clone();
        }
        let c = Self::canon_str(path);
        self.path_canon.insert(path.to_string(), c.clone());
        c
    }

    /// `canon_str` for the `&self` read queries: hits the cache `update` warmed
    /// (no syscall on the hot path), falling back to a syscall only for a path
    /// never passed to a mutator.
    fn canon(&self, path: &str) -> String {
        self.path_canon.get(path).cloned().unwrap_or_else(|| Self::canon_str(path))
    }

    /// The deepest enclosing `stick.toml` directory for `path`, or None for a
    /// standalone file with no project. Determines whether `update` should
    /// batch-index siblings (real project) or stay single-file (scratch file).
    fn stick_root(path: &str) -> Option<PathBuf> {
        let p = Path::new(path);
        let canon = fs_err::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let mut cur = canon.parent();
        while let Some(dir) = cur {
            if dir.join("stick.toml").exists() {
                return Some(dir.to_path_buf());
            }
            cur = dir.parent();
        }
        None
    }

    /// Resolve the session key for `path`: the deepest enclosing `stick.toml`
    /// directory, else the file's parent directory (standalone file). Must be
    /// deterministic so an edit and a later query land in the same session.
    fn root_for(path: &str) -> PathBuf {
        if let Some(root) = Self::stick_root(path) {
            return root;
        }
        let p = Path::new(path);
        let canon = fs_err::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        canon
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or(canon)
    }

    fn project_mut(&mut self, root: &Path) -> &mut BoneProject {
        self.projects
            .entry(root.to_path_buf())
            .or_insert_with(|| BoneProject {
                // Warm-only session is always valid even if the first file
                // doesn't parse; the file is added by the update_file below.
                session: Session::new(warm_stdlib(), Vec::new())
                    .expect("bone session warm-init (stdlib) must succeed"),
                texts: HashMap::new(),
                hole_cache: HashMap::new(),
                reach: None,
                fn_sigs: None,
                call_through: None,
            })
    }

    fn project(&self, path: &str) -> Option<&BoneProject> {
        self.projects.get(&Self::root_for(path))
    }

    /// Load every `.rock` file under `path`'s project root into the session, so
    /// cross-file queries (references/rename) see the whole project rather than
    /// only the files an editor happened to open. Idempotent; reads from disk.
    /// The one-shot CLI calls this before a cross-file query; an interactive
    /// server can call it once per project root to index it eagerly.
    pub fn load_project(&mut self, path: &str) {
        self.batch_load_for_file(path);
    }

    /// Per-project loaded-file summary for the `rock/modules` introspection
    /// request: `(root, sorted module ids)` for every indexed project. The LSP
    /// serves one root per server, but a session can hold several (scratch files
    /// keyed by parent dir), so this returns all of them as separate contexts.
    pub fn projects_summary(&self) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = self
            .projects
            .iter()
            .map(|(root, proj)| {
                let mut ids: Vec<String> = proj.texts.keys().cloned().collect();
                ids.sort();
                (root.display().to_string(), ids)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Batch-create the session from `file_path`'s whole compilation unit -- the
    /// project's own files PLUS its `stick.toml` path-dependencies (e.g. a game's
    /// engine) -- so cross-file and cross-package references resolve. The file set
    /// is exactly the one a real `rock compile` uses
    /// (`compilation_units::load_sources_for_compilation_unit`), so the LSP sees
    /// the same program the compiler does. Incremental `update_file` reverts a
    /// module whose dependencies load later, so a one-file-at-a-time load would
    /// flag every cross-file symbol as unknown; batching avoids that. Falls back
    /// to per-file incremental loading if the batch fails (e.g. a file with a
    /// parse error, which `Session::new` rejects wholesale). Marks the root loaded
    /// so `update` won't re-index (and clobber unsaved buffers).
    fn batch_load_for_file(&mut self, file_path: &str) {
        let root = Self::root_for(file_path);
        self.loaded_roots.insert(root.clone());
        let Some(files) = Self::project_sources(file_path) else {
            return; // couldn't resolve the unit; `update` still adds the one file
        };
        match Session::new(warm_stdlib(), files.clone()) {
            Ok(session) => {
                let texts = files.into_iter().collect();
                self.projects.insert(
                    root,
                    BoneProject { session, texts, hole_cache: HashMap::new(), reach: None, fn_sigs: None, call_through: None },
                );
            }
            Err(e) => {
                log::warn!(
                    "batch load failed for `{}` ({e:?}); falling back to per-file indexing",
                    root.display()
                );
                for (p, src) in files {
                    let _ = self.update(&p, src);
                }
            }
        }
    }

    /// The (canonical-path, contents) of every source file in `file_path`'s
    /// compilation unit: the project plus its `stick.toml` dependencies, the same
    /// resolution `rock compile` performs. Paths are canonicalized so they match
    /// the canonical paths the LSP queries with (`root_for` / the CLI's
    /// `canonical`); a mismatch would leave the opened file's module unfindable.
    fn project_sources(file_path: &str) -> Option<Vec<(String, String)>> {
        let unit =
            rock_host::compilation_units::determine_compilation_unit_for_file(file_path).ok()?;
        let sources = rock_host::compilation_units::load_sources_for_compilation_unit(&unit).ok()?;
        let mut out = Vec::new();
        for f in &sources.files {
            let f = f.borrow();
            let raw = f.file.to_id_path();
            out.push((canonicalize_lenient(&raw), f.contents.clone()));
        }
        Some(out)
    }

    /// Apply an editor edit (didOpen/didChange) for `path`. Returns diagnostics
    /// for **every** tracked file in the affected session, so the caller can
    /// republish cross-file changes (and clear files that are now clean).
    pub fn update(&mut self, path: &str, text: String) -> Vec<(String, Vec<Diagnostic>)> {
        // Canonicalize so the editor's path spelling matches the canonical keys
        // project discovery used; otherwise the same file lands under two keys
        // (duplicate module -> "duplicate struct" / a half-built session crash).
        let path_c = self.canon_cached(path);
        let path = path_c.as_str();
        // First touch of a real (`stick.toml`) project root: batch-index its
        // siblings so cross-file types/functions resolve. Without this, opening
        // one file of a multi-file project flags every cross-file reference as
        // "unknown". Scratch files (no `stick.toml`) stay single-file. The
        // edited file's buffer text is applied below, overriding the disk copy
        // the batch load read.
        if let Some(proj_root) = Self::stick_root(path) {
            if !self.loaded_roots.contains(&proj_root) {
                self.batch_load_for_file(path);
            }
        }

        // `BONE_LSP_PROFILE=1` prints a per-call breakdown of update's three
        // sub-phases (re-typecheck / diagnostics scan / hole pass), the lens for
        // per-keystroke LSP latency. Mirrors `SKULL_EMIT_PROFILE`. Read once --
        // `update` is on the keystroke hot path, so no per-call env alloc.
        let prof = lsp_profile_enabled();

        let root = Self::root_for(path);
        let proj = self.project_mut(&root);
        proj.texts.insert(path.to_string(), text.clone());

        // A parse/lower failure of *this* file reverts the session to its last
        // good state for it; surface that one error on this file directly.
        // Holes can only change in files that were re-typechecked, so collect
        // the changed set (edited file + reinferred dependents) to drive the
        // hole cache below.
        let t_uf = std::time::Instant::now();
        let mut this_file_parse_err: Option<CompilerError> = None;
        let mut changed: std::collections::HashSet<String> = std::collections::HashSet::new();
        changed.insert(path.to_string());
        // Tier governs the type-graph-derived caches (reachability index +
        // completion signature cache): both depend only on the type graph
        // (struct fields + fn signatures), which a `BodyOnly` edit cannot
        // change -- so rebuild only on a non-`BodyOnly` edit (or the very first
        // build), and reuse them across body keystrokes.
        let mut rebuild_type_caches = proj.reach.is_none() || proj.fn_sigs.is_none();
        match proj.session.update_file(path, &text) {
            Ok(upd) => {
                if !matches!(upd.tier, UpdateTier::BodyOnly | UpdateTier::NoChange) {
                    rebuild_type_caches = true;
                }
                let ws = proj.session.workspace();
                for (mid, _) in &upd.reinferred {
                    changed.insert(ws.module(*mid).file.as_str().to_string());
                }
            }
            Err(e) => this_file_parse_err = Some(e),
        }
        if rebuild_type_caches {
            let t_reach = std::time::Instant::now();
            let idx = crate::reachability::ReachabilityIndex::build(proj.session.workspace());
            if prof {
                eprintln!(
                    "[bone reach] index build {:.2}ms ({})",
                    t_reach.elapsed().as_secs_f64() * 1000.0,
                    idx.stats()
                );
            }
            proj.reach = Some(idx);
            let t_sigs = std::time::Instant::now();
            let sigs = build_fn_sigs(proj.session.workspace());
            if prof {
                eprintln!(
                    "[bone completion] sig-cache build {:.2}ms ({} fns)",
                    t_sigs.elapsed().as_secs_f64() * 1000.0,
                    sigs.len()
                );
            }
            proj.fn_sigs = Some(sigs);
            let t_ct = std::time::Instant::now();
            let ct = build_call_through(proj.session.workspace(), proj.reach.as_ref().unwrap());
            if prof {
                eprintln!(
                    "[bone completion] call-through index build {:.2}ms ({} fns, {} types)",
                    t_ct.elapsed().as_secs_f64() * 1000.0,
                    ct.fns.len(),
                    ct.global_type_args.len()
                );
            }
            proj.call_through = Some(ct);
        }
        let uf_ms = t_uf.elapsed().as_secs_f64() * 1000.0;

        let t_diag = std::time::Instant::now();
        let mut by_file: HashMap<String, Vec<Diagnostic>> = HashMap::new();
        // Seed every tracked file with an empty vec so clean files get a
        // clearing publish.
        for p in proj.texts.keys() {
            by_file.entry(p.clone()).or_default();
        }

        for e in proj.session.diagnostics() {
            let file = e.location.file.as_str();
            // Errors with no/synthetic location are attributed to the edited file.
            let key = if file.is_empty() || !proj.texts.contains_key(file) {
                path.to_string()
            } else {
                file.to_string()
            };
            by_file
                .entry(key)
                .or_default()
                .push(compiler_error_to_diagnostic(&e));
        }

        if let Some(e) = &this_file_parse_err {
            // Override: current editor text for this file is broken; show that.
            by_file.insert(path.to_string(), vec![compiler_error_to_diagnostic(e)]);
        }
        let diag_ms = t_diag.elapsed().as_secs_f64() * 1000.0;

        // Unfilled-hole diagnostics. `hole_diagnostics_for` REPARSES, so we
        // recompute as few files as possible (a full scan reparses the whole
        // compilation unit every keystroke). A file's hole POSITIONS come from
        // its text and its hole TYPE MESSAGES from its inference, so recompute
        // when either could have changed:
        //   - the edited file (text changed -> positions may change), always;
        //   - a reinferred dependent (text UNCHANGED this edit, inference
        //     changed -> only messages can shift): recompute ONLY if it had
        //     holes last time -- an authoritatively hole-free file with
        //     unchanged text is still hole-free, so skip its reparse;
        //   - everything else: serve the cache (compute once if never seen).
        // `None` from hole_diagnostics_for means "couldn't compute" (no
        // inference / parse fail) -- never cached, so it isn't mistaken for a
        // known-hole-free file.
        let t_holes = std::time::Instant::now();
        let mut reparsed = 0usize;
        for (p, diags) in by_file.iter_mut() {
            if p == path && this_file_parse_err.is_some() {
                // Now-broken file: drop any stale cached holes; show the parse error only.
                proj.hole_cache.remove(p);
                continue;
            }

            let is_edited = p == path;
            let is_dependent = !is_edited && changed.contains(p.as_str());
            // A reinferred dependent that was authoritatively hole-free last time
            // (cached, empty) still is -- its text didn't change this edit.
            let dependent_holefree =
                is_dependent && proj.hole_cache.get(p).is_some_and(|v| v.is_empty());

            let must_recompute = if is_edited {
                true
            } else if is_dependent {
                !dependent_holefree
            } else {
                !proj.hole_cache.contains_key(p)
            };

            let holes = if must_recompute {
                reparsed += 1;
                match hole_diagnostics_for(proj, p) {
                    Some(v) => {
                        proj.hole_cache.insert(p.clone(), v.clone());
                        v
                    }
                    None => {
                        // Couldn't compute authoritatively: don't cache (retry
                        // next chance), surface nothing this round.
                        proj.hole_cache.remove(p);
                        Vec::new()
                    }
                }
            } else {
                proj.hole_cache.get(p).cloned().unwrap_or_default()
            };
            diags.extend(holes);
        }
        let holes_ms = t_holes.elapsed().as_secs_f64() * 1000.0;

        if prof {
            eprintln!(
                "[bone update] update_file {uf_ms:.2}ms  diag-scan {diag_ms:.2}ms  holes {holes_ms:.2}ms ({reparsed}/{} files reparsed)",
                by_file.len()
            );
        }

        by_file.into_iter().collect()
    }

    /// Drop a closed/removed file from its session.
    pub fn remove(&mut self, path: &str) {
        let path_c = self.canon_cached(path);
        let path = path_c.as_str();
        let root = Self::root_for(path);
        if let Some(proj) = self.projects.get_mut(&root) {
            proj.texts.remove(path);
            proj.hole_cache.remove(path);
            let _ = proj.session.remove_file(path);
        }
    }

    // -- Feature: hover (implemented via bone_semantic::query::type_at) ---------------

    pub fn hover(&self, path: &str, pos: Position) -> Option<Hover> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);

        // Whitespace guard: hovering the blanks between tokens resolves to
        // nothing. Without this, container exprs (match/if/block) -- which now
        // render their value for their keyword/braces -- would be a catch-all
        // hover over the whitespace inside them. ASCII-only (whitespace is ASCII;
        // a multi-byte char's lead byte isn't whitespace, so it proceeds).
        if src.as_bytes().get(offset).is_none_or(|b| b.is_ascii_whitespace()) {
            return None;
        }

        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;

        // A `?` hole under the cursor gets the full typed-hole report (kind +
        // inferred/expected type + in-scope bindings) -- the same content the
        // `rock-lsp hole-info` CLI prints -- rather than `hover_at`'s bare
        // `Type: ...`. Fenced so the multi-line "In scope:" block keeps its
        // formatting in a Markdown editor.
        if let Some(uf) = proj.session.files.get(path) {
            if let Some(hi) = reparse_user_file(uf, path)
                .and_then(|ast| crate::query::hole_info(ws, mid, inf, &ast, offset))
            {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```\n{}```", format_hole_report(&hi)),
                    }),
                    range: Some(range_from_offsets(
                        src,
                        hi.span.offset_range.0,
                        hi.span.offset_range.1,
                    )),
                });
            }
        }

        // Enum-variant match patterns (`MobKind::Skeleton`, incl. the `::`) are
        // resolved from the raw AST FIRST, so a pattern wins over the enclosing
        // match expr (which now renders its own value for its keyword/braces).
        // bone's HIR pattern carries no span, hence the raw-AST read.
        let pat_ast = proj
            .session
            .files
            .get(path)
            .and_then(|uf| reparse_user_file(uf, path));
        let pat_hover = pat_ast
            .as_ref()
            .and_then(|ast| crate::query::enum_variant_pattern_hover(ws, ast, offset));
        // Node resolution runs first (so a param NAME/TYPE in the signature
        // resolves to itself); the fn-signature hover is the FALLBACK for the
        // signature tokens no node covers (`fn`/parens/`->`), bounded by `src`
        // so body keywords (`let`/`for`) past the brace aren't swallowed.
        let (contents, span) = pat_hover
            .or_else(|| crate::query::hover_at(ws, mid, inf, offset))
            .or_else(|| crate::query::fn_signature_hover(ws, mid, src, offset))?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: contents,
            }),
            range: Some(range_from_offsets(src, span.offset_range.0, span.offset_range.1)),
        })
    }

    /// All diagnostics for a single file (used by the one-shot CLI handler):
    /// bone's typecheck errors plus the unfilled-hole warnings/hints.
    pub fn diagnostics_for(&self, path: &str) -> Vec<Diagnostic> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Vec::new();
        };
        let mut diags: Vec<Diagnostic> = proj
            .session
            .diagnostics()
            .into_iter()
            .filter(|e| e.location.file.as_str() == path || e.location.file.as_str().is_empty())
            .map(|e| compiler_error_to_diagnostic(&e))
            .collect();
        diags.extend(hole_diagnostics_for(proj, path).unwrap_or_default());
        diags
    }

    // -- Feature: goto-definition (stubbed: gaps::definition) ----------------

    pub fn definition(&self, path: &str, pos: Position) -> Option<Location> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;
        let span = gaps::definition(ws, mid, inf, offset)?;
        self.location_from_span(proj, &span)
    }

    // -- Feature: goto-type-definition (gaps::type_definition) ----------------

    /// LSP `textDocument/typeDefinition`: jump from the value under the cursor to
    /// the declaration of its *type* (on `mob` -> `struct Mob`). None when the
    /// cursor isn't on a typed node or the type has no navigable declaration.
    pub fn type_definition(&self, path: &str, pos: Position) -> Option<Location> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;
        let span = gaps::type_definition(ws, mid, inf, offset)?;
        self.location_from_span(proj, &span)
    }

    // -- Feature: find-references (stubbed: gaps::references) -----------------

    pub fn references(&self, path: &str, pos: Position) -> Vec<Location> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Vec::new();
        };
        let Some(src) = proj.texts.get(path) else {
            return Vec::new();
        };
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let mut idx_cache: HashMap<String, LineIndex> = HashMap::new();
        self.all_reference_spans(proj, path, offset)
            .into_iter()
            .filter_map(|s| self.location_from_span_indexed(proj, &s.span, &mut idx_cache))
            .collect()
    }

    /// Occurrences of the symbol under the cursor WITHIN `path` (the LSP
    /// document-highlight feature). Reuses the symbol-aware `references` query
    /// and keeps only the current file's sites, so a local/param/field/callee
    /// highlights every use in this buffer. Kind is left unset (Text) -- bone's
    /// reference spans don't distinguish read vs write access.
    pub fn document_highlight(&self, path: &str, pos: Position) -> Vec<DocumentHighlight> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let want = format!("file://{path}");
        self.references(path, pos)
            .into_iter()
            .filter(|loc| loc.uri.as_str() == want)
            .map(|loc| DocumentHighlight { range: loc.range, kind: None })
            .collect()
    }

    /// Plain-text typed-hole report (kind + inferred type + in-scope bindings)
    /// for the `?` hole under the cursor, or None if the cursor isn't on a hole.
    /// Drives the `rock-lsp hole-info` CLI on bone. Type holes carry their solved
    /// type; expression holes don't yet (bone rejects them at lowering -- see the
    /// LSP roadmap), but still list the bindings in scope.
    pub fn hole_info(&self, path: &str, pos: Position) -> Option<String> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;
        let uf = proj.session.files.get(path)?;
        let ast = reparse_user_file(uf, path)?;
        let hi = crate::query::hole_info(ws, mid, inf, &ast, offset)?;
        Some(format_hole_report(&hi))
    }

    /// A "fill in missing match arms" edit when the cursor is inside a `match`
    /// over an enum that's missing variants (the classic `match x {}` action).
    /// Returns the text edit (a replacement of the whitespace before the closing
    /// `}` with the generated `EnumName::Variant => {}` arms) and the count, or
    /// None when there's nothing to fill. See `query::match_arms_fill`.
    pub fn fill_match_arms(&self, path: &str, pos: Position) -> Option<MatchArmsFill> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;
        let (span, arms) = crate::query::match_arms_fill(ws, mid, inf, offset)?;

        let (start, end) = span.offset_range;
        // Closing brace of the match body = last `}` within its span.
        let close = src.get(..end)?.rfind('}')?;
        // Indent of the line the `match` sits on; arms get one level deeper.
        let line_start = src.get(..start)?.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let match_indent: String = src[line_start..start]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let arm_indent = format!("{match_indent}    ");
        // Replace the whitespace gap before `}` -- works whether the body was
        // empty (`{}`) or already had arms (insert after the last one).
        let replace_start = src.get(..close)?.trim_end().len();

        let mut new_text = String::new();
        for a in &arms {
            new_text.push('\n');
            new_text.push_str(&arm_indent);
            new_text.push_str(a);
        }
        new_text.push('\n');
        new_text.push_str(&match_indent);

        Some(MatchArmsFill {
            range: Range {
                start: offset_to_position(src, replace_start),
                end: offset_to_position(src, close),
            },
            new_text,
            missing: arms.len(),
        })
    }

    /// A "fill struct fields" edit when the cursor is inside a struct literal
    /// `Name { ... }` missing required fields. Inserts `field: ?` (a Rock
    /// expression hole) for each missing field, keeping the literal's single- or
    /// multi-line shape, or None when nothing's missing. See
    /// `query::struct_fields_fill`.
    pub fn fill_struct_fields(&self, path: &str, pos: Position) -> Option<StructFieldsFill> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;
        let (span, missing) = crate::query::struct_fields_fill(ws, mid, inf, offset)?;

        let (start, end) = span.offset_range;
        let open = src.get(start..end)?.find('{')? + start;
        let close = src.get(..end)?.rfind('}')?;
        let multiline = src.get(open + 1..close)?.contains('\n');

        // End of the last meaningful char before `}`; the gap from here to `}`
        // is whitespace we replace, so the edit works for `{}`, `{ a: 1 }`, and
        // `{ a: 1, }` alike.
        let content_end = src.get(..close)?.trim_end().len();
        let last = src.get(..content_end)?.chars().next_back();
        let needs_comma = !matches!(last, Some('{') | Some(','));

        let fields: Vec<String> = missing.iter().map(|n| format!("{n}: ?")).collect();

        let new_text = if multiline {
            // Indent inserted fields like the existing ones (or the brace line +
            // 4 spaces for an empty literal). Each gets a trailing comma; the
            // previous field gets one too if it lacked it.
            let field_indent = if matches!(last, Some('{')) {
                format!("{}    ", line_indent_at(src, open))
            } else {
                line_indent_at(src, content_end)
            };
            let struct_indent = line_indent_at(src, open);
            let mut t = String::new();
            if needs_comma {
                t.push(',');
            }
            for f in &fields {
                t.push('\n');
                t.push_str(&field_indent);
                t.push_str(f);
                t.push(',');
            }
            t.push('\n');
            t.push_str(&struct_indent);
            t
        } else {
            let lead = if needs_comma { ", " } else { " " };
            format!("{lead}{} ", fields.join(", "))
        };

        Some(StructFieldsFill {
            range: Range {
                start: offset_to_position(src, content_end),
                end: offset_to_position(src, close),
            },
            new_text,
            missing: missing.len(),
        })
    }

    /// A "Generate function" edit when the cursor is on a call to an undeclared
    /// function: inserts a stub `fn name(params) [-> ret] { ? }` after the
    /// enclosing function. Param types come from the argument types (`any` when
    /// unknown), the return type from a concrete use site (else Unit), and the
    /// body is a `?` hole. See `query::generate_function`.
    pub fn generate_function(&self, path: &str, pos: Position) -> Option<GenerateFunctionEdit> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let inf = ws.inference(mid)?;
        let gf = crate::query::generate_function(ws, mid, inf, offset)?;

        let params = gf
            .params
            .iter()
            .map(|(n, t)| format!("{n}: {t}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ret = gf.ret.map(|r| format!(" -> {r}")).unwrap_or_default();
        let new_text = format!("\n\nfn {}({}){} {{\n    ?\n}}", gf.name, params, ret);

        let at = offset_to_position(src, gf.insert_at.min(src.len()));
        Some(GenerateFunctionEdit {
            range: Range { start: at, end: at },
            new_text,
            fn_name: gf.name,
        })
    }

    /// Every reference site for the symbol at `offset`. Macro parameters resolve
    /// against the raw (pre-expansion) AST -- they're erased from bone's HIR --
    /// then everything else through the bone workspace.
    fn all_reference_spans(
        &self,
        proj: &BoneProject,
        path: &str,
        offset: usize,
    ) -> Vec<crate::query::RefSite> {
        if let Some(uf) = proj.session.files.get(path) {
            if let Some(ast) = reparse_user_file(uf, path) {
                let macro_refs = macro_refs::macro_param_references(&ast, offset);
                if !macro_refs.is_empty() {
                    return macro_refs
                        .into_iter()
                        .map(|span| crate::query::RefSite {
                            span,
                            shorthand: None,
                        })
                        .collect();
                }
            }
        }
        let ws = proj.session.workspace();
        let Some(mid) = user_module_for_path(ws, path) else {
            return Vec::new();
        };
        let Some(inf) = ws.inference(mid) else {
            return Vec::new();
        };
        gaps::references(ws, mid, inf, offset)
    }

    // -- Feature: completion (stubbed: crate::query::completion) ---------------------

    pub fn completion(&mut self, path: &str, pos: Position) -> Vec<CompletionItem> {
        // `&mut self`: the context-triggered paths (member/enum/struct/field-ref)
        // analyze the completion munge as a speculative Session variant
        // (`with_file_variant`), which mutates and restores the live session.
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.projects.get_mut(&Self::root_for(path)) else {
            return Vec::new();
        };
        let Some(src) = proj.texts.get(path) else {
            return Vec::new();
        };
        let src = src.as_str();
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let prefix_raw = ident_prefix(src, offset).to_string();
        let prefix = prefix_raw.to_lowercase();
        // Every early-return path below reports its total under BONE_LSP_PROFILE;
        // the fall-through path has its own phase breakdown further down.
        let prof = lsp_profile_enabled();
        let t_total = std::time::Instant::now();
        let path_ms = |label: &str, n: usize| {
            if prof {
                eprintln!(
                    "[bone completion] {label} {:.2}ms ({n} items, prefix {prefix_raw:?})",
                    t_total.elapsed().as_secs_f64() * 1000.0
                );
            }
        };
        // A `(` directly after the cursor means the call parens already exist
        // (e.g. `cw` over the callee in `world_to_screen(x)`): a `name($0)`
        // snippet would double them (`world_to_design()(x)`). `strip_call_template`
        // reuses the existing parens for every fn/method/tuple-variant item.
        let next_is_open_paren = src.get(offset..).is_some_and(|s| s.starts_with('('));

        // Member / enum-qualified completion is detected from source text and
        // analyzed on a munged copy of the file -- it must run BEFORE the
        // live-module guard, since the trigger `.`/`::` can drop the whole module
        // from the live session (e.g. an incomplete `for i in x.` leaves no user
        // module). The munge is applied as a speculative Session variant, so the
        // whole project (warm stdlib + dependencies like the engine) resolves a
        // cross-file receiver type at incremental cost -- plain prefix completion
        // (the fall-through below) uses the live workspace directly.
        // AST cursor classifier (docs/cursor-context-plan.md): parses the
        // munged buffer once and answers the context queries structurally.
        // None only when every ladder rung fails to tokenize -- the byte
        // scanners stay as the documented fallback on that frontier.
        let cursor_ast = crate::cursor_context::CursorAst::at(src, offset);
        // Call-arg slot, shared by the member / field-ref / ranking paths.
        // The receiver anchor is the receiver expr's END offset (the byte
        // fallback's dot offset serves the same "just past the receiver"
        // role in method_receiver_ty).
        let arg_ctx = cursor_ast.as_ref().and_then(|cx| cx.call_arg());
        // Member access (`.`) is the most specific trigger: a `recv.field` even
        // inside a struct-literal field value (`Foo { x: recv.<cursor> }`) is
        // member completion, not field completion -- so it's checked first.
        let member_segments = cursor_ast.as_ref().and_then(|cx| cx.member_path());
        if let Some(segments) = member_segments {
            // In a call-arg position, a member completion (`recv.field`) also
            // surfaces TRANSITIVE suffix paths whose deep type matches the
            // expected param (`recv.movable.grid`), and direct members that fit
            // the slot rank first. The typed session is built INSIDE the member
            // variant (member TyIds live in that workspace); here we just pass
            // the enclosing (callee, arg index).
            let arg_slot = arg_ctx.as_ref().map(|(callee, idx, _)| (callee.clone(), *idx as usize));
            // Bool-field soft boost: the chain is the whole `if`/`while`
            // condition. Structural from the classifier; byte fallback on
            // the tokenize frontier.
            let bool_ctx = cursor_ast.as_ref().is_some_and(|cx| cx.bool_condition_chain());
            let mut items = member_completions(
                &mut proj.session, path, src, offset, &segments, &prefix, arg_slot,
                proj.reach.as_ref(), bool_ctx,
            );
            if next_is_open_paren {
                strip_call_template(&mut items);
            }
            path_ms("member path", items.len());
            return items;
        }
        // Field-reference key path: a call-arg slot whose expected param is
        // `FieldOf<Struct, V>` completes to Struct's fields of type V (the
        // typechecker's invariance) -- offered whether or not the leading `.`
        // is typed yet, with a generic `T` resolved from a sibling arg. Analyze
        // a munged variant so the callee + args resolve through an incomplete
        // `.`. Gated so the variant only fires when the slot is plausibly a
        // field ref (live signature says FieldOf, or a `.` is already typed);
        // `field_ref_arg_completions` returns None -> falls through to normal
        // completion (e.g. enum-shorthand `.Variant` in an arg).
        if let Some((callee, arg_idx, _)) = arg_ctx.clone() {
            let lead_dot = leading_dot(src, offset);
            // Skip the variant only when the live signature is KNOWN and clearly
            // not a field ref. If the callee is unknown live (the incomplete arg
            // broke the current file) or the slot is FieldOf, analyze the munge
            // to resolve it -- so normal `foo(arg` stays variant-free but a
            // field-ref slot is caught even through a parse error. The gate reads
            // the param's nominal HEAD (`TypeCtor`), not a rendered substring.
            let definitely_not_field_ref = matches!(
                callee_slot_is_field_of(proj.session.workspace(), &callee, arg_idx as usize),
                Some(false)
            );
            let plausible = lead_dot || !definitely_not_field_ref;
            if plausible {
                for munged in completion_munges(src, offset) {
                    let attempt = analyze_variant(&mut proj.session, path, &munged, |ws| {
                        field_ref_arg_completions(
                            ws, path, src, offset, &callee, arg_idx as usize, &prefix, lead_dot,
                        )
                    });
                    if let Some(Some(items)) = attempt {
                        path_ms("field-ref path", items.len());
                        return items;
                    }
                }
            }
        }
        let enum_rec = cursor_ast.as_ref().and_then(|cx| cx.enum_record());
        if let Some((enum_name, variant)) = enum_rec {
            // Falls through (None) when the head isn't a record variant -- a unit/
            // tuple variant as a block head (`if x == Enum::Variant { .. }`) is NOT
            // a record literal, so the block still gets global completion.
            if let Some(items) = enum_record_field_completions(
                &mut proj.session, path, src, offset, &enum_name, &variant, &prefix,
            ) {
                path_ms("enum-record path", items.len());
                return items;
            }
        }
        let enum_base = cursor_ast.as_ref().and_then(|cx| cx.qualified_base());
        if let Some(enum_name) = enum_base {
            let mut items = enum_qualified_completions(
                &mut proj.session, path, src, offset, &enum_name, &prefix,
            );
            if next_is_open_paren {
                strip_call_template(&mut items);
            }
            path_ms("enum-qualified path", items.len());
            return items;
        }
        let struct_lit = cursor_ast.as_ref().and_then(|cx| cx.struct_literal());
        if let Some((struct_name, brace_open)) = struct_lit {
            // Falls through (None) when the head isn't actually a struct, so a
            // `match X {` / `if Cond {` block still gets global completion.
            if let Some(items) = struct_literal_field_completions(
                &mut proj.session, path, src, offset, &struct_name, brace_open, &prefix,
            ) {
                path_ms("struct-literal path", items.len());
                return items;
            }
        }

        // Decorator position (`@late_dra|`): the candidates are the workspace's
        // `deco` declarations, not the fn/type namespace. Detected from the `@`
        // trigger and served from the live workspace (decorators are declared in
        // the engine, so they're present even if the edited module dropped out).
        if decorator_context(src, offset) {
            let items = decorator_completions(proj.session.workspace(), &prefix);
            path_ms("decorator path", items.len());
            return items;
        }

        // Scope + global namespace needs a workspace with the user module.
        // Prefer the live session; but if the buffer is broken so the strict
        // session dropped the module, reparse it with RECOVERY so scope
        // completion still works mid-edit (backlog #21).
        let live = proj.session.workspace();
        let fallback_ws: Option<Workspace> = if user_module_for_path(live, path).is_none() {
            let t = std::time::Instant::now();
            let ws = proj.session.combined_unexpanded().ok().and_then(|combined| {
                reparse_with_project(&combined, path, &balance_delimiters(src.to_string()))
            });
            if prof {
                eprintln!(
                    "[bone completion] broken-buffer fallback reparse {:.2}ms (recovered: {})",
                    t.elapsed().as_secs_f64() * 1000.0,
                    ws.is_some()
                );
            }
            if ws.is_none() {
                log::warn!(
                    "completion: broken-buffer recovery failed for `{path}`; \
                     scope completion degraded to the live workspace"
                );
            }
            ws
        } else {
            None
        };
        let ws: &Workspace = fallback_ws.as_ref().unwrap_or(live);
        let Some(mid) = user_module_for_path(ws, path) else {
            return Vec::new();
        };
        let Some(inf) = ws.inference(mid) else {
            return Vec::new();
        };
        // Type-vs-expression slot governs enum completion (bare `Enum` in a type
        // slot, qualified `Enum::Variant` values in an expression slot).
        let expr_position = !cursor_ast.as_ref().is_some_and(|cx| cx.is_type_position());
        // After a `yield`/`yield*`, coroutine-returning fns rank first (the
        // `yield* frame(1)` delegation pattern).
        let after_yield =
            expr_position && cursor_ast.as_ref().is_some_and(|cx| cx.after_yield());
        // Param-name + fn-prefix type heuristic (param TYPE slot only): a param
        // named `e`/`entity` suggests the (generic) `Entity` type; the enclosing
        // fn's name prefix picks the payload (`mob_attack_co` -> `Mob` ->
        // `Entity<Mob>`). With no inferable payload the suggestion is the snippet
        // `Entity<$0>` (cursor parked in the generic slot), never bare `Entity`.
        let mut entity_hint = false;
        let mut entity_payload: Option<Ustr> = None;
        if !expr_position {
            if let Some((pn, fn_name)) = cursor_ast.as_ref().and_then(|cx| cx.param_type()) {
                if is_entity_param_name(&pn) {
                    entity_hint = true;
                    entity_payload = fn_prefix_struct(ws, &fn_name);
                }
            }
        }
        // The prefix filter happens inside `crate::query::completion`, before per-candidate
        // detail rendering -- so typing narrows the work, not just the result.
        let t_cands = std::time::Instant::now();
        let cands = crate::query::completion(ws, mid, inf, offset, &prefix, expr_position);
        let cands_ms = t_cands.elapsed().as_secs_f64() * 1000.0;
        let n_cands = cands.len();
        // Type-directed ranking: inside `f(|)` with a known expected param type,
        // a variable of that type ranks first, a function returning it second,
        // then everything else. Encoded as `sort_text` (only when in arg position
        // -- otherwise the default order is kept). The TYPED oracle
        // (`ExpectedArg`: bone's `coerce` on TyIds, so generics / nullable
        // widening / numeric top-level coercion all rank correctly) leads; the
        // rendered-string list stays alongside as the reachability index's
        // keys and as the fallback rung when no typed session can be built
        // (broken body, no cached inference for the cursor body).
        let arg_call = arg_ctx;
        let field_slot = if arg_call.is_none() {
            cursor_ast.as_ref().and_then(|cx| cx.struct_field_value())
        } else {
            None
        };
        let mut expected = arg_call.as_ref().and_then(|(callee, idx, _recv)| {
            // Receiver type for UFCS overload filtering: structural from the
            // classifier (the receiver is an AST node -- a chain receiver's
            // type is its callee's unique return type, no paren rescanning);
            // the byte scanner remains the tokenize-frontier fallback.
            let recv_ty = cursor_ast.as_ref().and_then(|cx| {
                cx.call_receiver().and_then(|a| match a {
                    crate::cursor_context::ReceiverAnchor::ChainCall(name) => {
                        fn_unique_return_ty(ws, &name)
                    }
                    crate::cursor_context::ReceiverAnchor::End(e) => e
                        .checked_sub(1)
                        .and_then(|last| bone_semantic::query::type_at(ws, mid, inf, last))
                        .map(|ta| ta.ty),
                })
            });
            ExpectedArg::for_call(ws, mid, inf, offset, callee, *idx as usize, recv_ty)
        });
        if expected.is_none() {
            // Struct-literal field VALUE slot (`Name { field: <cursor> }`):
            // the expected type is that field's declared type, so an enum-
            // typed field surfaces its variants type-directed. Inside a lambda
            // body assigned to a function-typed field (`is_open: () => x|`),
            // the body returns the field's RETURN type, so type-direct on that.
            expected = field_slot.as_ref().and_then(|(sname, fname, is_lambda)| {
                let slot = if *is_lambda {
                    crate::query::struct_field_fn_return_ty(ws, sname, fname)
                        .or_else(|| crate::query::struct_field_ty(ws, sname, fname))
                } else {
                    crate::query::struct_field_ty(ws, sname, fname)
                }?;
                ExpectedArg::for_slot(ws, mid, inf, offset, slot)
            });
        }
        // Declared renders of the typed session's surviving slots -- the
        // string-keyed reachability index's targets, receiver-FILTERED by
        // construction (only overloads the receiver coerces into contribute, so
        // a Color slot never re-admits float reach paths from a rejected
        // overload). Empty when no typed session built -- then there is no
        // type-directed ranking or reach (kind-based only), never a
        // rendered-string type compare.
        let expected_types: Vec<String> =
            expected.as_ref().map_or_else(Vec::new, |exp| exp.rendered.clone());
        let has_expected = expected.is_some();
        let t_build = std::time::Instant::now();
        // The signature cache (rebuilt only on non-`BodyOnly` edits) supplies fn
        // `detail` for the survivors below. The one-shot CLI path has no cached
        // build, so render a throwaway one from the same workspace.
        let local_sigs;
        let fn_sigs: &HashMap<ustr::Ustr, String> = match proj.fn_sigs.as_ref() {
            Some(c) => c,
            None => {
                local_sigs = build_fn_sigs(ws);
                &local_sigs
            }
        };
        // Rank EVERY candidate (full accuracy), then materialize a CompletionItem
        // only for the post-cap survivors. The rank key is exactly what
        // `sort_text` encodes -- `{type_rank}{9999-score:04}{dist:02}{method}{lc}`
        // -- and `dist` is 0 for all direct candidates, so ordering by the tuple
        // `(type_rank, 9999-score, method, name-ci)` is identical to a sort_text
        // string sort. Ranking allocates NOTHING per candidate: `fuzzy_score`
        // lowercases the name inline and the tiebreak compares names
        // case-insensitively inline (`ci_cmp`). Only the sort_text string, the fn
        // signature `detail`, `insert_text`/`label_details`, and the
        // `CompletionItem` itself are built for the ~200 we keep, not the ~2500 we
        // cap away. `type_rank` is the leading bucket:
        //   - ARG position: 0 = var/param/global/variant of the expected type,
        //     1 = fn returning it, 2 = everything else.
        //   - non-arg: rank by KIND -- 0 = in-scope local/param, 1 = global,
        //     2 = function / struct / bare enum / newtype, 3 = enum-member
        //     variant, so variables beat functions/types beat enum members,
        //     fuzzy-ranking within each bucket. Call-through items sit at bucket
        //     4, below all direct candidates.
        struct RankedCand {
            case_rank: u8,
            type_rank: u8,
            key2: u16,
            method: u8,
            entry: crate::query::CompletionEntry,
        }
        let q = prefix.as_bytes();
        let q_raw = prefix_raw.as_bytes();
        let mut ranked: Vec<RankedCand> = cands
            .into_iter()
            .map(|c| {
                let type_rank = if let Some(exp) = expected.as_mut() {
                    match c.kind {
                        // A local/param/global whose type COERCES into the
                        // expected slot (bone's real relation: generics
                        // instantiate, `int` fits `int?`/`float`/`any`), OR an
                        // enum variant whose enum is the slot's nominal head
                        // (`reject_target(nog)` wanting a TargetError ->
                        // `TargetError::NotGround`; the variant carries its
                        // enum name in `detail`).
                        SymCat::Local | SymCat::Param | SymCat::Global
                            if c.ty.is_some_and(|t| exp.accepts(t)) =>
                        {
                            0
                        }
                        SymCat::Variant
                            if c.detail.as_deref().is_some_and(|e| exp.expects_nominal(e)) =>
                        {
                            0
                        }
                        SymCat::Function if exp.accepts_fn_returning(ws, c.name.as_str()) => 1,
                        _ => 2,
                    }
                } else {
                    // No expected type: rank by KIND. Variables first
                    // (locals/params, then globals), then functions / structs /
                    // bare enums / newtypes, then enum-member VARIANTS last. Enum
                    // members are only top-priority in a typed arg position (the
                    // branch above) that expects their enum. Structs sit in
                    // bucket 2 (NOT below functions): on an EMPTY prefix the cap
                    // keeps the best MAX_COMPLETION_ITEMS, and burying types under
                    // every stdlib fn would truncate them away entirely; within
                    // bucket 2 the `method` tiebreak already orders non-fns
                    // (structs/types) ahead of functions.
                    match c.kind {
                        SymCat::Local | SymCat::Param => 0,
                        // After `yield`/`yield*`, a coroutine fn is the expected
                        // operand -- promote it above plain fns/types.
                        SymCat::Function if after_yield && fn_is_coroutine(ws, c.name.as_str()) => 0,
                        SymCat::Global => 1,
                        SymCat::Variant => 3,
                        _ => 2,
                    }
                };
                let method = u8::from(c.kind == SymCat::Function);
                let name = c.name.as_str();
                // Smart-case leading rank, NON-arg path only: measure against the
                // variant NAME (the user types it; `Enum::` uppercasing would
                // skew it). In arg position the expected-TYPE bucket must stay
                // dominant over case, so leave case neutral (0) there.
                let case_rank = if !has_expected {
                    let case_target =
                        if c.kind == SymCat::Variant { name.rsplit("::").next().unwrap_or(name) } else { name };
                    case_mismatch_rank(case_target.as_bytes(), q_raw)
                } else {
                    0
                };
                let score = if c.kind == SymCat::Variant {
                    // The user types the variant NAME (`nog` -> `NotGround`);
                    // score against it, but keep the qualified score too so a
                    // `tarnog` (Enum + variant) query still ranks.
                    let vname = name.rsplit("::").next().unwrap_or(name);
                    fuzzy_score(vname.as_bytes(), q).max(fuzzy_score(name.as_bytes(), q))
                } else {
                    fuzzy_score(name.as_bytes(), q)
                };
                let key2 = (9999 - score.clamp(0, 9999)) as u16;
                RankedCand { case_rank, type_rank, key2, method, entry: c }
            })
            .collect();
        // We only need the BEST `MAX_COMPLETION_ITEMS`, not a full ordering --
        // the `rank_and_cap` below re-sorts the kept set by sort_text. So select
        // the top-N in O(n) (`select_nth_unstable_by`) rather than sort all in
        // O(n log n); at empty prefix that is ~2500 candidates.
        // Non-arg (no expected type) ranks score-first; arg position keeps the
        // expected-type bucket dominant. Mirrors the two `sort_text_with_score`
        // layouts so the O(n) top-N select agrees with the final string sort.
        let score_leads = !has_expected;
        let rank_cmp = |a: &RankedCand, b: &RankedCand| {
            let primary = if score_leads {
                // MATCH QUALITY leads, then the smart-case tiebreak, then KIND: a
                // strong match on a SCREAMING_CONST is no longer buried below a
                // weaker case-clean one; case only decides near-ties.
                (a.key2, a.case_rank, a.type_rank, a.method)
                    .cmp(&(b.key2, b.case_rank, b.type_rank, b.method))
            } else {
                // Expected-type bucket leads (case neutral in arg position).
                (a.case_rank, a.type_rank, a.key2, a.method)
                    .cmp(&(b.case_rank, b.type_rank, b.key2, b.method))
            };
            primary
                // Shorter-first tiebreak, matching the `{len:03}` in sort_text --
                // only when there's a real match (key2 < 9999 == score > 0); at
                // empty prefix (key2 == 9999) keep the alphabetical order.
                .then_with(|| {
                    if a.key2 < 9999 {
                        a.entry.name.as_str().len().cmp(&b.entry.name.as_str().len())
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| ci_cmp(a.entry.name.as_str().as_bytes(), b.entry.name.as_str().as_bytes()))
        };
        if ranked.len() > MAX_COMPLETION_ITEMS {
            ranked.select_nth_unstable_by(MAX_COMPLETION_ITEMS, rank_cmp);
            ranked.truncate(MAX_COMPLETION_ITEMS);
        }
        let mut out: Vec<CompletionItem> = ranked
            .into_iter()
            .map(|r| {
                let RankedCand { case_rank, type_rank, key2, method, entry: c } = r;
                // Reconstruct the exact sort_text the old per-candidate path
                // produced: `9999 - key2` recovers the clamped score.
                let sort_text = Some(sort_text_with_score(
                    case_rank,
                    type_rank,
                    c.name.as_str(),
                    9999 - key2 as i32,
                    0,
                    method,
                    score_leads,
                ));
                // Function `detail` is the cached signature, materialized HERE for
                // survivors only; non-fn kinds carry their (cheap) detail already.
                let detail = match c.kind {
                    SymCat::Function => fn_sigs.get(&c.name).cloned(),
                    _ => c.detail,
                };
                // The full detail goes in the panel; a concise form renders inline
                // next to the label in the popup (a fn's `(params) -> ret`, a
                // binding's `: type`).
                let label_details = fn_or_type_label_details(c.kind, detail.as_deref());
                // A function with params accepts as a snippet `name($0)` ->
                // inserts `name()` with the cursor parked between the parens (the
                // client shows a snippet marker, e.g. nvim-cmp's `~`). A param-less
                // function inserts `name()` as plain text so the cursor lands AFTER
                // the `)` -- there's nothing to fill.
                // A qualified `Enum::Variant` value in an expression slot carries
                // the same payload template as the `Enum::` path: tuple -> `($0)`,
                // record -> `{ $0 }`, unit -> bare.
                let (insert_text, insert_text_format) = match c.kind {
                    // `on_killed`/`on_resume`/`on_hot_reload` are contextual
                    // lifecycle-hook keywords with a statement BLOCK form
                    // (`on_killed { .. }`) -- complete to the block, not a call.
                    SymCat::Function if is_lifecycle_hook(c.name.as_str()) => {
                        (Some(format!("{} {{ $0 }}", c.name)), Some(InsertTextFormat::SNIPPET))
                    }
                    SymCat::Function if c.takes_params => {
                        (Some(format!("{}($0)", c.name)), Some(InsertTextFormat::SNIPPET))
                    }
                    SymCat::Function => (Some(format!("{}()", c.name)), None),
                    SymCat::Variant => match c.variant_shape {
                        Some(crate::query::VariantShape::Tuple) => {
                            (Some(format!("{}($0)", c.name)), Some(InsertTextFormat::SNIPPET))
                        }
                        Some(crate::query::VariantShape::Record) => {
                            (Some(format!("{} {{ $0 }}", c.name)), Some(InsertTextFormat::SNIPPET))
                        }
                        _ => (None, None),
                    },
                    _ => (None, None),
                };
                CompletionItem {
                    label: c.name.to_string(),
                    kind: Some(completion_kind(c.kind)),
                    label_details,
                    detail,
                    insert_text,
                    insert_text_format,
                    sort_text,
                    ..Default::default()
                }
            })
            .collect();
        // Synthesized generic `Entity<..>` from the param-name + fn-prefix
        // heuristic. Pushed directly (not a workspace symbol) with the minimal
        // sort_text so it ranks first among the type candidates. With an inferred
        // payload it's the concrete `Entity<Mob>` (plain insert); without one it's
        // the `Entity<$0>` snippet so the cursor lands in the generic slot.
        if entity_hint {
            let (label, insert, fmt) = match entity_payload {
                Some(p) => (format!("Entity<{p}>"), format!("Entity<{p}>"), None),
                None => ("Entity<>".to_string(), "Entity<$0>".to_string(), Some(InsertTextFormat::SNIPPET)),
            };
            if clean_match(label.as_bytes(), prefix.as_bytes()) {
                out.push(CompletionItem {
                    label: label.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    insert_text: Some(insert),
                    insert_text_format: fmt,
                    sort_text: Some(sort_text_with_score(0, 0, &label, 9999, 0, 0, score_leads)),
                    ..Default::default()
                });
            }
        }
        // Named-arg completions: at a DIRECT call arg slot, offer the callee's
        // still-unsupplied params as `name:` items. Additive to the value-
        // expression candidates and ranked in the top (type_rank 0) bucket, so
        // `greet(gr|)` surfaces `greeting:` above the identifier fallback while
        // `grid` etc. stay. Skips params already passed positionally or by name.
        if let Some(nargs) = cursor_ast.as_ref().and_then(|cx| cx.named_arg_slot()) {
            for (i, (pname, pty)) in callee_param_names(ws, &nargs.callee).into_iter().enumerate() {
                if i < nargs.positional || nargs.named.iter().any(|n| n.as_str() == pname.as_str()) {
                    continue;
                }
                if !prefix.is_empty() && !clean_match(pname.as_str().as_bytes(), prefix.as_bytes()) {
                    continue;
                }
                let score = fuzzy_score(pname.as_str().as_bytes(), prefix.as_bytes());
                out.push(CompletionItem {
                    label: format!("{pname}:"),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(pty),
                    insert_text: Some(format!("{pname}: ")),
                    sort_text: Some(sort_text_with_score(0, 0, pname.as_str(), score, 0, 0, score_leads)),
                    ..Default::default()
                });
            }
        }
        // Coroutine ergonomics: `CoFrame` is the dominant Stone yield type
        // (~75% of `Coroutine<..>` annotations), so surface it. All gated on a
        // `CoFrame` struct existing (stone projects); otherwise the generic
        // `Coroutine<$0>` snippet stands in.
        let coframe = struct_exists(ws, "CoFrame");
        let coroutine_slot =
            cursor_ast.as_ref().is_some_and(|cx| cx.coroutine_first_generic_arg());
        if coroutine_slot {
            // (a) first generic arg of `Coroutine<..>` -> `CoFrame` ranks #0.
            if coframe && clean_match(b"CoFrame", prefix.as_bytes()) {
                out.retain(|it| it.label != "CoFrame");
                out.push(CompletionItem {
                    label: "CoFrame".to_string(),
                    kind: Some(CompletionItemKind::STRUCT),
                    sort_text: Some(sort_text_with_score(0, 0, "CoFrame", 9999, 0, 0, score_leads)),
                    ..Default::default()
                });
            }
        } else if !expr_position && !prefix.is_empty() && clean_match(b"Coroutine", prefix.as_bytes()) {
            // (b) completing the `Coroutine` type name -> snippet forms replace
            // the bare type: `Coroutine<CoFrame>` first, `Coroutine<$0>` second.
            out.retain(|it| it.label != "Coroutine");
            if coframe {
                out.push(CompletionItem {
                    label: "Coroutine<CoFrame>".to_string(),
                    kind: Some(CompletionItemKind::STRUCT),
                    insert_text: Some("Coroutine<CoFrame>".to_string()),
                    sort_text: Some(sort_text_with_score(0, 0, "Coroutine<CoFrame>", 9999, 0, 0, score_leads)),
                    ..Default::default()
                });
            }
            out.push(CompletionItem {
                label: "Coroutine<>".to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                insert_text: Some("Coroutine<$0>".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(sort_text_with_score(0, 0, "Coroutine<>", 9998, 0, 0, score_leads)),
                ..Default::default()
            });
        }
        // (c) `co fn NAME()|` with no return type -> offer the `-> Coroutine<..>`
        // return type to the right of `)`.
        if prefix.is_empty() && co_fn_return_slot(src.as_bytes(), offset) {
            let (label, insert, fmt) = if coframe {
                ("-> Coroutine<CoFrame>", "-> Coroutine<CoFrame>", None)
            } else {
                ("-> Coroutine<>", "-> Coroutine<$0>", Some(InsertTextFormat::SNIPPET))
            };
            out.push(CompletionItem {
                label: label.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                insert_text: Some(insert.to_string()),
                insert_text_format: fmt,
                sort_text: Some(sort_text_with_score(0, 0, label, 9999, 0, 0, score_leads)),
                ..Default::default()
            });
        }
        let rank_ms = t_build.elapsed().as_secs_f64() * 1000.0;
        // Reachability-path candidates: in arg position with a known expected
        // type, every in-scope root from which a value of that type is reachable
        // -- through struct fields, UFCS method returns, or a MIX
        // (`player.movable.to_ivec()`) -- is offered as a dotted path. The
        // reachability index is precomputed (rebuilt only on Interface-tier
        // edits), so this is O(in-scope roots) guided lookups against a prebuilt
        // table, not the old per-root O(functions) `ufcs_methods` scan. Ranked
        // with the type-matching variables (rank 0), shortest paths first.
        let t_reach = std::time::Instant::now();
        if !expected_types.is_empty() {
            let reach_owned;
            let reach = match &proj.reach {
                Some(r) => r,
                None => {
                    reach_owned = crate::reachability::ReachabilityIndex::build(ws);
                    &reach_owned
                }
            };
            let pfx = prefix.as_bytes();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            // Roots: in-scope locals/params PLUS every global (Rock has one global
            // namespace across files, so `player.movable.grid` works where
            // `player` is a global from another file -- `locals_in_scope` omits
            // globals).
            let mut roots: Vec<(String, String)> = crate::query::locals_in_scope(ws, mid, inf, offset)
                .into_iter()
                .map(|(n, _k, ty, _ty_id)| (n.to_string(), ty))
                .collect();
            for m in ws.modules.iter() {
                for (gname, gidx) in m.global_by_name.iter() {
                    roots.push((gname.to_string(), ws.render(None, m.globals[*gidx].ty)));
                }
            }
            // UX cap on paths surfaced per root, ranked by distance -- principled
            // "k shortest ways", not a depth limit. `truncated` is logged below.
            const REACH_CAP: usize = 64;
            for (vname, vty) in roots {
                let res = reach.paths_to(&vty, &expected_types, REACH_CAP);
                if res.truncated && lsp_profile_enabled() {
                    eprintln!("[bone completion] reach paths from `{vname}: {vty}` clipped at {REACH_CAP}");
                }
                for p in res.paths {
                    // Dedup / fuzzy-match / sort on the bare access form; the
                    // final hop's parens/snippet are rendered from the flags.
                    let key = format!("{vname}.{}", p.suffix);
                    // Require a CLEAN match (word-boundary / run), not a
                    // coincidental scatter, so `nog` doesn't drag in
                    // `necro_cast.ima(g)e_dims()...` reach paths.
                    if !pfx.is_empty() && !clean_match(key.as_bytes(), pfx) {
                        continue;
                    }
                    if !seen.insert(key.clone()) {
                        continue;
                    }
                    let (label, kind, insert_text, insert_text_format) = if p.is_method {
                        let call = format!("{key}()");
                        let (insert, fmt) = if p.takes_args {
                            (format!("{key}($0)"), Some(InsertTextFormat::SNIPPET))
                        } else {
                            (format!("{key}()"), None)
                        };
                        (format!("{call} -> {}", p.final_ty), CompletionItemKind::METHOD, insert, fmt)
                    } else {
                        (format!("{key}: {}", p.final_ty), CompletionItemKind::FIELD, key.clone(), None)
                    };
                    // A reachability path IS a value of the expected type
                    // (type_rank 0); the shared key folds in match quality
                    // (prefix on the root beats a scattered subsequence), the
                    // hop distance, and field-before-method.
                    let sort_text = arg_sort_text(0, &key, pfx, p.dist, u8::from(p.is_method));
                    out.push(CompletionItem {
                        label,
                        kind: Some(kind),
                        detail: Some(p.final_ty),
                        insert_text: Some(insert_text),
                        insert_text_format,
                        sort_text: Some(sort_text),
                        ..Default::default()
                    });
                }
            }
        }
        let reach_ms = t_reach.elapsed().as_secs_f64() * 1000.0;
        // Whole-call-through-first-param candidates (`rejtanog` ->
        // `reject_target(TargetError::NotGround)`). Expression position only
        // (a value slot), additive to the normal candidates.
        let t_ct = std::time::Instant::now();
        let mut ct_counts = CtCounts::default();
        // Skip call-through entirely when the general (+ reach) candidates already
        // fill the cap: call-through items are bucket 4 (sort_text "4..."), so
        // they sort AFTER every general (bucket 0-3) / reach (bucket 0) item, and
        // the final `rank_and_cap` keeps only the best `MAX_COMPLETION_ITEMS`.
        // If `out` already holds that many bucket-<4 items, every call-through
        // item would be truncated -- so computing them is pure waste. This is
        // EXACT (the output is byte-identical) and removes the worst keystrokes,
        // which are exactly the high-general-count short prefixes (e.g. `sp` ->
        // 229 general candidates -> all call-through truncated anyway).
        if expr_position && out.len() < MAX_COMPLETION_ITEMS {
            // reach (for local-arg expansion) + the precomputed call-through
            // index, both cached on the project and rebuilt only on non-BodyOnly
            // edits. The one-shot CLI path has neither cached -> build throwaways.
            let reach_owned;
            let reach = match proj.reach.as_ref() {
                Some(r) => r,
                None => {
                    reach_owned = crate::reachability::ReachabilityIndex::build(ws);
                    &reach_owned
                }
            };
            let ct_owned;
            let ct = match proj.call_through.as_ref() {
                Some(c) => c,
                None => {
                    ct_owned = build_call_through(ws, reach);
                    &ct_owned
                }
            };
            let (ct_items, c) =
                call_through_param_completions(ws, mid, inf, offset, &prefix, ct, reach, score_leads);
            ct_counts = c;
            out.extend(ct_items);
        }
        let call_through_ms = t_ct.elapsed().as_secs_f64() * 1000.0;
        LAST_COMPLETION_PHASES.with(|c| {
            c.set(CompletionPhaseTimings {
                candidates_ms: cands_ms,
                rank_ms,
                reach_ms,
                call_through_ms,
                n_general: n_cands,
                n_out: out.len(),
                ct_fns: ct_counts.fns,
                ct_arg_iters: ct_counts.arg_iters,
                ct_survivors: ct_counts.survivors,
            })
        });
        if lsp_profile_enabled() {
            eprintln!(
                "[bone completion] candidates {cands_ms:.2}ms ({n_cands} cands)  rank {rank_ms:.2}  reach {reach_ms:.2}  call-through {call_through_ms:.2}  ({} items, prefix {:?})",
                out.len(),
                prefix
            );
        }
        // The list is served `isIncomplete: true`, so cap it: the client
        // re-queries per keystroke and the server re-scores, so returning the
        // whole workspace (2539 items at empty prefix) is pure wasted
        // serialization -- the dominant end-to-end completion cost.
        rank_and_cap(&mut out, MAX_COMPLETION_ITEMS, "completion");
        if next_is_open_paren {
            strip_call_template(&mut out);
        }
        out
    }

    // -- Feature: rename + prepare-rename (stubbed: gaps::references) --------

    pub fn prepare_rename(&self, path: &str, pos: Position) -> Option<Range> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        // Reuse the exact reference resolution `rename` uses, so prepare-rename
        // and rename can never disagree: the token is renameable iff it has
        // reference sites, and the editable range is the cursor's own site (the
        // identifier token). `gaps::definition` + `type_at` did neither -- None
        // at a declaration name ("nothing to rename" on `fn foo`), and the whole
        // enclosing `Call` span at a callee (prefilling `foo(args)`, not `foo`).
        let here = self
            .all_reference_spans(proj, path, offset)
            .into_iter()
            .map(|s| s.span)
            .find(|s| {
                s.file.as_str() == path
                    && s.offset_range.0 <= offset
                    && offset < s.offset_range.1
            })?;
        Some(range_from_offsets(src, here.offset_range.0, here.offset_range.1))
    }

    pub fn rename(&self, path: &str, pos: Position, new_name: &str) -> Option<WorkspaceEdit> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let sites = self.all_reference_spans(proj, path, offset);
        if sites.is_empty() {
            return None;
        }

        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        let mut idx_cache: HashMap<String, LineIndex> = HashMap::new();
        for site in sites {
            let Some(loc) = self.location_from_span_indexed(proj, &site.span, &mut idx_cache)
            else {
                continue;
            };
            // A shorthand token (`S { x }`) names both the field and a
            // binding; renaming one side expands the shorthand, keeping the
            // other side's (old) name.
            let new_text = match site.shorthand {
                None => new_name.to_string(),
                Some(crate::query::Shorthand::KeepField(field)) => {
                    format!("{field}: {new_name}")
                }
                Some(crate::query::Shorthand::KeepBinding(binding)) => {
                    format!("{new_name}: {binding}")
                }
            };
            changes.entry(loc.uri).or_default().push(TextEdit {
                range: loc.range,
                new_text,
            });
        }
        Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        })
    }

    // -- Feature: document symbols (stubbed: crate::query::document_symbols) ---------

    pub fn document_symbols(&self, path: &str) -> Vec<DocumentSymbol> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Vec::new();
        };
        let Some(src) = proj.texts.get(path) else {
            return Vec::new();
        };
        let ws = proj.session.workspace();
        let Some(mid) = user_module_for_path(ws, path) else {
            return Vec::new();
        };
        // One LineIndex for the file: a per-symbol `range_from_offsets` would do
        // an O(offset) scan from the file start for each symbol's two ends.
        let line_index = LineIndex::new(src);
        crate::query::document_symbols(ws, mid)
            .into_iter()
            .map(|s| {
                let range = line_index.range(src, s.span.offset_range.0, s.span.offset_range.1);
                #[allow(deprecated)]
                DocumentSymbol {
                    name: s.name,
                    detail: None,
                    kind: symbol_kind(s.kind),
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range: range,
                    children: None,
                }
            })
            .collect()
    }

    // -- Feature: workspace symbols -----------------------------------------

    /// All top-level symbols across every loaded project whose name matches
    /// `query` (case-insensitive substring; empty query = everything). Rock has
    /// no modules -- "workspace" is every tracked `.rock` file (stdlib/builtin
    /// modules aren't in `texts`, so they're naturally excluded). Reuses the
    /// per-module `crate::query::document_symbols` walk, one pass per file. Spans every
    /// project the session has loaded (the one-shot CLI loads exactly one; the
    /// interactive server accumulates each opened root).
    pub fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        let needle = query.to_lowercase();
        let mut out = Vec::new();
        for proj in self.projects.values() {
            let ws = proj.session.workspace();
            for (file, src) in proj.texts.iter() {
                let Some(mid) = user_module_for_path(ws, file) else {
                    continue;
                };
                let Ok(uri) = <Uri as std::str::FromStr>::from_str(&format!("file://{file}")) else {
                    continue;
                };
                // Built lazily on the first match in this file: a Ctrl-T query
                // skips most files, so don't index a file with no hit. Without it
                // every matched symbol re-scanned from the file start (O(offset)).
                let mut line_index: Option<LineIndex> = None;
                for s in crate::query::document_symbols(ws, mid) {
                    if !needle.is_empty() && !s.name.to_lowercase().contains(&needle) {
                        continue;
                    }
                    let li = line_index.get_or_insert_with(|| LineIndex::new(src));
                    let range = li.range(src, s.span.offset_range.0, s.span.offset_range.1);
                    #[allow(deprecated)]
                    out.push(SymbolInformation {
                        name: s.name,
                        kind: symbol_kind(s.kind),
                        tags: None,
                        deprecated: None,
                        location: Location { uri: uri.clone(), range },
                        container_name: None,
                    });
                }
            }
        }
        out
    }

    // -- Feature: ctags emission (project `.rock.tags`) ----------------------

    /// Regenerate the `.rock.tags` (extended ctags) file for the project that
    /// owns `path`, writing it to `<project-root>/.rock.tags`. Enumerates every
    /// top-level symbol (functions, structs, enums, newtypes, globals) across the
    /// project's tracked files from bone's HIR -- the same `document_symbols`
    /// walk `workspace_symbols` uses -- so the tags match the live program. The
    /// LSP calls this on `textDocument/didSave`; the stdlib is excluded (it isn't
    /// in `texts`), keeping the file project-scoped. A path with no session yet
    /// writes nothing and returns 0. Returns the number of symbols written.
    pub fn write_ctags(&self, path: &str) -> std::io::Result<usize> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let root = Self::root_for(path);
        let Some(proj) = self.projects.get(&root) else {
            return Ok(0);
        };
        let ws = proj.session.workspace();

        let mut entries = Vec::new();
        // Every USER module the session loaded -- the whole compilation unit
        // (project + stick.toml deps), NOT just the files open in the editor.
        // Iterating the workspace modules directly (vs `texts.keys()` +
        // `user_module_for_path`) can't silently drop a file on a path mismatch
        // and needs no canonicalization round-trip. Excludes the builtin module
        // and the warm stdlib (`warm_mids`).
        for m in ws.modules.iter() {
            if m.id == ModuleId::builtin() || proj.session.warm_mids.contains(&m.id) {
                continue;
            }
            let mid = m.id;
            let file = m.file.as_str().to_string();
            // Function `signature:`/`returns:` fields, looked up per function by
            // declaration-span start. Built once per file, not on the hot
            // `document_symbols` path.
            let sigs = crate::query::fn_ctags_signatures(ws, mid);
            // Global declared types, keyed the same way, for the `type:` field.
            let global_tys: std::collections::HashMap<usize, String> = m
                .globals
                .iter()
                .map(|(_, g)| (g.span.offset_range.0, ws.render(None, g.ty)))
                .collect();
            for s in crate::query::document_symbols(ws, mid) {
                let Some(kind) = ctags_kind(s.kind) else {
                    continue;
                };
                let mut fields = Vec::new();
                match s.kind {
                    SymCat::Function => {
                        if let Some((sig, ret)) = sigs.get(&s.span.offset_range.0) {
                            fields.push(("signature".to_string(), sig.clone()));
                            if let Some(ret) = ret {
                                fields.push(("returns".to_string(), ret.clone()));
                            }
                        }
                    }
                    SymCat::Global => {
                        if let Some(ty) = global_tys.get(&s.span.offset_range.0) {
                            fields.push(("type".to_string(), ty.clone()));
                        }
                    }
                    _ => {}
                }
                entries.push(crate::ctags::CtagsEntry {
                    name: s.name,
                    file: file.clone(),
                    // line_range is 0-based; ctags lines are 1-based.
                    line: s.span.line_range.0 + 1,
                    kind,
                    fields,
                });
            }

            // document_symbols only yields top-level symbols; emit struct fields
            // (kind `m`) and enum variants (kind `g`) as scoped members so they
            // are jump-to-able too.
            for (_, st) in m.structs.iter() {
                for f in &st.fields {
                    entries.push(crate::ctags::CtagsEntry {
                        name: f.name.to_string(),
                        file: file.clone(),
                        line: f.span.line_range.0 + 1,
                        kind: 'm',
                        fields: vec![("struct".to_string(), st.name.to_string())],
                    });
                }
            }
            for (_, en) in m.enums.iter() {
                for v in &en.variants {
                    entries.push(crate::ctags::CtagsEntry {
                        name: v.name.to_string(),
                        file: file.clone(),
                        line: v.name_span.line_range.0 + 1,
                        kind: 'g',
                        fields: vec![("enum".to_string(), en.name.to_string())],
                    });
                }
            }

            // `deco` definitions (kind `d`), with their parameter signature
            // rendered the same way as functions.
            for (_, dec) in m.decorators.iter() {
                let params: Vec<String> = dec
                    .param_decls
                    .iter()
                    .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
                    .collect();
                entries.push(crate::ctags::CtagsEntry {
                    name: dec.name.to_string(),
                    file: file.clone(),
                    line: dec.span.line_range.0 + 1,
                    kind: 'd',
                    fields: vec![("signature".to_string(), format!("({})", params.join(", ")))],
                });
            }

            // Macros (kind `M`). They are expanded away before bone lowering, so
            // the workspace HIR has no record of them -- but the Session captured
            // each file's macro signatures at parse time (`UserFile.macros`), so
            // read them directly. No reparse; the recorded spans are shifted on
            // incremental edits, so the lines stay accurate.
            if let Some(uf) = proj.session.files.get(&file) {
                for md in &uf.macros {
                    let params: Vec<String> = md
                        .params
                        .iter()
                        .map(|p| {
                            let variadic = if p.is_variadic { "..." } else { "" };
                            format!("{}: {}{}", p.name, variadic, p.ty)
                        })
                        .collect();
                    let mut fields =
                        vec![("signature".to_string(), format!("({})", params.join(", ")))];
                    if let Some(ret) = &md.ret {
                        fields.push(("returns".to_string(), ret.clone()));
                    }
                    entries.push(crate::ctags::CtagsEntry {
                        name: md.name.to_string(),
                        file: file.clone(),
                        line: md.span.line_range.0 + 1,
                        kind: 'M',
                        fields,
                    });
                }
            }
        }

        crate::ctags::write_tags_file(&root.join(".rock.tags"), entries)
    }

    // -- Feature: signature help (stubbed: crate::query::signature_help) -------------

    pub fn signature_help(&self, path: &str, pos: Position) -> Option<SignatureHelp> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        // Inference is optional: tiers 2/3 (by-name overload list, struct ctor)
        // serve a signature even when the file doesn't fully type-check.
        let inf = ws.inference(mid);
        let (sigs, call_span) = crate::query::signature_help(ws, mid, inf, offset)?;
        // Active argument = top-level commas between the call's `(` and the cursor.
        let active = active_param(src, call_span.offset_range.0, offset);
        Some(SignatureHelp {
            signatures: sigs
                .into_iter()
                .map(|sig| SignatureInformation {
                    label: sig.label,
                    documentation: None,
                    parameters: Some(
                        sig.params
                            .into_iter()
                            .map(|p| ParameterInformation {
                                label: ParameterLabel::Simple(p),
                                documentation: None,
                            })
                            .collect(),
                    ),
                    active_parameter: Some(active),
                })
                .collect(),
            active_signature: Some(0),
            active_parameter: Some(active),
        })
    }

    // -- Feature: inlay hints (stubbed: gaps::inlay_hints) ------------------

    pub fn inlay_hints(&self, path: &str, range: Range) -> Vec<InlayHint> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Vec::new();
        };
        let Some(src) = proj.texts.get(path) else {
            return Vec::new();
        };
        let start = offset_at_line_col(src, range.start.line, range.start.character);
        let end = offset_at_line_col(src, range.end.line, range.end.character);
        // Prefer the live session; reparse with RECOVERY if a broken buffer
        // dropped the module, so hints still show during partial edits (#21).
        let live = proj.session.workspace();
        let fallback_ws: Option<Workspace> = if user_module_for_path(live, path).is_none() {
            let ws = proj.session.combined_unexpanded().ok().and_then(|combined| {
                reparse_with_project(&combined, path, &balance_delimiters(src.to_string()))
            });
            if ws.is_none() {
                log::warn!(
                    "inlay: broken-buffer recovery failed for `{path}`; \
                     hints degraded to the live workspace"
                );
            }
            ws
        } else {
            None
        };
        let ws: &Workspace = fallback_ws.as_ref().unwrap_or(live);
        let Some(mid) = user_module_for_path(ws, path) else {
            return Vec::new();
        };
        let Some(inf) = ws.inference(mid) else {
            return Vec::new();
        };
        // One line index for the whole request: an inlay response is many hints,
        // and a per-hint `offset_to_position` rescans from the file start each
        // time (O(hints * filesize)). With the index each is O(log n + col).
        let line_index = LineIndex::new(src);
        gaps::inlay_hints(ws, mid, inf, start, end)
            .into_iter()
            .map(|h| {
                // A leading (parameter-name) hint renders before its span with
                // trailing padding; a type hint renders after with leading padding.
                let (offset, kind, pad_left, pad_right) = if h.leading {
                    (h.span.offset_range.0, InlayHintKind::PARAMETER, None, Some(true))
                } else {
                    (h.span.offset_range.1, InlayHintKind::TYPE, Some(true), None)
                };
                InlayHint {
                    position: line_index.position(src, offset),
                    label: InlayHintLabel::String(h.label),
                    kind: Some(kind),
                    text_edits: None,
                    tooltip: None,
                    padding_left: pad_left,
                    padding_right: pad_right,
                    data: None,
                }
            })
            .collect()
    }

    // -- Shared span -> LSP conversion --------------------------------------

    /// Resolve a span's file to its LSP [`Uri`] and source text: embedded stdlib
    /// sources for a synthetic `<bone-stdlib:NAME>` id, the session buffer for a
    /// tracked file, else a disk read (untracked sibling). `Cow` borrows in the
    /// common (tracked/stdlib) cases and only owns on the disk read.
    fn span_uri_and_text<'a>(
        &self,
        proj: &'a BoneProject,
        span: &Span,
    ) -> Option<(Uri, Cow<'a, str>)> {
        let file = span.file.as_str();
        if let Some(name) = file
            .strip_prefix("<bone-stdlib:")
            .and_then(|s| s.strip_suffix('>'))
        {
            let text = rock_host::stdlib_loader::stdlib_files()
                .iter()
                .find(|f| f.name == name)?
                .contents;
            // Resolve to the stdlib's REAL on-disk path so the editor can open it
            // and the span's offsets (valid against `text`, which IS that file's
            // contents) land on the right line. A bare `file://{name}` is a
            // malformed URI -- `name` becomes the host, the path is empty -- so
            // the editor opens an empty buffer and applies a stdlib-relative line
            // to it (nvim: "Invalid cursor line: out of range").
            let path = rock_host::stdlib_loader::stdlib_dir().join(name);
            let path = path.canonicalize().unwrap_or(path);
            let uri = <Uri as std::str::FromStr>::from_str(&format!("file://{}", path.display())).ok()?;
            return Some((uri, Cow::Borrowed(text)));
        }
        let uri = <Uri as std::str::FromStr>::from_str(&format!("file://{file}")).ok()?;
        match proj.texts.get(file) {
            Some(t) => Some((uri, Cow::Borrowed(t))),
            None => Some((uri, Cow::Owned(fs_err::read_to_string(file).ok()?))),
        }
    }

    /// Convert a bone span to an LSP [`Location`]. Single-span use (definition);
    /// loops over many spans (references/rename) use
    /// [`location_from_span_indexed`] with a per-file [`LineIndex`].
    fn location_from_span(&self, proj: &BoneProject, span: &Span) -> Option<Location> {
        let (uri, text) = self.span_uri_and_text(proj, span)?;
        Some(Location {
            uri,
            range: range_from_offsets(&text, span.offset_range.0, span.offset_range.1),
        })
    }

    /// Like [`location_from_span`] but reuses a per-file [`LineIndex`] across a
    /// batch of spans (references/rename emit many sites, often in one file): a
    /// bare per-span `range_from_offsets` rescans from the file start each time
    /// (O(sites * filesize)).
    fn location_from_span_indexed(
        &self,
        proj: &BoneProject,
        span: &Span,
        cache: &mut HashMap<String, LineIndex>,
    ) -> Option<Location> {
        let (uri, text) = self.span_uri_and_text(proj, span)?;
        let li = cache
            .entry(span.file.as_str().to_string())
            .or_insert_with(|| LineIndex::new(&text));
        Some(Location {
            uri,
            range: li.range(&text, span.offset_range.0, span.offset_range.1),
        })
    }

    /// Lexical semantic tokens for the whole file, delta-encoded per the LSP
    /// spec. Built from the tokenizer (robust on broken/incomplete code, where
    /// the HIR may be partial); identifiers are enriched from the file's module
    /// name indexes (struct/enum/newtype/function), defaulting to `variable`.
    /// Token-type indices match [`semantic_tokens_legend`].
    /// Lexical+semantic tokens for the whole file (`textDocument/semanticTokens/full`).
    pub fn semantic_tokens(&self, path: &str) -> SemanticTokens {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        self.semantic_tokens_impl(path, None)
    }

    /// Tokens for a viewport (`textDocument/semanticTokens/range`). The file is
    /// still tokenized whole (cheap, and multi-line `"""` strings make partial
    /// lexing unsafe), but classification skips bodies outside the range and only
    /// in-range tokens are emitted -- so the O(file) classify + the wire shrink to
    /// the viewport. `range` is byte offsets `[lo, hi)`.
    pub fn semantic_tokens_range(&self, path: &str, range: Range) -> SemanticTokens {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(src) = self.project(path).and_then(|p| p.texts.get(path)) else {
            return SemanticTokens::default();
        };
        let lo = offset_at_line_col(src, range.start.line, range.start.character);
        let hi = offset_at_line_col(src, range.end.line, range.end.character);
        self.semantic_tokens_impl(path, Some((lo, hi)))
    }

    fn semantic_tokens_impl(&self, path: &str, range: Option<(usize, usize)>) -> SemanticTokens {
        let Some(proj) = self.project(path) else {
            return SemanticTokens::default();
        };
        let Some(src) = proj.texts.get(path) else {
            return SemanticTokens::default();
        };
        let prof = lsp_profile_enabled();
        let t = std::time::Instant::now();
        let Ok(toks) = rock_tokenizer::Tokenizer::new(src, Ustr::from(path)).tokenize() else {
            return SemanticTokens::default();
        };
        let tok_ms = t.elapsed().as_secs_f64() * 1000.0;
        let ws = proj.session.workspace();
        let t = std::time::Instant::now();
        let mut names = semantic_name_map(ws);
        // Macros are expanded away before bone lowering, so the workspace HIR
        // has no record of them; the Session's parse-time capture supplies the
        // names (def sites and call sites are both bare ident tokens).
        for uf in proj.session.files.values() {
            for md in &uf.macros {
                names.entry(md.name).or_insert(TT_MACRO);
            }
        }
        let names_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let hir = user_module_for_path(ws, path)
            .map(|mid| semantic_classifications(ws, mid, range))
            .unwrap_or_default();
        let hir_ms = t.elapsed().as_secs_f64() * 1000.0;
        if prof {
            eprintln!(
                "[bone semtok] tokenize {tok_ms:.2}ms ({} toks)  name_map {names_ms:.2}ms ({} names)  classifications {hir_ms:.2}ms  range={range:?}",
                toks.len(),
                names.len()
            );
        }

        let mut data = Vec::new();
        let (mut prev_line, mut prev_start) = (0u32, 0u32);
        let mut prev_kind: Option<&rock_tokenizer::TokenKind> = None;
        let mut prev_end = 0usize;
        for (i, tok) in toks.iter().enumerate() {
            // A `.`/`?.` immediately before an identifier marks it a field/method
            // access (-> property). Tracked over the FULL stream (punctuation is
            // skipped from output but still advances `prev_kind`).
            let is_member = matches!(
                prev_kind,
                Some(rock_tokenizer::TokenKind::Dot) | Some(rock_tokenizer::TokenKind::QuestionDot)
            );
            // A GLUED `@ident` is an annotation (`@guarded`, `@global_write`)
            // when the HIR doesn't claim the ident; the `@` trailing-lambda
            // operator's operand is an expression the HIR does claim, and is
            // conventionally spaced anyway.
            let is_annotation = matches!(prev_kind, Some(rock_tokenizer::TokenKind::At))
                && prev_end == tok.span.offset_range.0;
            prev_kind = Some(&tok.kind);
            prev_end = tok.span.offset_range.1;

            // Next non-comment token — lets `classify_token` recognize the
            // contextual `co` keyword (`co` before `fn`).
            let next_kind = toks[i + 1..]
                .iter()
                .map(|t| &t.kind)
                .find(|k| !matches!(k, rock_tokenizer::TokenKind::Comment(_)));

            // Range request: emit only tokens whose start is inside the viewport.
            if let Some((lo, hi)) = range {
                let off = tok.span.offset_range.0;
                if off < lo || off >= hi {
                    continue;
                }
            }

            // LSP tokens are single-line; skip a token that spans lines (block
            // comment / multi-line string).
            if tok.span.line_range.0 != tok.span.line_range.1 {
                continue;
            }
            let Some((token_type, token_modifiers_bitset)) = classify_token(
                &tok.kind,
                next_kind,
                &hir.by_offset,
                &names,
                tok.span.offset_range.0,
                is_member,
                is_annotation,
            ) else {
                continue;
            };
            let line = tok.span.line_range.0 as u32;
            let start = tok.span.col_range.0 as u32;
            let length = tok.span.col_range.1.saturating_sub(tok.span.col_range.0) as u32;
            if length == 0 {
                continue;
            }
            // An interpolated string splits into literal segments (STRING,
            // incl. the braces) and hole expressions colored as what they are
            // (the hole's root classification from the offset map; the token
            // is single-line here, so char-based `col_range` math is safe).
            let mut pieces: Vec<(u32, u32, u32, u32)> = Vec::new();
            if token_type == TT_STRING {
                let (ts, te) = tok.span.offset_range;
                let mut cur = start;
                for h in hir
                    .interp_holes
                    .iter()
                    .filter(|h| h.offset_range.0 >= ts && h.offset_range.1 <= te)
                {
                    let (hs, he) = (h.col_range.0 as u32, h.col_range.1 as u32);
                    if hs > cur {
                        pieces.push((cur, hs - cur, TT_STRING, 0));
                    }
                    let (tt, mods) = hir
                        .by_offset
                        .get(&h.offset_range.0)
                        .copied()
                        .unwrap_or((TT_VARIABLE, 0));
                    pieces.push((hs, he - hs, tt, mods));
                    cur = he;
                }
                if !pieces.is_empty() && start + length > cur {
                    pieces.push((cur, start + length - cur, TT_STRING, 0));
                }
            }
            if pieces.is_empty() {
                pieces.push((start, length, token_type, token_modifiers_bitset));
            }
            for (col, len, tt, mods) in pieces {
                let delta_line = line.saturating_sub(prev_line);
                let delta_start = if delta_line == 0 {
                    col.saturating_sub(prev_start)
                } else {
                    col
                };
                data.push(SemanticToken {
                    delta_line,
                    delta_start,
                    length: len,
                    token_type: tt,
                    token_modifiers_bitset: mods,
                });
                prev_line = line;
                prev_start = col;
            }
        }
        SemanticTokens {
            result_id: None,
            data,
        }
    }
}

/// Token-type legend: the index of each [`SemanticTokenType`] is the
/// `token_type` value emitted by [`BoneWorkspace::semantic_tokens`]. A server
/// advertises this in its `SemanticTokensLegend`.
pub fn semantic_tokens_legend() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::KEYWORD,  // 0
        SemanticTokenType::VARIABLE, // 1
        SemanticTokenType::FUNCTION, // 2
        SemanticTokenType::STRUCT,   // 3
        SemanticTokenType::ENUM,     // 4
        SemanticTokenType::TYPE,     // 5
        SemanticTokenType::NUMBER,   // 6
        SemanticTokenType::STRING,   // 7
        SemanticTokenType::COMMENT,  // 8
        SemanticTokenType::PROPERTY, // 9
        SemanticTokenType::PARAMETER, // 10
        SemanticTokenType::ENUM_MEMBER, // 11
        SemanticTokenType::TYPE_PARAMETER, // 12
        SemanticTokenType::DECORATOR, // 13
        SemanticTokenType::MACRO, // 14
    ]
}

/// Token-modifier legend: bit `i` of a token's `token_modifiers_bitset`
/// corresponds to `semantic_tokens_modifiers_legend()[i]`. The server remaps
/// these bits to the legend it advertises (rock's), the same way it remaps token
/// types -- so bone stays self-contained over its own legend.
pub fn semantic_tokens_modifiers_legend() -> Vec<SemanticTokenModifier> {
    vec![
        // The binding's inferred type is `T?` and flow has NOT narrowed it here.
        SemanticTokenModifier::new("nullable"), // bit 0
        // A use of a `T?` binding that flow narrowed to `T` (e.g. inside
        // `if x != null { .. }`). Mutually exclusive with `nullable`.
        SemanticTokenModifier::new("refined"), // bit 1
        // A `const` declaration or use. Combines with nullable/refined.
        // Editors style `variable` + `readonly`/`static` to make globals pop
        // against locals (the nvim plugin bolds both
        // `@lsp.typemod.variable.readonly.rock` and `.static.rock`).
        SemanticTokenModifier::READONLY, // bit 2
        // A mutable `global` declaration or use (readonly would be a lie).
        SemanticTokenModifier::STATIC, // bit 3
    ]
}

const MOD_NULLABLE: u32 = 1 << 0;
const MOD_REFINED: u32 = 1 << 1;
const MOD_READONLY: u32 = 1 << 2;
const MOD_STATIC: u32 = 1 << 3;

const TT_KEYWORD: u32 = 0;
const TT_VARIABLE: u32 = 1;
const TT_FUNCTION: u32 = 2;
const TT_STRUCT: u32 = 3;
const TT_ENUM: u32 = 4;
const TT_TYPE: u32 = 5;
const TT_NUMBER: u32 = 6;
const TT_STRING: u32 = 7;
const TT_COMMENT: u32 = 8;
const TT_PROPERTY: u32 = 9;
const TT_PARAMETER: u32 = 10;
const TT_ENUM_MEMBER: u32 = 11;
const TT_TYPE_PARAMETER: u32 = 12;
const TT_DECORATOR: u32 = 13;
const TT_MACRO: u32 = 14;

/// Classify a token into a legend index, or `None` to skip it (operators,
/// punctuation, delimiters -- not highlighted).
fn classify_token(
    kind: &rock_tokenizer::TokenKind,
    next_kind: Option<&rock_tokenizer::TokenKind>,
    hir: &HashMap<usize, (u32, u32)>,
    names: &HashMap<Ustr, u32>,
    offset: usize,
    is_member: bool,
    is_annotation: bool,
) -> Option<(u32, u32)> {
    use rock_tokenizer::TokenKind as T;
    Some(match kind {
        // Contextual `co` keyword — only highlighted when it precedes `fn`
        // (`co fn ...`); a bare `co` identifier falls through to the Ident arm.
        T::Ident(n) if n.as_str() == "co" && matches!(next_kind, Some(T::Fn)) => (TT_KEYWORD, 0),
        T::Fn | T::Operator | T::Table | T::Let | T::Return | T::Defer | T::Yield
        | T::Struct | T::Newtype | T::If | T::Else | T::For | T::Loop | T::In | T::While
        | T::Break | T::Continue | T::Null | T::Enum | T::Match | T::Extern | T::Global
        | T::Const | T::Type | T::As | T::Deco | T::Macro | T::True | T::False
        | T::DirectiveIf | T::DirectiveElseIf | T::DirectiveElse | T::DirectiveEnd => (TT_KEYWORD, 0),
        T::Comment(_) => (TT_COMMENT, 0),
        T::Int(..) | T::Float(_) => (TT_NUMBER, 0),
        T::String(_) | T::RawString(_) => (TT_STRING, 0),
        // Identifiers: the HIR's precise classification (param vs local, field
        // declarations/literal fields, plus the nullable/refined modifier) wins;
        // then the `.`/`?.` field-access heuristic; then the workspace name map
        // (cross-file type / function names); else a bare variable. The lexer
        // stays the lexical layer (keywords/literals/comments) and the fallback
        // on broken code, where the HIR is stale. Only HIR-classified bindings
        // carry modifiers; the fallbacks emit none.
        T::Ident(n) => hir
            .get(&offset)
            .copied()
            // A `.`/`?.` member that names a known FUNCTION is a UFCS method call
            // (`m.resolve_strike()`) -> color it `function`, not `property`. A
            // member that isn't a known fn is a field access -> `property`.
            .or_else(|| {
                is_member.then(|| match names.get(n) {
                    Some(&t) if t == TT_FUNCTION => (TT_FUNCTION, 0),
                    _ => (TT_PROPERTY, 0),
                })
            })
            // A glued `@ident` the HIR didn't claim: statement/item-level
            // annotations (`@guarded`, `@global_write`) that never reach hir.
            .or_else(|| is_annotation.then_some((TT_DECORATOR, 0)))
            .or_else(|| names.get(n).map(|&t| (t, 0)))
            .unwrap_or((TT_VARIABLE, 0)),
        _ => return None,
    })
}

/// Per-offset identifier classifications drawn from the module's HIR -- the
/// authoritative source the tokenizer lacks: parameters vs locals (binding
/// kind), struct field declarations, and struct-literal field names, all as the
/// `property`/`parameter`/`variable` they actually are. Keyed by the span's
/// start offset (the tokenizer and HIR share rock-core spans). When the file is
/// mid-edit-broken the HIR is the last-good state, so edited-line offsets may not
/// line up -- those tokens fall back to the lexer heuristics, no worse than before.
fn semantic_classifications(
    ws: &Workspace,
    mid: ModuleId,
    range: Option<(usize, usize)>,
) -> SemanticClassifications {
    use bone_semantic::hir::{BindingKind, BodyOwner, Expr};
    let binding_tt = |k: BindingKind| match k {
        BindingKind::Param => TT_PARAMETER,
        BindingKind::Let => TT_VARIABLE,
    };
    let mut map: HashMap<usize, (u32, u32)> = HashMap::new();
    let mut holes: Vec<Span> = Vec::new();
    let Some(module) = ws.modules.get(mid.index()) else {
        return SemanticClassifications::default();
    };
    let inf = ws.inference(mid);
    // Struct field declaration names -> property.
    for s in module.structs.values() {
        for f in &s.fields {
            map.insert(f.span.offset_range.0, (TT_PROPERTY, 0));
        }
    }
    // Enum variant declaration names -> enum member.
    for en in module.enums.values() {
        for v in &en.variants {
            map.insert(v.name_span.offset_range.0, (TT_ENUM_MEMBER, 0));
        }
    }
    // Function annotation names (`@test`, `@late_draw`) -> decorator. The
    // hir keeps each annotation's name Ident with its span.
    for f in module.functions.values() {
        for anno in &f.annotations {
            map.insert(anno.name.span.offset_range.0, (TT_DECORATOR, 0));
        }
    }
    // Generic param declarations (`fn id<T>`, `struct Box<T>`): the decl's
    // `TypeExpr::Generic` node keeps its `<T>` name span in ty_spans
    // (span_recordable excludes generics, so uses never clobber it).
    let generic_decls = module
        .functions
        .values()
        .flat_map(|f| &f.generics)
        .chain(module.structs.values().flat_map(|s| &s.generics))
        .chain(module.enums.values().flat_map(|e| &e.generics));
    for g in generic_decls {
        if let bone_semantic::prelude::TyId::Static(gmid, idx) = g {
            if *gmid == mid {
                if let Some(sp) = module.ty_spans.get(*idx) {
                    map.insert(sp.offset_range.0, (TT_TYPE_PARAMETER, 0));
                }
            }
        }
    }
    // Workspace-wide global decls, name -> (declared type, is_const):
    // classifies `Expr::Global` uses (incl. cross-module refs). `const` gets
    // `readonly`, a mutable `global` gets `static`.
    let global_mod = |is_const: bool| if is_const { MOD_READONLY } else { MOD_STATIC };
    let mut global_tys: HashMap<Ustr, (bone_semantic::prelude::TyId, bool)> = HashMap::new();
    for m in ws.modules.iter() {
        for (_, g) in m.globals.iter() {
            global_tys.entry(g.name).or_insert((g.ty, g.is_const));
        }
    }
    // This module's global declaration names. A `T?` global carries `nullable`
    // at its declaration, like a binding decl.
    for (_, g) in module.globals.iter() {
        let mods = global_mod(g.is_const)
            | if ty_is_nullable(ws, None, g.ty) {
                MOD_NULLABLE
            } else {
                0
            };
        map.insert(g.name_span.offset_range.0, (TT_VARIABLE, mods));
    }
    for (owner, body) in module.bodies.iter() {
        // Range request (viewport): walk only function bodies whose footprint
        // overlaps [lo,hi) -- the expensive per-body walk is what scales with
        // file size. Globals/decorators are few, so include them unconditionally.
        if let (Some((lo, hi)), BodyOwner::Function(fid)) = (range, owner) {
            let (s, e) = module.functions[*fid].span.offset_range;
            if e <= lo || s >= hi {
                continue;
            }
        }
        let bi = inf.and_then(|i| i.per_fn.get(owner));
        // Binding declarations: params vs locals. A `T?` binding carries
        // `nullable` at its declaration -- no flow narrowing applies at a decl.
        for (b, info) in body.binding_arena.iter() {
            if let Some(sp) = body.binding_spans.get(b) {
                let mods = match bi.and_then(|bi| bi.binding_types.get(b)) {
                    Some(&ty) if ty_is_nullable(ws, bi, ty) => MOD_NULLABLE,
                    _ => 0,
                };
                map.insert(sp.offset_range.0, (binding_tt(info.kind), mods));
            }
        }
        for (e, expr) in body.expr_arena.iter() {
            match expr {
                // A variable use -> classified by the binding it refers to. A use
                // of a `T?` binding flow narrowed to `T` here (e.g. inside
                // `if x != null { .. }`) carries `refined`; an unrefined use still
                // `T?` carries `nullable`; otherwise neither.
                Expr::Var(b) => {
                    if let Some(sp) = body.expr_spans.get(e) {
                        let tt = binding_tt(body.binding_arena[*b].kind);
                        let decl_nullable = bi
                            .and_then(|bi| bi.binding_types.get(*b))
                            .is_some_and(|&ty| ty_is_nullable(ws, bi, ty));
                        let mods = if decl_nullable {
                            match bi.and_then(|bi| bi.expr_types.get(e)) {
                                Some(&use_ty) if ty_is_nullable(ws, bi, use_ty) => MOD_NULLABLE,
                                Some(_) => MOD_REFINED,
                                None => MOD_NULLABLE,
                            }
                        } else {
                            0
                        };
                        map.insert(sp.offset_range.0, (tt, mods));
                    }
                }
                // Variant names in expression position (`Color::Red`, bare
                // `Red`) -> enum member; the enum qualifier colors via the
                // workspace name map.
                Expr::EnumVariant { variant_span, .. } => {
                    map.insert(variant_span.offset_range.0, (TT_ENUM_MEMBER, 0));
                }
                // A global use -> readonly variable, with the same
                // nullable/refined flow logic as `Var` (bone refines globals
                // purity-aware). Names that aren't declared globals (e.g. a fn
                // used as a value) fall through to the workspace name map.
                Expr::Global(name) => {
                    if let (Some(sp), Some(&(decl_ty, is_const))) =
                        (body.expr_spans.get(e), global_tys.get(name))
                    {
                        let mods = global_mod(is_const)
                            | if ty_is_nullable(ws, bi, decl_ty) {
                                match bi.and_then(|bi| bi.expr_types.get(e)) {
                                    Some(&use_ty) if ty_is_nullable(ws, bi, use_ty) => MOD_NULLABLE,
                                    Some(_) => MOD_REFINED,
                                    None => MOD_NULLABLE,
                                }
                            } else {
                                0
                            };
                        map.insert(sp.offset_range.0, (TT_VARIABLE, mods));
                    }
                }
                // Struct-literal field names -> property.
                Expr::StructLiteral { fields, .. } => {
                    for fi in fields.clone() {
                        let f = &body.struct_lit_field_arena[fi];
                        map.insert(f.name_span.offset_range.0, (TT_PROPERTY, 0));
                    }
                }
                // Interpolation holes: each `{expr}`'s expr span (real in-file
                // offsets from the parser) lets the emitter split the string
                // token and color the hole as the expression it is.
                Expr::InterpolatedString(parts) => {
                    for pi in parts.clone() {
                        if let bone_semantic::hir::InterpPart::Value { expr, .. } =
                            &body.interp_part_arena[pi]
                        {
                            if let Some(sp) = body.expr_spans.get(*expr) {
                                holes.push(*sp);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Generic USES in type positions (`x: T`, `-> T`, `Vec<T>`'s param):
        // every use lowers a lapper interval (with the use's span) pointing at
        // the shared `TypeExpr::Generic` decl node -- the only per-use record,
        // since the resolved TyId is the decl's.
        for iv in &body.node_intervals {
            if let bone_semantic::hir::NodeRef::StaticTy { mid: tmid, idx } = iv.val {
                if matches!(
                    ws.module(tmid).ty_arena[idx],
                    bone_semantic::hir::TypeExpr::Generic(..)
                ) {
                    map.insert(iv.start, (TT_TYPE_PARAMETER, 0));
                }
            }
        }
        // Variant names in match patterns (`Color::Red =>`, record variants
        // `Circle { .. } =>`). The flat arena walk reaches nested sub-patterns.
        for (_, pat) in body.pattern_arena.iter() {
            match pat {
                bone_semantic::hir::Pattern::EnumVariant { variant_span, .. } => {
                    map.insert(variant_span.offset_range.0, (TT_ENUM_MEMBER, 0));
                }
                bone_semantic::hir::Pattern::Record {
                    enum_name: Some(_),
                    name_span: Some(sp),
                    ..
                } => {
                    map.insert(sp.offset_range.0, (TT_ENUM_MEMBER, 0));
                }
                _ => {}
            }
        }
    }
    holes.sort_by_key(|h| h.offset_range.0);
    SemanticClassifications { by_offset: map, interp_holes: holes }
}

/// Output of [`semantic_classifications`]: per-offset identifier
/// classifications plus the interpolation-hole expr spans the emitter uses to
/// split string tokens.
#[derive(Default)]
struct SemanticClassifications {
    by_offset: HashMap<usize, (u32, u32)>,
    /// `{expr}` hole EXPR spans (no braces), sorted by start offset.
    interp_holes: Vec<Span>,
}

/// Whether `ty` resolves to a `T?` nullable, following inference-var
/// substitutions (mirrors `Workspace::render`'s resolution). `body` is the
/// `BodyInference` that owns Local types and substitutions; without it, Local
/// types and unsubstituted inference vars read as non-nullable.
fn ty_is_nullable(
    ws: &Workspace,
    body: Option<&bone_semantic::prelude::BodyInference>,
    ty: bone_semantic::prelude::TyId,
) -> bool {
    use bone_semantic::hir::TypeExpr;
    use bone_semantic::prelude::TyId;
    let te = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => match body {
            Some(b) => b.ty_arena[idx],
            None => return false,
        },
    };
    match te {
        TypeExpr::Nullable(_) => true,
        TypeExpr::Infer(var_id, _) => match body.and_then(|b| b.substitutions.get(var_id)) {
            Some(sub) => ty_is_nullable(ws, body, sub),
            None => false,
        },
        _ => false,
    }
}

/// Workspace-wide identifier categories for semantic tokens: every struct /
/// enum / newtype / function name across ALL modules (incl. dependencies), so a
/// cross-file type name colors as a type, not a bare variable. Built once per
/// request. Structs win over later kinds on a name clash (`entry` keeps first).
fn semantic_name_map(ws: &Workspace) -> HashMap<Ustr, u32> {
    let mut names: HashMap<Ustr, u32> = HashMap::new();
    for m in ws.modules.iter() {
        for n in m.struct_by_name.keys() {
            names.entry(*n).or_insert(TT_STRUCT);
        }
        for n in m.enums_by_name.keys() {
            names.entry(*n).or_insert(TT_ENUM);
        }
        for n in m.newtypes_by_name.keys() {
            names.entry(*n).or_insert(TT_TYPE);
        }
        for n in m.functions_by_name.keys() {
            names.entry(*n).or_insert(TT_FUNCTION);
        }
        for d in m.decorators.values() {
            names.entry(d.name).or_insert(TT_DECORATOR);
        }
    }
    names
}

/// Refine an identifier by its declared kind in the file's module (struct /
/// enum / newtype / function), defaulting to `variable`.

// ---------------------------------------------------------------------------
// Coordinate-unit conversions (the single place these are gotten right)
// ---------------------------------------------------------------------------

/// 0-based LSP `(line, character)` -> 0-based **byte** offset into `src`.
/// `character` counts Unicode scalar values; a multibyte glyph before the cursor
/// still maps to the right byte. Out-of-range clamps to end-of-line / EOF.
pub fn offset_at_line_col(src: &str, line: u32, character: u32) -> usize {
    let line = line as usize;
    let character = character as usize;
    let mut offset = 0usize;
    for (i, line_str) in src.split_inclusive('\n').enumerate() {
        if i == line {
            let mut chars = line_str.char_indices();
            for _ in 0..character {
                if chars.next().is_none() {
                    break;
                }
            }
            let within = chars.next().map(|(b, _)| b).unwrap_or(line_str.len());
            return offset + within;
        }
        offset += line_str.len();
    }
    src.len()
}

/// Byte offset -> 0-based LSP [`Position`] (character counted in scalar values,
/// matching [`offset_at_line_col`]).
pub fn offset_to_position(src: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    let mut cur = 0usize;
    for ch in src.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    Position::new(line, col)
}

/// Byte offsets of each line start in `src`, for O(log n + col) repeated
/// offset->Position conversion. A bare `offset_to_position` is O(offset) (it
/// rescans from the file start every call), so converting many offsets in one
/// request -- e.g. one per inlay hint -- is O(hits * filesize). Build this once
/// and resolve each offset via [`LineIndex::position`] instead.
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(src: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { line_starts }
    }

    /// LSP [`Range`] for a byte span, both ends via [`LineIndex::position`].
    pub fn range(&self, src: &str, start: usize, stop: usize) -> Range {
        Range::new(self.position(src, start), self.position(src, stop))
    }

    /// 0-based byte `offset` -> 0-based LSP `(line, character)`. `character` counts
    /// Unicode scalar values within the line, matching [`offset_to_position`].
    pub fn position(&self, src: &str, offset: usize) -> Position {
        let offset = offset.min(src.len());
        // Last line start <= offset (partition_point gives the count of starts
        // that are <=, so -1 is that line's index; line_starts[0]=0 so >=1).
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let line_start = self.line_starts[line];
        // Column = scalar values from the line start up to (not past) offset.
        let mut col = 0u32;
        let mut cur = line_start;
        for ch in src[line_start..].chars() {
            if cur >= offset {
                break;
            }
            cur += ch.len_utf8();
            col += 1;
        }
        Position::new(line as u32, col)
    }
}

fn range_from_offsets(src: &str, start: usize, stop: usize) -> Range {
    Range::new(offset_to_position(src, start), offset_to_position(src, stop))
}

/// Leading whitespace of the line containing byte `pos`. Used to indent
/// generated code (fill-struct-fields) to match the surrounding source.
fn line_indent_at(src: &str, pos: usize) -> String {
    let pos = pos.min(src.len());
    let line_start = src.get(..pos).and_then(|s| s.rfind('\n')).map_or(0, |i| i + 1);
    src.get(line_start..)
        .unwrap_or("")
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// Active argument index for signature help: count of top-level commas between
/// the call's opening `(` and the cursor. Depth tracks nested parens so a comma
/// inside a nested call doesn't advance the outer arg; a naive string-skip keeps
/// commas inside string literals from counting (no escape handling -- fine for a
/// cursor heuristic). `call_start` is the byte offset of the call expression.
fn active_param(src: &str, call_start: usize, offset: usize) -> u32 {
    let end = offset.min(src.len());
    let start = call_start.min(end);
    let mut depth = 0i32;
    let mut arg = 0u32;
    let mut in_str = false;
    for &b in &src.as_bytes()[start..end] {
        match b {
            b'"' => in_str = !in_str,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => depth -= 1,
            b',' if !in_str && depth == 1 => arg += 1,
            _ => {}
        }
    }
    arg
}

/// Whether any overload of `callee` declares arg `arg_index` as a
/// `FieldOf<..>` slot, decided on the param's nominal head. None when no
/// overload is known live (unknown callee / arity) -- the caller treats that
/// as "can't rule it out".
fn callee_slot_is_field_of(ws: &Workspace, callee: &str, arg_index: usize) -> Option<bool> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let name = Ustr::from(callee);
    let mut any = false;
    let mut found = false;
    for m in ws.modules.iter() {
        let Some(fns) = m.functions_by_name.get(&name) else { continue };
        for (fmid, fid) in fns {
            let f = &ws.modules[fmid.index()].functions[*fid];
            let Some(p) = f.param_decls.get(arg_index) else { continue };
            any = true;
            let te = match p.ty {
                TyId::Static(pmid, idx) => ws.module(pmid).ty_arena[idx],
                TyId::Local(_) => continue,
            };
            if matches!(te, TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if n.as_str() == "FieldOf")
            {
                found = true;
            }
        }
    }
    any.then_some(found)
}

/// Declared params of every fn named `callee`, unioned across overloads in
/// first-seen order (param name + rendered declared type). Drives named-arg
/// completion at a call site.
fn callee_param_names(ws: &Workspace, callee: &str) -> Vec<(Ustr, String)> {
    let name = Ustr::from(callee);
    let mut out: Vec<(Ustr, String)> = Vec::new();
    for m in ws.modules.iter() {
        let Some(fns) = m.functions_by_name.get(&name) else { continue };
        for (fmid, fid) in fns {
            let f = &ws.modules[fmid.index()].functions[*fid];
            for p in &f.param_decls {
                if out.iter().any(|(n, _)| *n == p.name) {
                    continue;
                }
                out.push((p.name, ws.render(None, p.ty)));
            }
        }
    }
    out
}

/// Typed expected-type oracle for one completion request's arg slot
/// (docs/assignable-plan.md). The callee's surviving overloads' params are
/// instantiated ONCE per overload (fresh vars) in a scratch
/// `Gamma::for_query` seeded from the cursor body's inference; candidate
/// compatibility is bone's REAL `coerce` under trial, rolled back per
/// candidate so one candidate's bindings never leak into the next. A UFCS
/// receiver coerces into each overload's first param and its bindings are
/// KEPT -- the receiver's instantiation is what the arg slot must agree with
/// (`nums.add_all(|)`: receiver `[int]` binds T, so the slot expects `[int]`).
struct ExpectedArg<'a> {
    gamma: Gamma<'a>,
    span: Span,
    slots: Vec<TyId>,
    /// DECLARED-form renders of the surviving slots ("Color", "[T]") -- the
    /// string-keyed reachability index's targets, receiver-filtered by
    /// construction (only overloads the receiver coerces into contribute). The
    /// reach graph is nominal-name keyed, so consuming these is a graph lookup,
    /// not a type compare -- bone's `coerce` is the only type-comparison path.
    rendered: Vec<String>,
    memo: HashMap<TyId, bool>,
    ret_memo: HashMap<TyId, bool>,
}

impl<'a> ExpectedArg<'a> {
    /// Throwaway Gamma over the cursor body's cached inference. None when the
    /// body is broken enough to have no cached inference -- the caller falls
    /// back to the rendered-string rung.
    fn scratch(
        ws: &'a Workspace,
        mid: ModuleId,
        inf: &InferenceResult,
        offset: usize,
    ) -> Option<(Gamma<'a>, Span)> {
        let module = ws.modules.get(mid.index())?;
        let owner = module.body_at(offset)?;
        let body = module.bodies.get(&owner)?;
        let seed = inf.per_fn.get(&owner)?;
        let span = Span {
            file: module.file,
            line_range: (0, 0),
            col_range: (0, 0),
            offset_range: (0, 0),
        };
        Some((Gamma::for_query(ws, body, mid, seed), span))
    }

    /// Session for `callee(.. <arg_idx> ..)`; `recv` is the UFCS receiver's
    /// type when the call is `recv.callee(..)`.
    fn for_call(
        ws: &'a Workspace,
        mid: ModuleId,
        inf: &InferenceResult,
        offset: usize,
        callee: &str,
        arg_idx: usize,
        recv: Option<TyId>,
    ) -> Option<ExpectedArg<'a>> {
        let (mut gamma, span) = Self::scratch(ws, mid, inf, offset)?;
        let name = Ustr::from(callee);
        let mut slots = Vec::new();
        let mut rendered: Vec<String> = Vec::new();
        for m in ws.modules.iter() {
            let Some(fns) = m.functions_by_name.get(&name) else { continue };
            for (fmid, fid) in fns {
                let f = &ws.modules[fmid.index()].functions[*fid];
                if f.param_decls.len() <= arg_idx {
                    continue;
                }
                let params: Vec<TyId> = f.param_decls.iter().map(|p| p.ty).collect();
                let Ok(inst) =
                    gamma.trial(|g| inference::instantiate_params(g, &f.generics, &params, span))
                else {
                    continue;
                };
                if let Some(rt) = recv {
                    let cp = gamma.out.checkpoint();
                    let ok = gamma.trial(|g| inference::coerce(g, rt, inst[0], span).is_ok());
                    if !ok {
                        gamma.out.rollback_to(cp);
                        continue;
                    }
                    // Bindings deliberately kept -- see the struct doc.
                }
                slots.push(inst[arg_idx]);
                let r = ws.render(None, params[arg_idx]);
                if !rendered.contains(&r) {
                    rendered.push(r);
                }
            }
        }
        (!slots.is_empty()).then(|| ExpectedArg {
            gamma,
            span,
            slots,
            rendered,
            memo: HashMap::new(),
            ret_memo: HashMap::new(),
        })
    }

    /// Session for a single concrete expected slot (struct-literal field
    /// value / lambda-body return type).
    fn for_slot(
        ws: &'a Workspace,
        mid: ModuleId,
        inf: &InferenceResult,
        offset: usize,
        slot: TyId,
    ) -> Option<ExpectedArg<'a>> {
        let (gamma, span) = Self::scratch(ws, mid, inf, offset)?;
        let rendered = match slot {
            TyId::Static(..) => vec![ws.render(None, slot)],
            TyId::Local(_) => Vec::new(),
        };
        Some(ExpectedArg {
            gamma,
            span,
            slots: vec![slot],
            rendered,
            memo: HashMap::new(),
            ret_memo: HashMap::new(),
        })
    }

    /// Would passing a value of type `ty` to this slot type-check? Bone's
    /// `coerce` under trial; bindings rolled back so candidates stay
    /// independent. Memoized per candidate TyId (candidates repeat few
    /// distinct types across a ~2500-entry ranking pass).
    fn accepts(&mut self, ty: TyId) -> bool {
        if let Some(&b) = self.memo.get(&ty) {
            return b;
        }
        let span = self.span;
        let mut ok = false;
        for i in 0..self.slots.len() {
            let slot = self.slots[i];
            let cp = self.gamma.out.checkpoint();
            let hit = self.gamma.trial(|g| inference::coerce(g, ty, slot, span).is_ok());
            self.gamma.out.rollback_to(cp);
            if hit {
                ok = true;
                break;
            }
        }
        self.memo.insert(ty, ok);
        ok
    }

    /// Would a CALL of fn `name` produce this slot's type? The candidate fn's
    /// own generics instantiate fresh (`clone(xs)` CAN return `[int]`), then
    /// its return coerces into a slot; everything rolls back.
    fn accepts_fn_returning(&mut self, ws: &Workspace, name: &str) -> bool {
        let n = Ustr::from(name);
        let span = self.span;
        let slots = self.slots.clone();
        for m in ws.modules.iter() {
            let Some(fns) = m.functions_by_name.get(&n) else { continue };
            for (fmid, fid) in fns {
                let f = &ws.modules[fmid.index()].functions[*fid];
                let Some(ret) = f.return_ty else { continue };
                if let Some(&b) = self.ret_memo.get(&ret) {
                    if b {
                        return true;
                    }
                    continue;
                }
                let cp = self.gamma.out.checkpoint();
                let inst = self
                    .gamma
                    .trial(|g| inference::instantiate_params(g, &f.generics, &[ret], span));
                let hit = match inst {
                    Ok(inst) => slots.iter().any(|&slot| {
                        let cp2 = self.gamma.out.checkpoint();
                        let ok = self
                            .gamma
                            .trial(|g| inference::coerce(g, inst[0], slot, span).is_ok());
                        self.gamma.out.rollback_to(cp2);
                        ok
                    }),
                    Err(_) => false,
                };
                self.gamma.out.rollback_to(cp);
                self.ret_memo.insert(ret, hit);
                if hit {
                    return true;
                }
            }
        }
        false
    }

    /// The slot expects nominal type `name` -- the enum-variant fast path
    /// (`Enum::V` candidates carry their enum's NAME, not a TyId). Unwraps
    /// one level of nullable so an `E?` param still boosts `E::..` variants.
    fn expects_nominal(&self, name: &str) -> bool {
        use bone_semantic::hir::{TypeCtor, TypeExpr};
        self.slots.iter().any(|&slot| {
            let te = *self.gamma.get(slot);
            let te = match te {
                TypeExpr::Nullable(inner) => *self.gamma.get(slot.with_idx(inner)),
                other => other,
            };
            matches!(te, TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if n.as_str() == name)
        })
    }
}

/// The return TyId shared by ALL overloads of `name`, or None if the
/// overloads disagree / any lacks a return type. Agreement is decided on the
/// RENDERED form (structural sameness across overloads' Static arenas), but
/// the TyId is what flows on -- the typed compare never re-parses the string.
fn fn_unique_return_ty(ws: &Workspace, name: &str) -> Option<TyId> {
    let n = Ustr::from(name);
    let mut ret: Option<(TyId, String)> = None;
    for m in ws.modules.iter() {
        let Some(fns) = m.functions_by_name.get(&n) else { continue };
        for (fmid, fid) in fns {
            let f = &ws.modules[fmid.index()].functions[*fid];
            let t = f.return_ty?;
            let r = ws.render(None, t);
            match &ret {
                None => ret = Some((t, r)),
                Some((_, prev)) if *prev == r => {}
                Some(_) => return None, // overloads disagree -> can't decide
            }
        }
    }
    ret.map(|(t, _)| t)
}

/// True when a leading `.` sits just before the partial identifier under the
/// cursor (a key-path `.field`, not member access -- `receiver_path` already
/// returned for a receiver-preceded dot). Decides insert text: `co` when the
/// dot is present, `.co` to add it.
fn leading_dot(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1;
    }
    i > 0 && b[i - 1] == b'.'
}

/// Start offsets of the arg segments that begin before `offset`, within the
/// innermost enclosing call -- used to resolve a callee generic (`T`) from a
/// sibling arg's inferred type. `[]` if not inside a call.
fn preceding_arg_starts(src: &str, offset: usize) -> Vec<usize> {
    let b = src.as_bytes();
    let end = offset.min(b.len());
    let mut i = end;
    let (mut p, mut br, mut bc) = (0i32, 0i32, 0i32);
    while i > 0 {
        match b[i - 1] {
            b')' => p += 1,
            b'(' if p > 0 => p -= 1,
            b'(' => break,
            b']' => br += 1,
            b'[' if br > 0 => br -= 1,
            b'[' => return Vec::new(),
            b'}' => bc += 1,
            b'{' if bc > 0 => bc -= 1,
            b'{' => return Vec::new(),
            b';' if p == 0 && br == 0 && bc == 0 => return Vec::new(),
            _ => {}
        }
        i -= 1;
    }
    if i == 0 {
        return Vec::new();
    }
    let mut starts = Vec::new();
    let (mut p, mut br, mut bc) = (0i32, 0i32, 0i32);
    let mut at_start = true;
    let mut j = i;
    while j < end {
        let c = b[j];
        if at_start && !c.is_ascii_whitespace() {
            starts.push(j);
            at_start = false;
        }
        match c {
            b'(' => p += 1,
            b')' if p > 0 => p -= 1,
            b'[' => br += 1,
            b']' if br > 0 => br -= 1,
            b'{' => bc += 1,
            b'}' if bc > 0 => bc -= 1,
            b',' if p == 0 && br == 0 && bc == 0 => at_start = true,
            _ => {}
        }
        j += 1;
    }
    starts
}

/// Fetch a `TypeExpr` for a `TyId` from the right arena, following `Infer`
/// substitutions so a solved inference var reads as its concrete type.
fn ty_of(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> bone_semantic::hir::TypeExpr {
    use bone_semantic::hir::{BuiltinTy, TypeExpr};
    let te = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => match body {
            Some(b) => b.ty_arena[idx],
            None => return TypeExpr::Builtin(BuiltinTy::Error),
        },
    };
    if let TypeExpr::Infer(var, _) = te {
        if let Some(sub) = body.and_then(|b| b.substitutions.get(var)) {
            return ty_of(ws, body, sub);
        }
    }
    te
}

/// Resolve `ty` through `Infer` substitutions to its canonical `TyId`, whose
/// child indices are valid via `with_idx`.
fn resolved_tyid(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> TyId {
    use bone_semantic::hir::TypeExpr;
    let te = match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => match body {
            Some(b) => b.ty_arena[idx],
            None => return ty,
        },
    };
    if let TypeExpr::Infer(var, _) = te {
        if let Some(sub) = body.and_then(|b| b.substitutions.get(var)) {
            return resolved_tyid(ws, body, sub);
        }
    }
    ty
}

/// Walk `param` (a callee param type carrying generics) against `arg` (an
/// inferred concrete type) in parallel; return the `arg` subtree bound to the
/// generic named `target`. Recurses through App and Nullable.
fn bind_generic(
    ws: &Workspace,
    target: Ustr,
    p_body: Option<&BodyInference>,
    param: TyId,
    a_body: Option<&BodyInference>,
    arg: TyId,
) -> Option<TyId> {
    use bone_semantic::hir::TypeExpr;
    let param = resolved_tyid(ws, p_body, param);
    let arg = resolved_tyid(ws, a_body, arg);
    match ty_of(ws, p_body, param) {
        TypeExpr::Generic(_, name) if name == target => Some(arg),
        TypeExpr::App { params: pp, .. } => {
            if let TypeExpr::App { params: ap, .. } = ty_of(ws, a_body, arg) {
                if pp.len() == ap.len() {
                    return pp.into_iter().zip(ap.into_iter()).find_map(|(pi, ai)| {
                        bind_generic(ws, target, p_body, param.with_idx(pi), a_body, arg.with_idx(ai))
                    });
                }
            }
            None
        }
        TypeExpr::Nullable(pi) => match ty_of(ws, a_body, arg) {
            TypeExpr::Nullable(ai) => {
                bind_generic(ws, target, p_body, param.with_idx(pi), a_body, arg.with_idx(ai))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The base nominal (struct) name of `ty`, if it is a `Nominal` App.
fn nominal_base(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> Option<Ustr> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    match ty_of(ws, body, ty) {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => Some(name),
        _ => None,
    }
}

/// Field-ref completions for a call-arg slot. `None` if the slot's expected
/// param isn't `FieldOf<Struct, V>` (caller falls through to normal
/// completion); `Some(items)` when it is -- offering the struct's fields whose
/// declared type is exactly V (the typechecker's invariance), whether or not
/// the leading `.` is typed. A generic `T` is resolved from a preceding sibling
/// arg's inferred type.
fn field_ref_arg_completions(
    ws: &Workspace,
    path: &str,
    src: &str,
    offset: usize,
    callee: &str,
    arg_idx: usize,
    prefix: &str,
    lead_dot: bool,
) -> Option<Vec<CompletionItem>> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let name = Ustr::from(callee);
    let (fmid, fid) = ws
        .modules
        .iter()
        .find_map(|m| m.functions_by_name.get(&name).and_then(|l| l.first()).copied())?;
    let f = &ws.modules[fmid.index()].functions[fid];
    let param = f.param_decls.get(arg_idx)?.ty;

    // The slot must be `FieldOf<T, V>`.
    let (t_idx, v_idx) = match ty_of(ws, None, param) {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), params }
            if n.as_str() == "FieldOf" && params.len() == 2 =>
        {
            let mut it = params.into_iter();
            (it.next().unwrap(), it.next().unwrap())
        }
        _ => return None,
    };
    let v_str = ws.render(None, param.with_idx(v_idx));

    // Resolve T -> a concrete struct name (directly, or via a sibling arg).
    let t_name = match ty_of(ws, None, param.with_idx(t_idx)) {
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => n,
        TypeExpr::Generic(_, gname) => {
            let mid = user_module_for_path(ws, path)?;
            let inf = ws.inference(mid)?;
            let starts = preceding_arg_starts(src, offset);
            let mut resolved = None;
            for (i, &aoff) in starts.iter().enumerate().take(arg_idx) {
                let Some(pty) = f.param_decls.get(i).map(|p| p.ty) else {
                    continue;
                };
                let Some(at) = bone_semantic::query::type_at(ws, mid, inf, aoff) else {
                    continue;
                };
                if let Some(bound) = bind_generic(ws, gname, None, pty, at.body, at.ty) {
                    if let Some(sn) = nominal_base(ws, at.body, bound) {
                        resolved = Some(sn);
                        break;
                    }
                }
            }
            resolved?
        }
        _ => return None,
    };

    let pfx = prefix.as_bytes();
    let mut seen: std::collections::HashSet<Ustr> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        let Some(&idx) = m.struct_by_name.get(&t_name) else {
            continue;
        };
        for fld in &m.structs[idx].fields {
            let fty = ws.render(None, fld.ty);
            if fty != v_str {
                continue;
            }
            if !(prefix.is_empty() || clean_match(fld.name.as_bytes(), pfx)) {
                continue;
            }
            if !seen.insert(fld.name) {
                continue;
            }
            // Dot already typed -> insert the bare name; otherwise add it.
            let insert = if lead_dot {
                fld.name.to_string()
            } else {
                format!(".{}", fld.name)
            };
            out.push(CompletionItem {
                sort_text: Some(arg_sort_text(0, fld.name.as_str(), pfx, 1, 0)),
                // The label carries the leading `.` -- a field ref IS `.co` (no
                // receiver), unlike member completion where the `recv.` is
                // already typed. `filter_text` stays the bare name so typing
                // `co` (with or without the dot) still matches.
                label: format!(".{}: {}", fld.name, fty),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(fty),
                insert_text: Some(insert),
                filter_text: Some(fld.name.to_string()),
                ..Default::default()
            });
        }
    }
    Some(out)
}

/// fzf-style fuzzy score of an already-lowercased `query` against an
/// already-lowercased candidate `cand` (higher = better match). Rewards long
/// CONTIGUOUS runs and matches at WORD BOUNDARIES (string start, or after a
/// `_`/`.`/`(`/`)`/`:`/space), so `tilecen` scores far higher on `tile_center`
/// (runs `tile` + boundary `cen`) than scattered across `table_new_with_size`,
/// and `tc` beats `tt` on `tile_center` (both letters at word starts). A small
/// DP over (query x cand) maximizing the bonus sum -- the same shape fzf uses.
/// Returns `MISS` when `query` isn't even a subsequence (callers pre-filter, so
/// that doesn't normally happen). Case is already folded by the caller.
/// Fuzzy subsequence score. The candidate is lowercased INLINE (ASCII), so
/// callers pass it raw -- no per-candidate `to_lowercase()` allocation. `query`
/// must already be lowercased (the typed prefix is).
/// Per-call phase timings for `completion()`, captured into a thread-local so a
/// profiler (`rock iter-completion`) can attribute per-keystroke latency to the
/// general-candidate, reachability, and call-through pipelines without parsing
/// log lines. Only the plain identifier-prefix path records these (the member /
/// enum / struct-literal early-returns leave the previous value untouched).
#[derive(Clone, Copy)]
pub struct CompletionPhaseTimings {
    pub candidates_ms: f64,
    pub rank_ms: f64,
    pub reach_ms: f64,
    pub call_through_ms: f64,
    pub n_general: usize,
    pub n_out: usize,
    /// Call-through explosion magnitude: fns passing the noise guard, total
    /// (fn,arg) pairs tested, and the subset that matched (got scored). These
    /// are the drivers behind `call_through_ms`.
    pub ct_fns: usize,
    pub ct_arg_iters: usize,
    pub ct_survivors: usize,
}

/// Per-fn counters accumulated inside `call_through_param_completions`, returned
/// to `completion()` so it can fold them into the phase timings.
#[derive(Clone, Copy, Default)]
struct CtCounts {
    fns: usize,
    arg_iters: usize,
    survivors: usize,
}

thread_local! {
    static LAST_COMPLETION_PHASES: std::cell::Cell<CompletionPhaseTimings> = const {
        std::cell::Cell::new(CompletionPhaseTimings {
            candidates_ms: 0.0,
            rank_ms: 0.0,
            reach_ms: 0.0,
            call_through_ms: 0.0,
            n_general: 0,
            n_out: 0,
            ct_fns: 0,
            ct_arg_iters: 0,
            ct_survivors: 0,
        })
    };
}

/// Read the phase timings recorded by the most recent `completion()` call ON
/// THIS THREAD. For the in-process profiler (`rock iter-completion`) only.
pub fn last_completion_phase_timings() -> CompletionPhaseTimings {
    LAST_COMPLETION_PHASES.with(|c| c.get())
}

// Shared fuzzy-scoring constants + boundary rule -- used by `fuzzy_score` and by
// the call-through partition decomposition (`fuzzy_prefix_scores` /
// `fuzzy_suffix_scores`), which MUST score identically to `fuzzy_score` for the
// decomposition `score = max_s(prefix[s] + suffix[s])` to stay byte-exact.
const FZ_MISS: i32 = -1_000_000;
const FZ_BASE: i32 = 1;
const FZ_BOUNDARY: i32 = 10; // matched char at a word start
const FZ_CONSEC: i32 = 8; // matched char right after the previous match (run)
const FZ_FIRST: i32 = 10; // extra for matching at absolute position 0


fn fuzzy_score(cand: &[u8], query: &[u8]) -> i32 {
    const MISS: i32 = FZ_MISS;
    const BASE: i32 = FZ_BASE;
    const BOUNDARY: i32 = FZ_BOUNDARY;
    const CONSEC: i32 = FZ_CONSEC;
    const FIRST: i32 = FZ_FIRST;
    if query.is_empty() {
        return 0;
    }
    let n = cand.len();
    if n == 0 {
        return MISS;
    }
    let boundary = |j: usize| fz_boundary(cand, j);
    // The two DP rows (`prev`/`cur`, each `n` wide) are reused from a thread-
    // local scratch -- `fuzzy_score` is called hundreds of times per keystroke
    // (clean_match per candidate fn, arg_sort_text per survivor, the general
    // ranking), and a per-call `vec![MISS; n]` x2 was the dominant short-prefix
    // cost. `resize` reuses the existing capacity, so steady state is alloc-free.
    thread_local! {
        static SCRATCH: std::cell::RefCell<(Vec<i32>, Vec<i32>)> =
            const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
    }
    SCRATCH.with(|sc| {
        let (prev, cur) = &mut *sc.borrow_mut();
        prev.clear();
        prev.resize(n, MISS); // best score with query[i-1] ending at each cand pos
        cur.clear();
        cur.resize(n, MISS);
        for (i, &qc) in query.iter().enumerate() {
            // `max_prev` = best prev[k] for k < j (query[i-1] somewhere before j).
            let mut max_prev = MISS;
            for j in 0..n {
                cur[j] = MISS;
                if cand[j].to_ascii_lowercase() == qc {
                    let mut b = BASE;
                    if boundary(j) {
                        b += BOUNDARY;
                    }
                    if j == 0 {
                        b += FIRST;
                    }
                    if i == 0 {
                        cur[j] = b;
                    } else {
                        let mut s = MISS;
                        if max_prev > MISS {
                            s = max_prev + b;
                        }
                        // Consecutive: query[i-1] ended exactly at j-1 -> extend the run.
                        if j > 0 && prev[j - 1] > MISS {
                            s = s.max(prev[j - 1] + b + CONSEC);
                        }
                        cur[j] = s;
                    }
                }
                if prev[j] > max_prev {
                    max_prev = prev[j];
                }
            }
            std::mem::swap(prev, cur);
        }
        let mut best = prev.iter().copied().max().unwrap_or(MISS);
        // A true whole-string prefix (typing the name/root from its start) is the
        // strongest signal -- it must beat any boundary-aligned partial match
        // elsewhere (`play` -> `player...` over `pixel_LAYer...`). Dominant bonus.
        if best > MISS && cand.len() >= query.len() && cand[..query.len()].eq_ignore_ascii_case(query) {
            best += 1000;
        }
        best
    })
}

// --- Call-through partition decomposition (lever A) ---------------------------
//
// `fuzzy_score(name + "(" + arg + ")", q)` is a max over alignments of `q`. Each
// alignment splits `q` at some index `s`: `q[..s]` matches inside the name,
// `q[s..]` inside the arg. The `(` between them breaks any consecutive-run bonus,
// so the two halves score INDEPENDENTLY:
//
//     score(name, arg, q) = max over s of [ prefix_name[s] + suffix_arg[s] ]
//
// `prefix_name[s]` depends only on (name, q); `suffix_arg[s]` only on (arg, q) --
// so the arg half is computed ONCE and shared by every fn in a group, and the
// per-(fn,arg) cost drops from a full DP to a length-`|q|` combine. EXACT: the
// `+1000` whole-query-prefix bonus never applies to call-through (a passing fn's
// name cannot contain all of `q`, by the noise guard `k < q.len()`), so it is
// intentionally omitted here.

/// `out[s]` = best `fuzzy_score`-style score of the prefix `q[..s]` matched as a
/// subsequence of `cand` (`s = 0..=q.len()`), WITHOUT the `+1000` bonus. `out[0]
/// = 0`; `out[s] = FZ_MISS` when `q[..s]` is not a subsequence. The forward DP is
/// `fuzzy_score`'s, recording the row max after each query char.
fn fuzzy_prefix_scores(cand: &[u8], query: &[u8], out: &mut Vec<i32>) {
    out.clear();
    out.push(0); // s = 0: empty prefix
    let m = query.len();
    let n = cand.len();
    if m == 0 {
        return;
    }
    if n == 0 {
        out.resize(m + 1, FZ_MISS);
        return;
    }
    let mut prev = vec![FZ_MISS; n];
    let mut cur = vec![FZ_MISS; n];
    for (i, &qc) in query.iter().enumerate() {
        let mut max_prev = FZ_MISS;
        let mut rowmax = FZ_MISS;
        for j in 0..n {
            cur[j] = FZ_MISS;
            if cand[j].to_ascii_lowercase() == qc {
                let mut b = FZ_BASE;
                if fz_boundary(cand, j) {
                    b += FZ_BOUNDARY;
                }
                if j == 0 {
                    b += FZ_FIRST;
                }
                if i == 0 {
                    cur[j] = b;
                } else {
                    let mut s = FZ_MISS;
                    if max_prev > FZ_MISS {
                        s = max_prev + b;
                    }
                    if j > 0 && prev[j - 1] > FZ_MISS {
                        s = s.max(prev[j - 1] + b + FZ_CONSEC);
                    }
                    cur[j] = s;
                }
            }
            if prev[j] > max_prev {
                max_prev = prev[j];
            }
            if cur[j] > rowmax {
                rowmax = cur[j];
            }
        }
        out.push(rowmax); // best score of q[..=i] = prefix length i+1
        std::mem::swap(&mut prev, &mut cur);
    }
}

/// `out[s]` = best `fuzzy_score`-style score of the suffix `q[s..]` matched as a
/// subsequence of `cand`, scored AS IF `cand` sits right after a `(`: `cand[0]`
/// is a word boundary (it is -- preceded by `(`) but NOT absolute position 0, so
/// it gets `FZ_BOUNDARY` but NOT `FZ_FIRST`. `out[q.len()] = 0`; `out[s] =
/// FZ_MISS` when `q[s..]` is not a subsequence. Backward DP: `next[j]` = best
/// score of `q[i+1..]` with `q[i+1]` starting at position `j`.
fn fuzzy_suffix_scores(cand: &[u8], query: &[u8], out: &mut Vec<i32>) {
    let m = query.len();
    out.clear();
    out.resize(m + 1, FZ_MISS);
    out[m] = 0; // empty suffix
    let n = cand.len();
    if m == 0 || n == 0 {
        return;
    }
    let mut next = vec![FZ_MISS; n]; // matchStart for q[i+1..]
    let mut cur = vec![FZ_MISS; n];
    for i in (0..m).rev() {
        let qc = query[i];
        let last = i + 1 == m;
        // `suffmax` = max next[j'] for j' > current j (scanning j descending).
        let mut suffmax = FZ_MISS;
        let mut rowmax = FZ_MISS;
        for j in (0..n).rev() {
            cur[j] = FZ_MISS;
            if cand[j].to_ascii_lowercase() == qc {
                let mut b = FZ_BASE;
                if fz_boundary(cand, j) {
                    b += FZ_BOUNDARY; // boundary; never FZ_FIRST (not at absolute 0)
                }
                let cont = if last {
                    0 // last query char: empty continuation
                } else {
                    // q[i+1] at some j' > j; +CONSEC only when adjacent (j'==j+1).
                    let mut c = suffmax;
                    if j + 1 < n && next[j + 1] > FZ_MISS {
                        c = c.max(next[j + 1] + FZ_CONSEC);
                    }
                    c
                };
                if cont > FZ_MISS {
                    cur[j] = b + cont;
                }
            }
            if next[j] > suffmax {
                suffmax = next[j];
            }
            if cur[j] > rowmax {
                rowmax = cur[j];
            }
        }
        out[i] = rowmax;
        std::mem::swap(&mut next, &mut cur);
    }
}

/// `max over s of prefix[s] + suffix[s]` -- the call-through partition combine.
/// Both vectors are `q.len()+1` long; `FZ_MISS` entries (non-subsequence splits)
/// are skipped. Returns `FZ_MISS` if no split is viable.
fn combine_scores(prefix: &[i32], suffix: &[i32]) -> i32 {
    let mut best = FZ_MISS;
    for s in 0..prefix.len() {
        if prefix[s] > FZ_MISS && suffix[s] > FZ_MISS {
            best = best.max(prefix[s] + suffix[s]);
        }
    }
    best
}

/// Ranking penalty for a call-through arg by PATH COMPLEXITY: `.` hops and `(`
/// method calls in the arg string. A direct value or enum variant has neither
/// (0); `player.movable.grid` pays for 2 field hops; `x.foo().image_dims()` pays
/// 2 hops + 2 calls. The boundary-rich `fuzzy_score` over-rewards long segmented
/// args (a scattered match on a deep method chain can out-score a tight match on
/// a clean short path -- e.g. `tile_center(PIXEL_EDITOR.active_layer_image().image_dims())`
/// beating `tile_center(player.movable.grid)`), so subtract this so a direct
/// value / clean field path wins on a near-tie. It's gentle (a call costs less
/// than one boundary bonus), so a genuinely strong fuzzy match still wins.
fn arg_path_penalty(arg: &str) -> i32 {
    const HOP: i32 = 1;
    const CALL: i32 = 6;
    let bytes = arg.as_bytes();
    let hops = bytes.iter().filter(|&&b| b == b'.').count() as i32;
    let calls = bytes.iter().filter(|&&b| b == b'(').count() as i32;
    hops * HOP + calls * CALL
}


/// Build the arg-position `sort_text`. Ordering is most-significant first:
/// `type_rank` (0 value-of-expected-type / 1 fn-returning-it / 2 other), then
/// the fuzzy match score DESCENDING (long boundary-aligned runs win -- `9999 -
/// score` so higher score sorts earlier), then `dist` (shorter paths first),
/// then `kind` (0 field/var < 1 method/fn), then the lowercased insert string
/// (case-insensitive tiebreak -- an UPPER_CASE global no longer wins a tie over
/// a lowercase local). All fields are fixed-width so lexicographic compare
/// matches the tuple.
fn arg_sort_text(type_rank: u8, cand: &str, query: &[u8], dist: u16, method: u8) -> String {
    // `fuzzy_score` lowercases the candidate inline; `query` is pre-lowercased.
    let score = fuzzy_score(cand.as_bytes(), query);
    sort_text_with_score(0, type_rank, cand, score, dist, method, false)
}


/// Like [`arg_sort_text`] but with the fuzzy score supplied by the caller --
/// used when the string the user is TYPING differs from the displayed `cand`.
/// An enum variant displays/insert as `Enum::Variant` but the user types the
/// VARIANT NAME, so the score is taken against `Variant` (else `nog` scores low
/// against `targeterror::notground` -- `n` is mid-string, no position-0 bonus --
/// and loses to a long path that starts with `n`).
fn sort_text_with_score(
    case_rank: u8,
    type_rank: u8,
    cand: &str,
    score: i32,
    dist: u16,
    method: u8,
    score_leads: bool,
) -> String {
    let lc = cand.to_lowercase();
    let score = score.clamp(0, 9999);
    // `{len:03}` before the name is a shorter-first tiebreak: equal-score
    // candidates (e.g. two prefix matches `attacking` / `attack_timer` of
    // `attack`) rank by length, so the tighter match wins -- without it the
    // alphabetical name fallback puts `attack_timer` first (`_` < `i`). Gated on
    // `score > 0`: at an EMPTY prefix every candidate scores 0 and there's no
    // match to be "tighter" about, so length must NOT reorder (else short stdlib
    // names bury longer user symbols past the result cap).
    let len_key = if score > 0 { lc.len().min(999) } else { 0 };
    let key2 = 9999 - score;
    let d = dist.min(99);
    if score_leads {
        // Non-arg (plain identifier prefix): MATCH QUALITY leads, then the
        // smart-case tiebreak, then KIND. A strong match on a SCREAMING_CONST /
        // PascalName is no longer buried below every weaker case-clean match --
        // case only decides near-ties (`jlog` -> `journal_log` over
        // `JIT_TRACE_LOG_ENABLED`, both score-equal). Call-through items
        // (type_rank >= 4) stay bucketed last regardless of their score.
        let ct_bucket = u8::from(type_rank >= 4);
        let kind = type_rank.min(3);
        format!("{ct_bucket}{key2:04}{case_rank}{kind}{d:02}{method}{len_key:03}{lc}")
    } else {
        // Arg / member / type-directed: the expected-TYPE bucket dominates (case
        // neutral there); score then dist then kind. Unchanged layout.
        format!("{case_rank}{type_rank}{key2:04}{d:02}{method}{len_key:03}{lc}")
    }
}

/// Strip a trailing `name($0)` / `name()` CALL template to the bare name when
/// the cursor is already directly before a `(` -- reusing the existing parens
/// instead of doubling them (`world_to_design()(x)`). Only touches call
/// templates: `{ $0 }` record/block and `<$0>` generic templates end in `}`/`>`
/// and a filled call-through (`foo(a, b)`) ends in `)` preceded by an arg, so
/// none of them match the exact `($0)`/`()` suffix.
fn strip_call_template(items: &mut [CompletionItem]) {
    for it in items.iter_mut() {
        let Some(t) = it.insert_text.as_deref() else { continue };
        if let Some(bare) = t.strip_suffix("($0)").or_else(|| t.strip_suffix("()")) {
            it.insert_text = Some(bare.to_string());
            it.insert_text_format = None;
        }
    }
}

/// Smart-case leading sort digit. Greedily align `query` into `cand` as a
/// case-INSENSITIVE subsequence (first match per query char), then count the
/// matched positions whose CASE differs from the query char. A lowercase query
/// over an all-caps name (`jlog` vs `JIT_..LOG..`) counts every char as a
/// mismatch and is demoted; a case-clean match (`jlog`/`journal_log`, or an
/// uppercase query over the caps name) scores 0. Capped at 9 (one sort digit).
/// Both slices are ORIGINAL case; an empty query yields 0 (no reordering).
fn case_mismatch_rank(cand: &[u8], query: &[u8]) -> u8 {
    let mut qi = 0;
    let mut miss = 0u32;
    for &cc in cand {
        if qi >= query.len() {
            break;
        }
        let qc = query[qi];
        if cc.eq_ignore_ascii_case(&qc) {
            miss += u32::from(cc != qc);
            qi += 1;
        }
    }
    miss.min(9) as u8
}

/// Length of the longest prefix of `query` (already lowercased) that is a
/// subsequence of `name_lc` (the function name, ALREADY lowercased -- callers
/// pass `CtFn::name_lc` so this never re-allocates a `to_lowercase` per fn on
/// the keystroke hot path) -- how many leading query chars the function name can
/// absorb. Used to prefilter call-through candidates: a fn whose name can't
/// absorb the start of the query is not a call the user is typing.
fn prefix_subseq_len(query: &[u8], name_lc: &str) -> usize {
    let mut qi = 0;
    for &c in name_lc.as_bytes() {
        if qi < query.len() && query[qi] == c {
            qi += 1;
        }
    }
    qi
}

/// Complete a whole call through its first parameter: typing `rejtanog` offers
/// `reject_target(TargetError::NotGround)` when the fn name absorbs the start of
/// the query and an arg value for its first param matches the rest. The
/// candidate is `fn(arg)` -- the reachability idea one level up. v1 fills the
/// arg from in-scope values of the param type and the type's enum UNIT variants
/// (the motivating case); reach-path args and tuple/record payloads are TODO
/// (`docs/completion-reachability-plan.md`).
/// Whether a call-through arg value needs a payload snippet: a tuple variant
/// (`Damage::Fire($..)`), a record variant (`Cfg { $.. }`), or none (a direct
/// value, unit variant, or reach path).
#[derive(Clone, Copy, PartialEq)]
enum ArgPayload {
    None,
    Tuple,
    Record,
}

/// Does the virtual call `name(arg)` subsequence-contain the query `q` (already
/// lowercased), GIVEN that the fn name already absorbs `q[..k]` (where `k =
/// prefix_subseq_len(q, name_lc)` -- the same value the noise guard computes)?
/// The residual `q[k..]` must subsequence-match `arg`, with the `(`/`)`
/// punctuation matched exactly as a scan of the whole `name(arg)` would.
///
/// Because the name-scan always lands at `qi == k`, this is BEHAVIOR-IDENTICAL to
/// scanning the full virtual `name(arg)` per fn -- but it depends only on
/// `(q, k, arg)`, NOT the fn name. That lets the call-through path memoize the
/// matching args per `(param_type, k)` and share the scan across every fn in the
/// group instead of re-testing each (fn, arg) pair (lever (b)). Equivalent to
/// `is_subsequence(q, format!("{name}({arg})").to_lowercase())` without building
/// the string -- the fzf "match, don't build" idea.
fn arg_matches_residual(q: &[u8], k: usize, arg: &str) -> bool {
    let mut qi = k.min(q.len());
    if qi < q.len() && q[qi] == b'(' {
        qi += 1;
    }
    for &c in arg.as_bytes() {
        if qi < q.len() && c.to_ascii_lowercase() == q[qi] {
            qi += 1;
        }
    }
    if qi < q.len() && q[qi] == b')' {
        qi += 1;
    }
    qi == q.len()
}

/// One call-through-eligible function: name (+ precomputed lowercase so the
/// per-keystroke noise guard doesn't re-lowercase ~1700 names every keystroke),
/// its first-param type (rendered), and whether it has more than one param.
struct CtFn {
    name: ustr::Ustr,
    name_lc: String,
    first_param_ty: String,
    multiparam: bool,
}

/// Precomputed call-through index: the function table + the GLOBAL-derived arg
/// values per param type (direct globals of the type, the type's enum variants,
/// and reach paths from globals -- `g.movable.grid`). All a pure function of the
/// type graph, so it is cached in `BoneProject::call_through` and rebuilt only on
/// non-`BodyOnly` edits. The per-keystroke path adds only the (small) in-scope
/// LOCAL args on top. Before this, `call_through_param_completions` rebuilt the
/// whole O(types*globals) reach expansion on EVERY keystroke (20-50ms stall).
struct CallThroughIndex {
    fns: Vec<CtFn>,
    /// `fns` indices bucketed by lowercase first byte of the fn name. The noise
    /// guard requires `name_lc[0] == q[0]`, so the per-keystroke loop walks only
    /// the `q[0]` bucket instead of every fn (and runs `prefix_subseq_len` only
    /// for plausible candidates, not all ~1700). Built with the index (a pure
    /// fn of the type graph).
    fns_by_first_char: HashMap<u8, Vec<u32>>,
    global_type_args: HashMap<String, Vec<(String, ArgPayload)>>,
}

/// Build the call-through index from a checked workspace + reach index. This is
/// the work that used to run per keystroke -- the O(types*globals) reach
/// expansion -- now done once per non-`BodyOnly` edit (alongside `reach`).
fn build_call_through(
    ws: &Workspace,
    reach: &crate::reachability::ReachabilityIndex,
) -> CallThroughIndex {
    let mut fns: Vec<CtFn> = Vec::new();
    for m in ws.modules.iter() {
        for f in m.functions.values() {
            if f.name.as_str().starts_with("__") {
                continue;
            }
            let Some(p0) = f.param_decls.first() else { continue };
            fns.push(CtFn {
                name: f.name,
                name_lc: f.name.as_str().to_lowercase(),
                first_param_ty: ws.render(None, p0.ty),
                multiparam: f.param_decls.len() > 1,
            });
        }
    }
    // Global roots (position-independent), each with its rendered type.
    let mut groots: Vec<(String, String)> = Vec::new();
    for m in ws.modules.iter() {
        for (gname, gidx) in m.global_by_name.iter() {
            groots.push((gname.to_string(), ws.render(None, m.globals[*gidx].ty)));
        }
    }
    // Static args per distinct first-param type. Seed each needed type with its
    // global-direct values + enum variants (no reach), then fill reach-from-
    // globals in ONE pass over globals -- a depth-<=2 path LOOKUP per global
    // (`arg_paths_from`), bucketed by target type. No per-(type,global) DFS.
    let needed: std::collections::HashSet<&str> =
        fns.iter().map(|c| c.first_param_ty.as_str()).collect();
    let mut global_type_args: HashMap<String, Vec<(String, ArgPayload)>> = HashMap::new();
    for t in &needed {
        let mut vals: Vec<(String, ArgPayload)> = groots
            .iter()
            .filter(|(_, rt)| rt == t)
            .map(|(n, _)| (n.clone(), ArgPayload::None))
            .collect();
        for (vname, shape) in crate::query::enum_variants_with_shape(ws, t) {
            let payload = match shape {
                crate::query::VariantShape::Unit => ArgPayload::None,
                crate::query::VariantShape::Tuple => ArgPayload::Tuple,
                crate::query::VariantShape::Record => ArgPayload::Record,
            };
            vals.push((format!("{t}::{vname}"), payload));
        }
        global_type_args.insert(t.to_string(), vals);
    }
    for (rn, rt) in &groots {
        for p in reach.arg_paths_from(rt) {
            if p.is_method && p.takes_args {
                continue; // can't park a snippet hole inside the arg
            }
            let Some(vals) = global_type_args.get_mut(p.final_ty.as_str()) else {
                continue; // not a first-param type any fn needs
            };
            let v = if p.is_method {
                format!("{rn}.{}()", p.suffix)
            } else {
                format!("{rn}.{}", p.suffix)
            };
            vals.push((v, ArgPayload::None));
        }
    }
    // Bucket fn indices by lowercase first byte for the per-keystroke first-char
    // lookup (the noise guard's `name_lc[0] == q[0]` condition).
    let mut fns_by_first_char: HashMap<u8, Vec<u32>> = HashMap::new();
    for (i, cf) in fns.iter().enumerate() {
        if let Some(&b) = cf.name_lc.as_bytes().first() {
            fns_by_first_char.entry(b).or_default().push(i as u32);
        }
    }
    CallThroughIndex { fns, fns_by_first_char, global_type_args }
}

fn call_through_param_completions(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    offset: usize,
    prefix: &str,
    index: &CallThroughIndex,
    reach: &crate::reachability::ReachabilityIndex,
    score_leads: bool,
) -> (Vec<CompletionItem>, CtCounts) {
    let mut counts = CtCounts::default();
    let q = prefix.as_bytes();
    if q.len() < 2 {
        // 1 char only completes the fn-name's first letter -- no room for an
        // arg yet. At 2+ chars the fn name can absorb the lead and the arg can
        // start to match, so call-through is meaningful.
        return (Vec::new(), counts);
    }
    // In-scope LOCAL roots only -- the position-dependent part. Globals + their
    // reach expansion are precomputed in `index` (the slow part, now cached).
    let locals: Vec<(String, String)> = crate::query::locals_in_scope(ws, mid, inf, offset)
        .into_iter()
        .map(|(n, _k, ty, _ty_id)| (n.to_string(), ty))
        .collect();

    // Call-through is a synthesized, bottom-ranked (bucket 4) convenience; the
    // GLOBAL `CAP` keeps it from burying the direct candidates. Per fn, the
    // per-fn cap applies ONLY to METHOD-chain args (`x.foo().bar()`), which are
    // unbounded reach-noise; direct values + field paths are NOT capped, so a
    // committed query (`tilecenp` -> `tile_center`) can explore all the sibling
    // fields it could pass (e.g. `player.movable.grid` AND `.prev_grid`).
    // Call-through is the ONLY arg-suggestion path while the fn name is still an
    // abbreviation -- arg completion needs a resolved callee to know the param
    // type, which `tilecen(` doesn't give -- so it must be browsable, not capped.
    const CAP: usize = 10;
    const PER_FN_METHOD_CAP: usize = 2;

    // Pass 1: walk only the first-char bucket (lever (a)) and keep the fns that
    // clear the noise guard, recording `k` (how many leading query chars the
    // name absorbs) and the set of distinct first-param types seen. NOISE GUARD:
    // the name must begin at the query's first char (guaranteed by the bucket),
    // absorb >= half of the query but NOT all of it, and land its chars cleanly
    // (word boundaries / a run). `k` matters because the arg-side match tests the
    // RESIDUAL `q[k..]`, which depends only on `(param_type, k)` -- NOT the name.
    let bucket: &[u32] = index.fns_by_first_char.get(&q[0]).map_or(&[], |v| v.as_slice());
    let mut passing: Vec<(usize, usize)> = Vec::new(); // (fn index, k)
    // Fns whose name FULLY covers the query (`k == q.len()`): normally suppressed
    // as fn-name browsing, but eligible for confident unique-arg autofill below.
    let mut committed: Vec<usize> = Vec::new();
    let mut needed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for &fi in bucket {
        let cf = &index.fns[fi as usize];
        let k = prefix_subseq_len(q, &cf.name_lc);
        if !clean_match(cf.name_lc.as_bytes(), &q[..k.min(q.len())]) {
            continue;
        }
        if k == q.len() {
            committed.push(fi as usize);
            needed.insert(cf.first_param_ty.as_str());
        } else if k >= q.len().div_ceil(2) {
            counts.fns += 1;
            passing.push((fi as usize, k));
            needed.insert(cf.first_param_ty.as_str());
        }
    }

    // Local-derived args per NEEDED type (local-direct + reach-from-locals),
    // built once and immutable after -- the per-`(type, k)` match memo below
    // borrows from it.
    let mut local_args: HashMap<&str, Vec<(String, ArgPayload)>> = HashMap::new();
    for &t in &needed {
        let mut v: Vec<(String, ArgPayload)> = locals
            .iter()
            .filter(|(_, lt)| lt == t)
            .map(|(n, _)| (n.clone(), ArgPayload::None))
            .collect();
        for (ln, lt) in &locals {
            for p in reach.arg_paths_from(lt) {
                if (p.is_method && p.takes_args) || p.final_ty != t {
                    continue;
                }
                let val = if p.is_method {
                    format!("{ln}.{}()", p.suffix)
                } else {
                    format!("{ln}.{}", p.suffix)
                };
                v.push((val, ArgPayload::None));
            }
        }
        local_args.insert(t, v);
    }

    // (b)+(A) Per `(param_type, k)` group, share BOTH the matching-arg set AND
    // each arg's SUFFIX score vector (`fuzzy_suffix_scores`, a pure fn of
    // `(arg, q)`): computed once, reused by every fn in the group. The arg-side
    // subsequence match depends only on the residual `q[k..]` (not the fn name),
    // so the group filter is exact (`arg_matches_residual`). The per-fn score is
    // then the partition combine `max_s(prefix_name[s] + suffix_arg[s])` (lever
    // A) -- a length-`|q|` max, not a full DP per (fn,arg) pair. This is byte-
    // EXACT vs the old per-pair `fuzzy_score(name(arg), q)` (the `(` junction
    // breaks any cross-half run, and the +1000 prefix bonus never applies to a
    // passing call-through fn), just without re-scoring the shared arg set per fn.
    let empty: Vec<(String, ArgPayload)> = Vec::new();
    // Per group: (arg, payload, suffix_scores, path_penalty). suffix + penalty
    // both depend only on the arg (not the fn), so computed once and shared.
    let mut match_memo: HashMap<(&str, usize), Vec<(&str, ArgPayload, Vec<i32>, i32)>> = HashMap::new();
    let mut prefix_scores: Vec<i32> = Vec::new(); // reused across fns

    // Collect lightweight candidates (score, fn index, arg, payload) across all
    // fns; materialize ONLY the global top `CAP`. Per fn we keep ALL simple args
    // (browsable fields) so the candidate pool can be large at a short prefix, but
    // we don't `format!` items we'll drop -- rank cheap, materialize survivors
    // (same pattern as general completion).
    let mut cands: Vec<(i32, usize, &str, ArgPayload)> = Vec::new();
    for (fi, k) in passing {
        let cf = &index.fns[fi];
        let t = cf.first_param_ty.as_str();
        let group = match_memo.entry((t, k)).or_insert_with(|| {
            let gargs = index.global_type_args.get(t).unwrap_or(&empty);
            let largs = local_args.get(t).unwrap_or(&empty);
            let mut v: Vec<(&str, ArgPayload, Vec<i32>, i32)> = Vec::new();
            for (a, payload) in gargs.iter().chain(largs.iter()) {
                counts.arg_iters += 1;
                if arg_matches_residual(q, k, a) {
                    let mut suffix = Vec::new();
                    fuzzy_suffix_scores(a.as_bytes(), q, &mut suffix);
                    v.push((a.as_str(), *payload, suffix, arg_path_penalty(a)));
                }
            }
            v
        });
        counts.survivors += group.len();
        // Prefix scores for THIS fn's name (one forward DP, reused buffer), then
        // combine with each arg's shared suffix vector.
        fuzzy_prefix_scores(cf.name.as_bytes(), q, &mut prefix_scores);
        let mut scored: Vec<(i32, &str, ArgPayload)> = Vec::with_capacity(group.len());
        for (a, payload, suffix, penalty) in group.iter() {
            // Bias toward simpler args: a direct value / clean field path beats a
            // deep method chain (which the boundary-rich score over-rewards). The
            // penalty is arg-only, precomputed once per group.
            let score = combine_scores(&prefix_scores, suffix) - penalty;
            scored.push((score, a, *payload));
        }
        // Cap only METHOD-chain args (an arg string containing a `(` -- the
        // unbounded `x.foo().bar()` reach-noise); keep ALL direct values / field
        // paths so sibling fields are browsable. The global `CAP` backstops the
        // total. (Method args still sort against everything in the final cap.)
        let (mut methods, simple): (Vec<_>, Vec<_>) =
            scored.into_iter().partition(|(_, a, _)| a.contains('('));
        if methods.len() > PER_FN_METHOD_CAP {
            // Best by score DESC, then arg lexical CASE-INSENSITIVE (so an
            // UPPER_CASE global doesn't beat a lowercase field path on a tie).
            methods.select_nth_unstable_by(PER_FN_METHOD_CAP, |x, y| {
                y.0.cmp(&x.0).then_with(|| ci_cmp(x.1.as_bytes(), y.1.as_bytes()))
            });
            methods.truncate(PER_FN_METHOD_CAP);
        }
        for (score, a, payload) in simple.into_iter().chain(methods) {
            cands.push((score, fi, a, payload));
        }
    }
    // CONFIDENT unique-arg autofill: for a committed (full-name-match) fn, the
    // residual query is empty so any arg "matches" -- offer `f(arg)` ONLY when the
    // first-param type has exactly ONE in-scope producer (a local/param of that
    // type or a single clean field path), i.e. the arg is unambiguous. Common
    // types (many producers) stay quiet; distinctive ones (`can_cast(s)`,
    // `spell_hotkey_label(s.kind)`) autofill even though the name covers the whole
    // query. Method-chain args are excluded (not "the obvious value").
    for fi in committed {
        let cf = &index.fns[fi];
        let t = cf.first_param_ty.as_str();
        let confident: Vec<&(String, ArgPayload)> = local_args
            .get(t)
            .map(|v| v.iter().filter(|(a, _)| !a.contains('(')).collect())
            .unwrap_or_default();
        if let [(a, payload)] = confident.as_slice() {
            // A full-name match with a unique, unambiguous arg is the highest-
            // confidence call-through -- rank it above the coincidental partial
            // (`passing`) matches (whose combined name+arg scores are far lower).
            let score = fuzzy_score(cf.name.as_bytes(), q) + 5000;
            cands.push((score, fi, a.as_str(), *payload));
        }
    }
    // Global top `CAP` by score (then ci-arg tiebreak); materialize only those.
    if cands.len() > CAP {
        cands.select_nth_unstable_by(CAP, |x, y| {
            y.0.cmp(&x.0).then_with(|| ci_cmp(x.2.as_bytes(), y.2.as_bytes()))
        });
        cands.truncate(CAP);
    }
    let mut out: Vec<CompletionItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (score, fi, a, payload) in cands {
        let cf = &index.fns[fi];
        let fname = cf.name;
        let call = format!("{fname}({a})");
        if !seen.insert(call.clone()) {
            continue;
        }
        // Insert: a tuple/record variant arg carries a payload hole; the trailing
        // param (when multiparam) takes `$0`, so the payload uses `$1` to keep the
        // tab order arg-then-next-param.
        let (insert_text, is_snippet) = match (payload, cf.multiparam) {
            (ArgPayload::None, false) => (format!("{fname}({a})"), false),
            (ArgPayload::None, true) => (format!("{fname}({a}, $0)"), true),
            (ArgPayload::Tuple, false) => (format!("{fname}({a}($0))"), true),
            (ArgPayload::Tuple, true) => (format!("{fname}({a}($1), $0)"), true),
            (ArgPayload::Record, false) => (format!("{fname}({a} {{ $0 }})"), true),
            (ArgPayload::Record, true) => (format!("{fname}({a} {{ $1 }}, $0)"), true),
        };
        let insert_text_format = is_snippet.then_some(InsertTextFormat::SNIPPET);
        // Label shows the payload shape so a tuple/record variant reads as a call
        // to fill, not a bare value.
        let label = match payload {
            ArgPayload::None => call.clone(),
            ArgPayload::Tuple => format!("{fname}({a}(...))"),
            ArgPayload::Record => format!("{fname}({a} {{ ... }})"),
        };
        out.push(CompletionItem {
            label,
            kind: Some(CompletionItemKind::FUNCTION),
            insert_text: Some(insert_text),
            insert_text_format,
            // type_rank 4: a synthesized whole-call, below every direct candidate
            // (vars/fns/structs/variants all rank <= 3); ranked among themselves by
            // match quality. Reuse the score computed above.
            sort_text: Some(sort_text_with_score(0, 4, &call, score, 0, 1, score_leads)),
            ..Default::default()
        });
    }
    // Final sort by sort_text; the global select above already bounded the count.
    rank_and_cap(&mut out, CAP, "call-through");
    (out, counts)
}

/// Max items returned by general (non-member) completion. Generous headroom over
/// a screenful -- the client narrows within a keystroke and re-requests (every
/// completion response is `isIncomplete`), so this exists only to stop
/// serializing the whole workspace (2539 items / ~518 KB on mag at empty prefix)
/// when the client will throw nearly all of it away and ask again.
const MAX_COMPLETION_ITEMS: usize = 200;

/// Case-insensitive ASCII byte-slice ordering -- the alloc-free equivalent of
/// `a.to_lowercase().cmp(&b.to_lowercase())` for the (ASCII) identifier
/// tiebreak in completion ranking. Matches the lowercased `lc` suffix that
/// `sort_text` appends, so the tuple sort selects the same survivors a
/// sort_text string sort would.
fn ci_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let (x, y) = (a[i].to_ascii_lowercase(), b[i].to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len())
}

/// Render every function's signature once -- the dominant completion cost --
/// keyed by name with first-overload-wins to match `crate::query::completion`'s
/// `seen`-dedup order (modules walked in `ws.modules` order). Stored in
/// `BoneProject::fn_sigs` and rebuilt only on non-`BodyOnly` edits, so body
/// keystrokes reuse it instead of re-rendering the whole workspace.
fn build_fn_sigs(ws: &Workspace) -> HashMap<ustr::Ustr, String> {
    let mut sigs = HashMap::new();
    for m in ws.modules.iter() {
        for (name, fns) in m.functions_by_name.iter() {
            if let Some((fmid, fid)) = fns.first() {
                sigs.entry(*name).or_insert_with(|| {
                    let f = &ws.modules[fmid.index()].functions[*fid];
                    crate::query::render_fn_signature(ws, *name, f)
                });
            }
        }
    }
    sigs
}

/// The single rank+cap used by every completion path: sort best-first by the
/// precomputed `sort_text`, then drop the tail past `cap`.
///
/// Sound to drop the tail because completion is always served `isIncomplete:
/// true` (see `do_completion_workspace`) -- the client re-requests per keystroke
/// and the server re-scores against the typed prefix, so an item cut from this
/// keystroke's `cap` reappears the moment the prefix ranks it back in. `what`
/// labels the truncation in the `BONE_LSP_PROFILE` log.
fn rank_and_cap(items: &mut Vec<CompletionItem>, cap: usize, what: &str) {
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    if items.len() > cap {
        if lsp_profile_enabled() {
            eprintln!("[bone completion] {what} truncated {} -> {cap}", items.len());
        }
        items.truncate(cap);
    }
}

/// True if any overload of `name` declares a return type rendering to one of
/// `expected`. Used to rank functions returning the wanted type just below
/// variables of that type.
/// Whether any function named `name` in the workspace is a `co fn`. Drives
/// coroutine-fn ranking after a `yield`/`yield*`.
fn fn_is_coroutine(ws: &Workspace, name: &str) -> bool {
    let n = Ustr::from(name);
    ws.modules.iter().any(|m| {
        m.functions_by_name
            .get(&n)
            .is_some_and(|fns| fns.iter().any(|(fmid, fid)| ws.modules[fmid.index()].functions[*fid].is_coroutine))
    })
}

/// Member sort_text used ONLY in a bool context (`if`/`while <chain>`), where no
/// reach-path items coexist. Inserts a bool-priority digit right after the score
/// so `bool` fields win a SCORE TIE -- a soft nudge, never overriding a
/// better-scoring match. Mirrors `sort_text_with_score`'s layout otherwise.
fn member_bool_sort_text(name: &str, pfx: &[u8], is_bool: bool, method: u8) -> String {
    let lc = name.to_lowercase();
    let score = fuzzy_score(name.as_bytes(), pfx).clamp(0, 9999);
    let bp = u8::from(!is_bool); // 0 = bool (ranks first), 1 = everything else
    // Leading `0` = case-neutral, aligning with `sort_text_with_score`'s
    // `{case_rank}` column (member context does not case-rank).
    format!("00{:04}{bp}01{method}{:03}{lc}", 9999 - score, lc.len().min(999))
}

/// Member sort_text used ONLY in a call-arg position (`callee(.., recv.<prefix>)`
/// with a known expected param type). Inserts a type-match priority digit BEFORE
/// the score so a field/method/reach-path whose type MATCHES the expected param
/// DOMINATES (mirrors the global path, where a value of the expected type
/// outranks a better-prefix candidate of the wrong type). The prefix filter still
/// removes wrong-prefix candidates first, so this only reorders genuine matches.
fn member_arg_sort_text(name: &str, pfx: &[u8], type_match: bool, method: u8, dist: u16) -> String {
    let lc = name.to_lowercase();
    let score = fuzzy_score(name.as_bytes(), pfx).clamp(0, 9999);
    let tp = u8::from(!type_match); // 0 = matches expected type (first), 1 = else
    let len_key = if score > 0 { lc.len().min(999) } else { 0 };
    // Leading `0` = case-neutral, aligning with `sort_text_with_score`'s
    // `{case_rank}` column (member context does not case-rank).
    format!("00{tp}{:04}{:02}{method}{len_key:03}{lc}", 9999 - score, dist.min(99))
}

/// Whether a struct named `name` is defined anywhere in the workspace.
fn struct_exists(ws: &Workspace, name: &str) -> bool {
    let u = Ustr::from(name);
    ws.modules.iter().any(|m| m.struct_by_name.contains_key(&u))
}

/// True when `offset` sits just after the `)` of a `co fn NAME(..)` signature
/// with no return type yet (`co fn spawn()|`). Pure byte heuristic; drives the
/// `-> Coroutine<CoFrame>` return-type snippet.
fn co_fn_return_slot(src: &[u8], offset: usize) -> bool {
    let mut i = offset.min(src.len());
    while i > 0 && src[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || src[i - 1] != b')' {
        return false;
    }
    // balance back to the matching `(`
    let mut depth = 0i32;
    let mut k = i - 1;
    loop {
        match src[k] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        if k == 0 {
            return false;
        }
        k -= 1;
    }
    // before the `(`: ws, fn-name ident, ws, `fn`, ws, `co`
    let mut p = k;
    let skip_ws = |p: &mut usize| while *p > 0 && src[*p - 1].is_ascii_whitespace() { *p -= 1; };
    skip_ws(&mut p);
    let name_end = p;
    while p > 0 && is_ident_byte(src[p - 1]) {
        p -= 1;
    }
    if p == name_end {
        return false; // no fn name
    }
    skip_ws(&mut p);
    let fn_end = p;
    while p > 0 && is_ident_byte(src[p - 1]) {
        p -= 1;
    }
    if &src[p..fn_end] != b"fn" {
        return false;
    }
    skip_ws(&mut p);
    let co_end = p;
    while p > 0 && is_ident_byte(src[p - 1]) {
        p -= 1;
    }
    &src[p..co_end] == b"co" && (p == 0 || !is_ident_byte(src[p - 1]))
}

/// The contextual lifecycle-hook keywords with a statement block form
/// (`on_killed { .. }`), parsed specially (`skull-parser` `LifecycleHook`).
fn is_lifecycle_hook(name: &str) -> bool {
    matches!(name, "on_killed" | "on_resume" | "on_hot_reload")
}

/// Param names that conventionally hold an `Entity<T>` (the `e`/`entity` idiom).
fn is_entity_param_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "e" | "ent" | "entity")
}

/// The struct named by a fn-name's first `_`-segment, capitalized (`mob_attack_co`
/// -> `Mob`), if such a struct exists -- the payload `T` for a synthesized
/// `Entity<T>`. None when the segment doesn't name a struct.
fn fn_prefix_struct(ws: &Workspace, fn_name: &str) -> Option<Ustr> {
    // Try each `_`-segment leading-first: `<type>_<verb>` (`mob_attack_co`) and
    // `<verb>_<type>` (`spawn_mob`) both name `Mob`. Leading match wins when
    // more than one segment is a struct (the UFCS convention).
    fn_name.split('_').filter(|s| !s.is_empty()).find_map(|seg| {
        let mut chars = seg.chars();
        let cap: String = chars.next()?.to_ascii_uppercase().to_string() + chars.as_str();
        let u = Ustr::from(&cap);
        ws.modules.iter().any(|m| m.struct_by_name.contains_key(&u)).then_some(u)
    })
}

/// The identifier characters immediately preceding `offset` (a byte offset).
/// Rock identifiers are ASCII (`[A-Za-z0-9_]`), so walking bytes never splits a
/// multibyte codepoint -- a non-ASCII byte is not alphanumeric and stops the walk.
fn ident_prefix(src: &str, offset: usize) -> &str {
    let end = offset.min(src.len());
    let bytes = src.as_bytes();
    let mut start = end;
    while start > 0 {
        let c = bytes[start - 1];
        if c == b'_' || c.is_ascii_alphanumeric() {
            start -= 1;
        } else {
            break;
        }
    }
    &src[start..end]
}

fn is_ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// True when the cursor is in decorator position (`@<ident-prefix>|`): the ident
/// under the cursor is immediately preceded by an `@`. The `@` isn't an ident
/// byte, so `ident_prefix` already excludes it -- this only checks the trigger.
fn decorator_context(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1;
    }
    i > 0 && b[i - 1] == b'@'
}

/// Completion candidates in decorator position: every `deco` declaration in the
/// workspace (game decorators like `@update`/`@late_draw` live in the engine),
/// prefix-filtered. Inserts the bare name -- the `@` is already typed.
fn decorator_completions(ws: &Workspace, prefix: &str) -> Vec<CompletionItem> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in ws.modules.iter() {
        for (_, dec) in m.decorators.iter() {
            let name = dec.name.as_str();
            if !clean_match(name.as_bytes(), prefix.as_bytes()) || !seen.insert(name.to_string()) {
                continue;
            }
            let params: Vec<String> = dec
                .param_decls
                .iter()
                .map(|p| format!("{}: {}", p.name, ws.render(None, p.ty)))
                .collect();
            out.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: (!params.is_empty()).then(|| format!("deco ({})", params.join(", "))),
                insert_text: Some(name.to_string()),
                sort_text: Some(sort_text_with_score(0, 0, name, 9999, 0, 0, true)),
                ..Default::default()
            });
        }
    }
    out
}

/// Completion-parseable variants of the source up to `offset`: keep the prefix
/// (where the receiver and earlier decls live), drop the broken tail, insert a
/// synthetic identifier so the trigger `.` has a field, then close every open
/// `([{`. This is the standard "parse up to the cursor" fixup -- structural
/// resolution needs only a parseable prefix, not a correct whole file.
///
/// Two candidates are tried in order: the bare prefix, then the prefix with an
/// empty block appended -- the latter recovers a dangling block header
/// (`if x.`, `while x.`, `for i in x.`) whose body the user hasn't typed.
///
/// Caveat: delimiter counting ignores string/comment contents, and declarations
/// *after* the cursor are dropped -- acceptable for completion, which edits in
/// the middle of an otherwise-defined file.
/// Inline `labelDetails.detail` for a completion item, rendered directly after
/// the label in the popup: a function shows `(params) -> ret` (the signature
/// minus the redundant `fn name`); a binding/global shows `: type`. None when
/// there's no detail. Structs/enums (label is the bare name, kind conveys it)
/// and fields (label already `name: type`) pass `None`.
fn fn_or_type_label_details(kind: SymCat, detail: Option<&str>) -> Option<CompletionItemLabelDetails> {
    let d = detail?;
    let inline = match kind {
        // `fn name(a: T) -> R` -> `(a: T) -> R`
        SymCat::Function => d.find('(').map_or_else(|| d.to_string(), |i| d[i..].to_string()),
        SymCat::Local | SymCat::Param | SymCat::Global => format!(": {d}"),
        _ => return None,
    };
    Some(CompletionItemLabelDetails { detail: Some(inline), description: None })
}

/// Re-type-check the WHOLE project with the current file replaced by `munged`,
/// so completion resolves cross-file / dependency types (e.g. a param of an
/// engine struct). `combined` is the session's `combined_unexpanded()` -- warm
/// stdlib + every project module, incl. the current file's original AST, which
/// is swapped out for the munge. A single-file reparse can't see cross-file
/// types, which is why member/struct-literal completion came up empty in real
/// projects.
fn reparse_with_project(
    combined: &[skull_parser::Ast],
    path: &str,
    munged: &str,
) -> Option<Workspace> {
    let t = std::time::Instant::now();
    let out = reparse_with_project_inner(combined, path, munged);
    if lsp_profile_enabled() {
        eprintln!(
            "[bone completion] reparse_with_project {:.2}ms ({} modules, recovered: {})",
            t.elapsed().as_secs_f64() * 1000.0,
            combined.len(),
            out.is_some()
        );
    }
    out
}

/// Analyze one completion munge: apply it as a speculative Session variant
/// (incremental cost -- a body-local munge re-infers one body) and run `f`
/// against the variant workspace. If the session REFUSES the variant (strict
/// rejection: a name colliding with the stdlib, macro-expansion failure, hard
/// tokenize error), fall back to a single-file scratch pass over the munged
/// text alone -- same-file types still resolve, the last rung of the old
/// reparse ladder. `None` only when both rungs fail.
fn analyze_variant<R>(
    session: &mut Session,
    path: &str,
    munged: &str,
    f: impl Fn(&Workspace) -> R,
) -> Option<R> {
    match session.with_file_variant(path, munged, &f) {
        Ok(r) => Some(r),
        Err(e) => {
            log::warn!(
                "completion: session refused the variant ({}); single-file fallback",
                e.message
            );
            if lsp_profile_enabled() {
                eprintln!(
                    "[bone completion] variant refused ({}) -> single-file fallback",
                    e.message
                );
            }
            let fallback = skull_parser::parse_module(munged, Ustr::from(path))
                .ok()
                .and_then(|m| bone_semantic::semantic_passes::semantic_pass(&mut vec![m]).ok());
            match fallback {
                Some(ws) => Some(f(&ws)),
                None => {
                    log::warn!(
                        "completion: single-file fallback also failed for `{path}`; \
                         this munge yields no completions"
                    );
                    None
                }
            }
        }
    }
}

fn reparse_with_project_inner(
    combined: &[skull_parser::Ast],
    path: &str,
    munged: &str,
) -> Option<Workspace> {
    // Project-wide pass: every module except the current file, plus the munge.
    // Resolves cross-file / dependency types (an `IVec2` engine param, etc.).
    let mut modules: Vec<skull_parser::Ast> =
        combined.iter().filter(|a| a.file.as_str() != path).cloned().collect();
    // The munge is a half-typed buffer (missing bracket, `recv.` mid-edit). The
    // parse is total -- skull-parser drops the broken statement and keeps the rest
    // (the recovery error stays on `recovery_errors`, which the LSP path never
    // gates on) -- so completion still resolves from the best-effort tree (backlog
    // #21). Only a hard tokenizer failure is `Err`/`None` here.
    modules.push(skull_parser::parse_module(munged, Ustr::from(path)).ok()?);
    if let Ok(ws) = bone_semantic::semantic_passes::semantic_pass(&mut modules) {
        return Some(ws);
    }
    // Fall back to the munged file alone (same-file structural resolution) when
    // the project pass can't be built -- e.g. a standalone file whose path
    // doesn't line up with the session's module ids. Keeps single-file behavior.
    let mut single = vec![skull_parser::parse_module(munged, Ustr::from(path)).ok()?];
    bone_semantic::semantic_passes::semantic_pass(&mut single).ok()
}

pub(crate) fn completion_munges(src: &str, offset: usize) -> [String; 2] {
    let off = offset.min(src.len());
    let (head, tail) = src.split_at(off);

    // Inject a sentinel identifier at the cursor (rust-analyzer's technique) so
    // the in-progress `recv.` / `Foo {` parses, then KEEP the rest of the file:
    // items defined AFTER the cursor (e.g. a UFCS method later in the same file)
    // must stay in the parse, or they're silently missing from completion.
    // skull-parser recovers at item/statement granularity, so the partially
    // sentinel-mangled current statement drops by itself without taking the tail
    // with it. balance_delimiters runs over head+tail together, so a delimiter
    // the tail already closes adds no spurious closer.
    let mk = |inject: &str| {
        let mut s = String::with_capacity(src.len() + 32);
        s.push_str(head);
        s.push_str("__rock_completion__");
        s.push_str(inject);
        s.push_str(tail);
        balance_delimiters(s)
    };

    [mk(""), mk(" {}")]
}

/// Append a closer for every still-open `([{` in `s` (innermost first).
///
/// Brace counting skips `//` line comments and `"`-delimited strings, so a brace
/// in trailing prose (`// missing }`) or a string literal isn't mistaken for real
/// nesting. When the completion munge keeps the file tail, the tail often ends in
/// a `// ...` comment with no trailing newline; the appended closers go on a fresh
/// line so they aren't swallowed by that comment (which would leave the enclosing
/// block unclosed and fail the whole parse). An unterminated string is closed too.
pub(crate) fn balance_delimiters(mut s: String) -> String {
    // One stack models the whole nesting: code delimiters, strings, and the
    // interpolation braces that re-enter CODE inside a string (`"a {expr}"`)
    // -- an unterminated `"foo {` must close as `}"`, not `"`, or the
    // tokenizer still fatals on the unmatched interpolation.
    #[derive(PartialEq)]
    enum Ctx {
        Paren,
        Bracket,
        Brace,
        /// `{` inside a string: code until the matching `}`.
        Interp,
        LineStr,
        TripleStr,
    }
    let mut stack: Vec<Ctx> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_line_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        match stack.last() {
            Some(Ctx::LineStr) | Some(Ctx::TripleStr) => {
                let triple = stack.last() == Some(&Ctx::TripleStr);
                match b {
                    b'\\' => i += 1, // skip the escaped character
                    b'"' if triple && bytes.get(i + 1..i + 3) == Some(b"\"\"") => {
                        stack.pop();
                        i += 2;
                    }
                    b'"' if !triple => {
                        stack.pop();
                    }
                    // `{{` emits a literal `{`; a bare `{` opens interpolation
                    b'{' if bytes.get(i + 1) == Some(&b'{') => i += 1,
                    b'{' => stack.push(Ctx::Interp),
                    _ => {}
                }
                i += 1;
            }
            _ => {
                match b {
                    b'/' if bytes.get(i + 1) == Some(&b'/') => {
                        in_line_comment = true;
                        i += 1;
                    }
                    b'"' if bytes.get(i + 1..i + 3) == Some(b"\"\"") => {
                        stack.push(Ctx::TripleStr);
                        i += 2;
                    }
                    b'"' => stack.push(Ctx::LineStr),
                    b'(' => stack.push(Ctx::Paren),
                    b'[' => stack.push(Ctx::Bracket),
                    b'{' => stack.push(Ctx::Brace),
                    b')' | b']' | b'}' => {
                        // a `}` closes a code Brace or pops Interp back into
                        // its string; `)`/`]` close their code delimiter
                        stack.pop();
                    }
                    _ => {}
                }
                i += 1;
            }
        }
    }
    if in_line_comment && !stack.is_empty() {
        s.push('\n'); // source ends inside a `//` comment; keep closers out of it
    }
    while let Some(c) = stack.pop() {
        s.push_str(match c {
            Ctx::Paren => ")",
            Ctx::Bracket => "]",
            Ctx::Brace => "}",
            Ctx::Interp => "}",
            Ctx::LineStr => "\"",
            Ctx::TripleStr => "\"\"\"",
        });
    }
    s
}

/// Field completions for an enum record-variant literal. Reparses (the
/// incomplete `{ }` breaks the live body) and lists the variant's record fields.
/// `None` when the head doesn't resolve to a record variant (a unit/tuple variant
/// block head like `if x == Enum::Variant { .. }` is not a record literal) -- the
/// caller falls through to global completion. `Some(vec![])` only when the variant
/// IS a record but the prefix filtered every field out.
fn enum_record_field_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    enum_name: &str,
    variant: &str,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    for munged in completion_munges(src, offset) {
        let Some(fields) = analyze_variant(session, path, &munged, |ws| {
            crate::query::variant_record_fields(ws, enum_name, variant)
        }) else {
            continue;
        };
        if fields.is_empty() {
            continue;
        }
        return Some(
            fields
                .into_iter()
                .filter(|(name, _)| prefix.is_empty() || clean_match(name.as_bytes(), prefix.as_bytes()))
                .map(|(name, ty)| CompletionItem {
                    label: format!("{name}: {ty}"),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(ty),
                    insert_text: Some(name),
                    ..Default::default()
                })
                .collect(),
        );
    }

    None
}

/// Field completions for `Name { <field> }`. Offers the struct's fields not
/// already written in the literal, filtered by the prefix. Returns None (so the
/// caller falls through to the global namespace) when `name` isn't a struct.
fn struct_literal_field_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    name: &str,
    brace_open: usize,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    // Analyze a balanced/synthetic munge as a Session variant so the struct def
    // survives the broken literal -- and so a struct defined in another file /
    // dependency resolves.
    let fields = completion_munges(src, offset).into_iter().find_map(|munged| {
        let f = analyze_variant(session, path, &munged, |ws| {
            crate::query::struct_fields_by_name(ws, name)
        })?;
        (!f.is_empty()).then_some(f)
    })?;

    // Fields already written between the `{` and the cursor (`name :`), to skip.
    let written = src.get(brace_open + 1..offset.min(src.len())).unwrap_or("");
    let present: std::collections::HashSet<String> = written
        .split(|c: char| c == ',' || c == '{' || c == '}')
        .filter_map(|seg| seg.split(':').next())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();

    Some(
        fields
            .into_iter()
            .filter(|(n, _)| !present.contains(n))
            .filter(|(n, _)| prefix.is_empty() || clean_match(n.as_bytes(), prefix.as_bytes()))
            .map(|(n, ty)| CompletionItem {
                label: format!("{n}: {ty}"),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(ty),
                insert_text: Some(n),
                ..Default::default()
            })
            .collect(),
    )
}

/// True if the `Enum::Variant` being completed at `offset` sits in a match-arm
/// **pattern** position rather than a value position. Two heuristics, in order:
///
/// 1. **Forward `=>` scan** -- for an arm whose body is already present, scan
///    past the variant name and an optional balanced payload group (`(..)` /
///    `{..}`); the next tokens being `=>` mark an arm pattern. A value position
///    (`let x = ..`, `return ..`, a call argument) never leads to `=>`.
/// 2. **Backward enclosing-`match` scan** -- a FRESH arm (`SpellState::Loa|`
///    with no `=>` typed yet) leads to no `=>`, and is lexically indistinguishable
///    from a value position by what precedes it (`{`/`,` open blocks AND args
///    too). The reliable signal is that the nearest enclosing block is a
///    `match <scrutinee> { .. }`, so walk out to that brace and check it.
///
/// Patterns bind, they don't construct -- so in either case no payload template
/// is offered.
fn in_pattern_position(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    // the variant identifier under / after the cursor
    while i < b.len() && is_ident_byte(b[i]) {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    // an already-typed payload pattern, e.g. `Loaded(x)` / `Bandage { .. }`
    if i < b.len() && (b[i] == b'(' || b[i] == b'{') {
        let (open, close) = if b[i] == b'(' { (b'(', b')') } else { (b'{', b'}') };
        let mut depth = 0i32;
        while i < b.len() {
            if b[i] == open {
                depth += 1;
            } else if b[i] == close {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 1 < b.len() && b[i] == b'=' && b[i + 1] == b'>' {
        return true;
    }
    // Fresh arm: no `=>` yet. Find the start of the `Enum::Variant` path the
    // cursor is in (ident bytes + `:` run backward).
    let mut start = offset.min(b.len());
    while start > 0 && (is_ident_byte(b[start - 1]) || b[start - 1] == b':') {
        start -= 1;
    }
    // The significant byte just before the path. An arm HEAD is opened by the
    // match `{`, an arm separator `,`, or a previous arm's block-body close `}`.
    // An arm BODY (value position) is preceded by `=>`; `let`/call/index/struct
    // contexts by `=`/`(`/`[`/`:` -- all rejected here. This excludes arm bodies
    // so the enclosing-`match` test below only fires for actual arm heads.
    let mut p = start;
    while p > 0 && b[p - 1].is_ascii_whitespace() {
        p -= 1;
    }
    if p == 0 || !matches!(b[p - 1], b'{' | b',' | b'}') {
        return false;
    }
    enclosing_block_is_match(b, start)
}

/// Whether the innermost block enclosing byte `from` is the body of a `match`
/// expression -- i.e. `from` sits in an arm position. Walks backward to the
/// nearest unmatched `{` (balancing `()[]{}`); a `(`/`[` reached first means we
/// are inside a call/index, not a block, so not a match. From that `{`, scans the
/// scrutinee back to the `match` keyword: only `()`/`[]`-balanced scrutinee
/// tokens may intervene; a `{`/`}`/`;` boundary at depth 0 ends the search
/// (the brace governs something else, e.g. a fn body or struct literal).
fn enclosing_block_is_match(b: &[u8], from: usize) -> bool {
    let mut depth = 0i32;
    let mut i = from;
    let brace = loop {
        if i == 0 {
            return false;
        }
        i -= 1;
        match b[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' => {
                if depth == 0 {
                    return false; // inside a call / index group, not a block
                }
                depth -= 1;
            }
            b'{' => {
                if depth == 0 {
                    break i;
                }
                depth -= 1;
            }
            _ => {}
        }
    };
    // Scan the scrutinee backward from the `{` to the governing keyword.
    let mut j = brace;
    let mut depth = 0i32;
    while j > 0 {
        j -= 1;
        match b[j] {
            b')' | b']' => depth += 1,
            b'(' | b'[' => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            b'{' | b'}' | b';' if depth == 0 => return false,
            c if depth == 0 && is_ident_byte(c) => {
                // Read this whole word. `match` -> the brace is a match body.
                // Any other word is part of the scrutinee (`match foo.bar {`) or
                // the head of some other construct (`fn r() {`); keep scanning
                // back -- the `{`/`}`/`;` boundary arms above stop a non-match
                // brace before a spurious earlier `match` can be reached.
                let end = j + 1;
                while j > 0 && is_ident_byte(b[j - 1]) {
                    j -= 1;
                }
                if &b[j..end] == b"match" {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Variant completions for `EnumName::<variant>`. The trailing `::<partial>` is
/// a hard error that aborts the whole module, so we truncate it back to the bare
/// enum name (accepted as a value) and balance -- that keeps the enum *def* in
/// the parse, which is all the variant list needs.
///
/// In a value position each payload-carrying variant is templated as a snippet
/// so the caller doesn't hand-fill it: a tuple variant inserts `Variant($0)`, a
/// record variant `Variant { $0 }`, with the cursor parked inside. Unit variants
/// (and any variant in a match-arm pattern position) insert the bare name.
fn enum_qualified_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    enum_name: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    // The query only needs the enum DEF resolvable, so the standard sentinel
    // munges (body-local -- a cheap variant tier) serve it; a truncate-at-`::`
    // munge would delete every item after the cursor and turn the variant into
    // an interface-tier edit.
    let mut all = Vec::new();
    for munged in completion_munges(src, offset) {
        let attempt = analyze_variant(session, path, &munged, |ws| {
            crate::query::enum_variants_with_shape(ws, enum_name)
        });
        if let Some(v) = attempt {
            if !v.is_empty() {
                all = v;
                break;
            }
        }
    }

    let pattern = in_pattern_position(src, offset);
    let matches_prefix =
        |v: &str| prefix.is_empty() || clean_match(v.as_bytes(), prefix.as_bytes());
    // When the in-progress prefix matches NO variant (a typo like `TextAlign::top`),
    // returning empty hands the popup to the client's buffer-word fallback -- the
    // worst outcome, since an enum is a closed set. Show every variant instead, and
    // force each past the CLIENT's own prefix filter via `filter_text = prefix` (cmp
    // re-matches the typed text against each item; without this they vanish again).
    // The response is `is_incomplete`, so the client re-requests each keystroke and
    // the prefix stays fresh. A partial match keeps normal narrowing + default filter.
    let none_match = !prefix.is_empty() && !all.iter().any(|(v, _)| matches_prefix(v));
    let force_filter = none_match.then(|| prefix.to_string());

    all.into_iter()
        .filter(|(v, _)| none_match || matches_prefix(v))
        .map(|(v, shape)| {
            use crate::query::VariantShape;
            let (insert_text, insert_text_format) = match (pattern, shape) {
                (false, VariantShape::Tuple) => {
                    (Some(format!("{v}($0)")), Some(InsertTextFormat::SNIPPET))
                }
                (false, VariantShape::Record) => {
                    (Some(format!("{v} {{ $0 }}")), Some(InsertTextFormat::SNIPPET))
                }
                _ => (None, None),
            };
            CompletionItem {
                label: v,
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                filter_text: force_filter.clone(),
                insert_text,
                insert_text_format,
                ..Default::default()
            }
        })
        .collect()
}

/// Struct-field completions for `receiver.<field>`. The live body is broken by
/// the trigger `.`, so this munges in a synthetic identifier (rust-analyzer's
/// technique) to get a parseable file, then resolves the receiver's struct
/// **structurally** (no inference required). Field labels are `name: type`.
/// Each munge is analyzed as a speculative Session variant (one incremental
/// update + restore), not a from-scratch project pass.
fn member_completions(
    session: &mut Session,
    path: &str,
    src: &str,
    offset: usize,
    segments: &[String],
    prefix: &str,
    arg_slot: Option<(String, usize)>,
    reach: Option<&crate::reachability::ReachabilityIndex>,
    bool_ctx: bool,
) -> Vec<CompletionItem> {
    for munged in completion_munges(src, offset) {
        let attempt = analyze_variant(session, path, &munged, |ws| {
            member_completion_items(
                ws, path, offset, segments, prefix, arg_slot.clone(), reach, bool_ctx,
            )
        });
        if let Some(items) = attempt {
            if !items.is_empty() {
                return items;
            }
        }
    }
    Vec::new()
}

/// One munge attempt's member items, resolved against the variant workspace.
/// Whether a member of type `ty_id` (rendered `ty_render`) type-directs into
/// the expected arg slot. Bone's `coerce` on the TyId when present (so an `int`
/// field boosts in an `int?`/`float`/`any` slot); rendered-string equality when
/// the member has no stored TyId (a generic-substituted field) or there's no
/// session. False with no session at all.
fn member_ty_match(mut expected: Option<&mut ExpectedArg>, ty_id: Option<TyId>, ty_render: &str) -> bool {
    let Some(exp) = expected.as_mut() else { return false };
    match ty_id {
        Some(t) => exp.accepts(t),
        None => exp.rendered.iter().any(|e| e == ty_render),
    }
}

fn member_completion_items(
    ws: &Workspace,
    path: &str,
    offset: usize,
    segments: &[String],
    prefix: &str,
    arg_slot: Option<(String, usize)>,
    reach: Option<&crate::reachability::ReachabilityIndex>,
    bool_ctx: bool,
) -> Vec<CompletionItem> {
    {
        // The CURRENT file's module (where the receiver/offset live) -- not just
        // "first non-builtin", which is wrong now that the whole project loads.
        let Some(mid) = user_module_for_path(ws, path) else {
            return Vec::new();
        };

        let fields = crate::query::struct_fields_for_path(ws, mid, offset, segments);
        // UFCS methods: functions whose first param is the receiver's struct,
        // callable as `recv.method(...)`. Surfaced alongside fields, as rock does.
        let recv_struct = crate::query::struct_name_for_path(ws, mid, offset, segments);
        let methods = recv_struct
            .map(|name| crate::query::ufcs_methods(ws, name))
            .unwrap_or_default();
        if fields.is_empty() && methods.is_empty() {
            return Vec::new();
        }
        // Typed expected-arg session for the enclosing call, built against THIS
        // (variant) workspace so its Gamma resolves the member TyIds the
        // candidates carry (docs/assignable-plan.md phase 2). `coerce` decides
        // the member type-match, not a rendered-string `==`; the receiver-
        // filtered `.rendered` still feeds the string-keyed reach index. None
        // when not in an arg slot or the body has no cached inference -- then
        // there's no type direction (kind/name order), same as before.
        let mut expected = arg_slot.as_ref().and_then(|(callee, idx)| {
            let inf = ws.inference(mid)?;
            ExpectedArg::for_call(ws, mid, inf, offset, callee, *idx, None)
        });
        let arg_ctx = expected.is_some();
        // Precompute the type-match decision per member (bone's `coerce` on the
        // member's TyId; rendered-string equality only where a TyId is absent --
        // e.g. a generic-substituted field). Sequential because `accepts` takes
        // `&mut` (it memoizes / rolls back trial bindings).
        let field_match: Vec<bool> = fields
            .iter()
            .map(|(_, ty, ty_id)| member_ty_match(expected.as_mut(), *ty_id, ty))
            .collect();
        let method_match: Vec<bool> = methods
            .iter()
            .map(|m| member_ty_match(expected.as_mut(), m.return_ty, m.returns.as_deref().unwrap_or("")))
            .collect();
        // Direct members are 1-hop (dist 1): they carry a sort_text too, so they
        // order consistently with -- and ahead of, by distance -- the transitive
        // reach paths added below (fuzzy score is the dominant term either way).
        let pfx = prefix.as_bytes();
        // In an `if`/`while <chain>` condition, `bool` fields are the likely
        // target -- soft-boost them (score-tie tiebreak only). Mutually exclusive
        // with arg-position reach paths, so the bool-format sort_text is safe to
        // mix only with the field/method items here.
        // Call-arg position (a known expected param type) -- a direct field/method
        // whose type matches that param ranks first. Mutually exclusive with
        // `bool_ctx` (a condition chain isn't inside a call).
        // `_`-prefixed fields are soft-private: hidden from member completion
        // unless the user is explicitly typing an `_` prefix. Fields stay public
        // (no type-check change) -- this is purely a display convention.
        let show_private = prefix.starts_with('_');
        let field_items = fields.into_iter().zip(field_match).filter_map(|((name, ty, _), tmatch)| {
            if name.starts_with('_') && !show_private {
                return None;
            }
            (prefix.is_empty() || clean_match(name.as_bytes(), prefix.as_bytes())).then(|| CompletionItem {
                sort_text: Some(if bool_ctx {
                    member_bool_sort_text(&name, pfx, ty == "bool", 0)
                } else if arg_ctx {
                    member_arg_sort_text(&name, pfx, tmatch, 0, 1)
                } else {
                    arg_sort_text(0, &name, pfx, 1, 0)
                }),
                label: format!("{name}: {ty}"),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(ty),
                insert_text: Some(name),
                ..Default::default()
            })
        });
        let method_items = methods.into_iter().zip(method_match).filter_map(|(m, rmatch)| {
            (prefix.is_empty() || clean_match(m.name.as_bytes(), prefix.as_bytes())).then(|| {
                // Inline `(params) -> ret`. A method with args left after the
                // receiver accepts as a snippet `name($0)` (`recv.name()`, cursor
                // inside); one with no remaining args inserts `name()` as plain
                // text so the cursor lands after the `)`.
                let (insert_text, insert_text_format) = if m.takes_args {
                    (Some(format!("{}($0)", m.name)), Some(InsertTextFormat::SNIPPET))
                } else {
                    (Some(format!("{}()", m.name)), None)
                };
                CompletionItem {
                    sort_text: Some(if bool_ctx {
                        member_bool_sort_text(&m.name, pfx, false, 1)
                    } else if arg_ctx {
                        member_arg_sort_text(&m.name, pfx, rmatch, 1, 1)
                    } else {
                        arg_sort_text(0, &m.name, pfx, 1, 1)
                    }),
                    label: m.name.clone(),
                    label_details: fn_or_type_label_details(SymCat::Function, Some(&m.signature)),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(m.signature),
                    insert_text,
                    insert_text_format,
                    ..Default::default()
                }
            })
        });
        let mut items: Vec<CompletionItem> = field_items.chain(method_items).collect();
        // Entity receiver: `.get()` (-> T?) is the dominant accessor, almost
        // always followed by `.unwrap()`. Rank `get` first, and offer the whole
        // `get().unwrap()` chain (matches a `getu`/`getunwrap` prefix, which the
        // bare `get` method can't since it filters on `starts_with`).
        if recv_struct.is_some_and(|s| s.as_str() == "Entity") {
            for it in items.iter_mut() {
                if it.label == "get" {
                    it.sort_text = Some(sort_text_with_score(0, 0, "get", 9999, 0, 0, false));
                }
            }
            if clean_match(b"get().unwrap()", prefix.as_bytes()) {
                items.push(CompletionItem {
                    label: "get().unwrap()".to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some("unwrap the Entity's value (T)".to_string()),
                    insert_text: Some("get().unwrap()".to_string()),
                    sort_text: Some(sort_text_with_score(0, 0, "get().unwrap()", 9998, 0, 0, false)),
                    ..Default::default()
                });
            }
        }
        // In a call-arg position with a known expected type, also offer
        // TRANSITIVE suffix paths through the receiver -- fields, UFCS method
        // returns, or a MIX (`movable.to_ivec()` when the param wants an IVec2)
        // -- via the reachability index. Multi-hop only: 1-hop fields/methods are
        // already in `field_items`/`method_items` above. The index keys on
        // type-name strings, so the live-built one serves the munged buffer; if
        // it's missing, build a local one from this workspace.
        let reach_targets: &[String] = expected.as_ref().map_or(&[], |e| &e.rendered);
        if !reach_targets.is_empty() {
            if let Some(rs) = recv_struct {
                let local_idx;
                let idx = match reach {
                    Some(r) => r,
                    None => {
                        local_idx = crate::reachability::ReachabilityIndex::build(ws);
                        &local_idx
                    }
                };
                const REACH_CAP: usize = 64;
                let res = idx.paths_to(rs.as_str(), reach_targets, REACH_CAP);
                let pfx = prefix.as_bytes();
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for p in res.paths {
                    if !p.suffix.contains('.') {
                        continue; // 1-hop field/method, already offered above
                    }
                    if !pfx.is_empty() && !clean_match(p.suffix.as_bytes(), pfx) {
                        continue;
                    }
                    if !seen.insert(p.suffix.clone()) {
                        continue;
                    }
                    let (label, kind, insert_text, insert_text_format) = if p.is_method {
                        let call = format!("{}()", p.suffix);
                        let (insert, fmt) = if p.takes_args {
                            (format!("{}($0)", p.suffix), Some(InsertTextFormat::SNIPPET))
                        } else {
                            (call.clone(), None)
                        };
                        (format!("{call} -> {}", p.final_ty), CompletionItemKind::METHOD, insert, fmt)
                    } else {
                        (format!("{}: {}", p.suffix, p.final_ty), CompletionItemKind::FIELD, p.suffix.clone(), None)
                    };
                    // Reach paths are type-matches by construction (paths TO the
                    // expected type) -- prio 0, same format as the direct items.
                    let sort_text = member_arg_sort_text(&p.suffix, pfx, true, u8::from(p.is_method), p.dist);
                    items.push(CompletionItem {
                        label,
                        kind: Some(kind),
                        detail: Some(p.final_ty),
                        insert_text: Some(insert_text),
                        insert_text_format,
                        sort_text: Some(sort_text),
                        ..Default::default()
                    });
                }
            }
        }
        items
    }
}

/// bone's `CompilerError.location` is 0-based in both line and col -- the same
/// convention as LSP -- so it passes through unshifted (a 1-based correction
/// would report every error one line high, masked on line 0 by saturation).
pub fn compiler_error_to_diagnostic(e: &CompilerError) -> Diagnostic {
    // Use the label's span (whole token) when there is one; `col_range` is
    // 0-based/end-exclusive, same as LSP. Fall back to a 1-char range at the
    // location for label-less or zero-width errors.
    let (start, end) = match e.labels.first().map(|l| &l.span) {
        Some(s) if !s.is_empty() => (
            Position::new(s.line_range.0 as u32, s.col_range.0 as u32),
            Position::new(s.line_range.1 as u32, s.col_range.1 as u32),
        ),
        _ => {
            let loc = &e.location;
            (
                Position::new(loc.line as u32, loc.col as u32),
                Position::new(loc.line as u32, loc.col as u32 + 1),
            )
        }
    };
    Diagnostic {
        range: Range::new(start, end),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("bone".to_string()),
        message: e.message.clone(),
        ..Default::default()
    }
}

/// Render a typed-hole report (kind line + `In scope:` block) as plain text.
/// Shared by the `rock-lsp hole-info` CLI handler and the interactive hover
/// handler so both surface identical content for a `?` under the cursor.
fn format_hole_report(hi: &crate::query::HoleInfo) -> String {
    let mut out = String::new();
    match hi.kind {
        crate::query::HoleKind::Type => {
            out.push_str("Type hole");
            if let Some(ty) = &hi.ty {
                out.push_str(": ");
                out.push_str(ty);
            }
        }
        crate::query::HoleKind::Expr => {
            out.push_str("Expression hole");
            if let Some(ty) = &hi.ty {
                out.push_str(" expecting: ");
                out.push_str(ty);
            }
        }
    }
    out.push('\n');
    if hi.in_scope.is_empty() {
        out.push_str("In scope: (nothing)\n");
    } else {
        out.push_str("In scope:\n");
        for (name, ty) in &hi.in_scope {
            if ty.is_empty() {
                out.push_str(&format!("  {name}\n"));
            } else {
                out.push_str(&format!("  {name}: {ty}\n"));
            }
        }
    }
    out
}

/// Unfilled-hole diagnostics for one file: a WARNING per expression hole ("fill
/// me in, expecting T") and a HINT per resolved type hole ("inferred as T").
/// These ride alongside bone's typecheck errors (a file with holes still
/// type-checks), turning every `?` -- hand-written or inserted by a code action
/// -- into a visible, typed TODO that clears when the `?` is replaced. Empty
/// when the file isn't loaded / hasn't been inferred.
/// Whether `BONE_LSP_PROFILE` is set, read once (the result is process-stable).
pub(crate) fn lsp_profile_enabled() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| std::env::var("BONE_LSP_PROFILE").is_ok())
}

/// `Some(diags)` is an AUTHORITATIVE result (parse + inference both succeeded) --
/// an empty vec means "this file genuinely has no holes". `None` means the holes
/// COULD NOT be computed (file untracked / no module / no inference / parse
/// failed); the caller must not cache `None` as a known-hole-free file, or it
/// would later skip recomputing one that became computable. The distinction is
/// what makes the hole-free-dependent skip in `update` sound.
fn hole_diagnostics_for(proj: &BoneProject, path: &str) -> Option<Vec<Diagnostic>> {
    let src = proj.texts.get(path)?;
    let ws = proj.session.workspace();
    let mid = user_module_for_path(ws, path)?;
    let inf = ws.inference(mid)?;
    let uf = proj.session.files.get(path)?;
    let ast = reparse_user_file(uf, path)?;
    let line_index = LineIndex::new(src);
    let diags = crate::query::all_holes(ws, mid, inf, &ast)
        .into_iter()
        .map(|(span, kind, ty)| {
            // Only name the type when it's a useful concrete type, not an
            // unsolved infer var (renders as `hole`/`?N`/`any`) -- "expecting
            // hole" is noise.
            let useful = ty.filter(|t| {
                !t.is_empty()
                    && t != "hole"
                    && t != "any"
                    && !t.starts_with('?')
                    && !t
                        .as_bytes()
                        .windows(2)
                        .any(|w| w[0] == b'?' && w[1].is_ascii_digit())
            });
            let (severity, message) = match kind {
                crate::query::HoleKind::Expr => (
                    DiagnosticSeverity::WARNING,
                    match &useful {
                        Some(t) => format!("unfilled expression hole ? expecting {t}"),
                        None => "unfilled expression hole ?".to_string(),
                    },
                ),
                crate::query::HoleKind::Type => (
                    DiagnosticSeverity::HINT,
                    match &useful {
                        Some(t) => format!("type hole ? inferred as {t}"),
                        None => "unfilled type hole ?".to_string(),
                    },
                ),
            };
            Diagnostic {
                range: line_index.range(src, span.offset_range.0, span.offset_range.1),
                severity: Some(severity),
                source: Some("bone".to_string()),
                message,
                ..Default::default()
            }
        })
        .collect();
    Some(diags)
}

/// Find the user module whose source file is `path` (excludes warm stdlib and
/// the builtin module, whose file ids never match a real path).
fn user_module_for_path(ws: &Workspace, path: &str) -> Option<ModuleId> {
    let want = Ustr::from(path);
    let found = ws.modules.iter().find(|m| m.file == want).map(|m| m.id);
    // TEMP Windows-LSP diagnostic: on a miss, dump the query path vs the
    // module paths the workspace actually holds, so we can see how they
    // diverge (separators / prefix / not-loaded). Gated; remove once fixed.
    if found.is_none() && std::env::var("ROCK_LSP_DEBUG").is_ok() {
        eprintln!("[lsp-debug] user_module_for_path MISS want={want:?}");
        for m in ws.modules.iter().filter(|m| !m.file.as_str().contains("stdlib")) {
            eprintln!("[lsp-debug]   have non-stdlib module file={:?}", m.file);
        }
        eprintln!("[lsp-debug]   ({} total modules)", ws.modules.len());
    }
    found
}

fn completion_kind(k: SymCat) -> CompletionItemKind {
    match k {
        SymCat::Function => CompletionItemKind::FUNCTION,
        SymCat::Struct => CompletionItemKind::STRUCT,
        SymCat::Enum => CompletionItemKind::ENUM,
        SymCat::Variant => CompletionItemKind::ENUM_MEMBER,
        SymCat::Newtype => CompletionItemKind::CLASS,
        SymCat::Global => CompletionItemKind::VARIABLE,
        SymCat::Local | SymCat::Param => CompletionItemKind::VARIABLE,
        SymCat::Field => CompletionItemKind::FIELD,
        SymCat::Keyword => CompletionItemKind::KEYWORD,
    }
}

/// Single-letter extended-ctags kind for a top-level declaration, or None for
/// categories that aren't file-level symbols (locals, params, fields, enum
/// variants, keywords) and so don't belong in a tags file.
fn ctags_kind(k: SymCat) -> Option<char> {
    match k {
        SymCat::Function => Some('f'),
        SymCat::Struct => Some('s'),
        SymCat::Enum => Some('e'),
        SymCat::Newtype => Some('t'),
        SymCat::Global => Some('v'),
        SymCat::Variant | SymCat::Local | SymCat::Param | SymCat::Field | SymCat::Keyword => None,
    }
}

fn symbol_kind(k: SymCat) -> SymbolKind {
    match k {
        SymCat::Function => SymbolKind::FUNCTION,
        SymCat::Struct => SymbolKind::STRUCT,
        SymCat::Enum => SymbolKind::ENUM,
        SymCat::Variant => SymbolKind::ENUM_MEMBER,
        SymCat::Newtype => SymbolKind::CLASS,
        SymCat::Global => SymbolKind::VARIABLE,
        SymCat::Local | SymCat::Param => SymbolKind::VARIABLE,
        SymCat::Field => SymbolKind::FIELD,
        SymCat::Keyword => SymbolKind::KEY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lever A: the call-through partition decomposition must score IDENTICALLY to
    // the full `fuzzy_score(name(arg), q)` for every valid call-through pair (name
    // starts with q[0], name does not contain all of q -- so no +1000 bonus).
    // `combine_scores(prefix_name, suffix_arg)` is the per-(fn,arg) score the loop
    // uses; if it ever diverges from the full DP, the completion output changes.
    #[test]
    fn call_through_partition_matches_full_fuzzy_score() {
        fn full(name: &str, arg: &str, q: &str) -> i32 {
            let cand = format!("{name}({arg})");
            fuzzy_score(cand.as_bytes(), q.as_bytes())
        }
        fn decomposed(name: &str, arg: &str, q: &str) -> i32 {
            let (mut pre, mut suf) = (Vec::new(), Vec::new());
            fuzzy_prefix_scores(name.as_bytes(), q.as_bytes(), &mut pre);
            fuzzy_suffix_scores(arg.as_bytes(), q.as_bytes(), &mut suf);
            combine_scores(&pre, &suf)
        }
        // Real-shaped PASSING pairs (name absorbs k < |q| chars, so no +1000),
        // incl. the interleaving cases that broke the lossy sharing: letter-rich
        // args where the DP could match query chars inside the arg, and reach-path
        // args with dots/parens.
        let cases = [
            ("set_blend_mode", "alpha_threshold", "set_w"),
            ("draw_text_ex", "BANK_PANEL.title", "draw_re"),
            ("stone_get_text_baseline", "outline_detect_shader", "stone_draw_r"),
            ("stone_canvas_save_png", "tile_floor_atlas.full_image_rect().w", "stone_draw_r"),
            ("set_x", "p.pos.x", "set_w"),
            ("stone_measure_text_vec2", "outline_detect_shader", "stone_draw_r"),
            ("draw_circle", "world.rect", "draw_t"),
            ("get_tile_at", "world.grid().origin", "get_tx"),
        ];
        for (name, arg, q) in cases {
            // Sanity: these must be PASSING pairs -- the name absorbs strictly
            // fewer than |q| chars (else the noise guard rejects the fn and the
            // +1000 prefix bonus, which the decomposition omits, would apply).
            let k = prefix_subseq_len(q.as_bytes(), &name.to_lowercase());
            assert!(k < q.len(), "test case not a passing pair: name={name:?} q={q:?} k={k}");
            assert_eq!(
                decomposed(name, arg, q),
                full(name, arg, q),
                "decomposition != full fuzzy for name={name:?} arg={arg:?} q={q:?}"
            );
        }
    }

    // bone's CompilerError.location is 0-based like LSP, so line/col must pass
    // through unshifted. (A 1-based correction would report one line high and
    // saturate to 0 on line 0, hiding the shift.)
    #[test]
    fn diagnostic_passes_zero_based_position_through() {
        let mut e = CompilerError::new_no_location("boom".to_string());
        e.location.line = 2;
        e.location.col = 26;
        let d = compiler_error_to_diagnostic(&e);
        assert_eq!(d.range.start.line, 2);
        assert_eq!(d.range.start.character, 26);
        assert_eq!(d.range.end.character, 27);
    }

    // A labelled error underlines the whole token, not just the start column.
    #[test]
    fn diagnostic_spans_the_full_label() {
        let mut span = Span::unknown();
        span.line_range = (10, 10);
        span.col_range = (7, 15); // `ris_shot`, 8 chars, end-exclusive
        span.offset_range = (100, 108);
        let e = CompilerError::new_with_label(span, "Undefined global `ris_shot`".to_string());
        let d = compiler_error_to_diagnostic(&e);
        assert_eq!((d.range.start.line, d.range.start.character), (10, 7));
        assert_eq!((d.range.end.line, d.range.end.character), (10, 15));
    }

    #[test]
    fn offset_round_trips_through_position() {
        let src = "fn main() {\n  let x = 1\n}\n";
        // byte offset of 'x' on line 1 (0-based), char 6
        let off = offset_at_line_col(src, 1, 6);
        assert_eq!(&src[off..off + 1], "x");
        let pos = offset_to_position(src, off);
        assert_eq!((pos.line, pos.character), (1, 6));
    }

    #[test]
    fn offset_handles_multibyte_before_cursor() {
        // "// \u{e9}x" then cursor on 'x': char index 4, but byte index 5.
        let src = "let s = \"\u{e9}\"\nx";
        let off = offset_at_line_col(src, 1, 0);
        assert_eq!(&src[off..off + 1], "x");
    }

    #[test]
    fn balance_appends_missing_closers() {
        assert_eq!(balance_delimiters("fn f() { g(".to_string()), "fn f() { g()}");
        assert_eq!(balance_delimiters("a[0".to_string()), "a[0]");
    }

    #[test]
    fn balance_ignores_braces_in_line_comments() {
        // The stray `}` in the comment must not cancel the real open brace, and the
        // appended closer must land on a fresh line, not inside the comment.
        let out = balance_delimiters("fn f() {\n  x.foo\n  // a } brace".to_string());
        assert!(out.ends_with("// a } brace\n}"), "got: {out:?}");
    }

    #[test]
    fn balance_closes_strings_and_interpolations() {
        // `{{` is a literal brace -- no nesting; a complete interp is closed.
        assert_eq!(balance_delimiters("let s = \"a {{ b\"".to_string()), "let s = \"a {{ b\"");
        assert_eq!(balance_delimiters("let s = \"a {x} b\"".to_string()), "let s = \"a {x} b\"");
        // A bare `{` in a string OPENS an interpolation (tokenizer rule): an
        // unterminated `"foo {` must close as `}"` or tokenize still fatals.
        assert_eq!(balance_delimiters("let s = \"foo {".to_string()), "let s = \"foo {}\"");
        // An unterminated string is closed before any brace closers.
        assert_eq!(balance_delimiters("fn f() { let s = \"hi".to_string()), "fn f() { let s = \"hi\"}");
        // Unterminated triple-quoted block.
        assert_eq!(
            balance_delimiters("let s = \"\"\"block\ntext".to_string()),
            "let s = \"\"\"block\ntext\"\"\""
        );
    }

    #[test]
    fn munge_keeps_items_after_the_cursor() {
        // Cursor right after `p.`; a later fn must survive in the munge so its
        // UFCS method can be offered.
        let src = "fn use_it(p: P) { p.\n}\nfn dist(p: P) {}\n";
        let off = src.find("p.\n").unwrap() + 2;
        let [m, _] = completion_munges(src, off);
        assert!(m.contains("__rock_completion__"), "got: {m:?}");
        assert!(m.contains("fn dist(p: P)"), "tail dropped: {m:?}");
    }
}
