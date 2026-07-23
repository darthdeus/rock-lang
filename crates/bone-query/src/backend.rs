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
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    DocumentHighlight, NumberOrString,
    DocumentSymbol, Hover,
    HoverContents, InlayHint, InlayHintKind, InlayHintLabel, Location, MarkupContent, MarkupKind,
    ParameterInformation, ParameterLabel, Position, Range, SemanticToken, SemanticTokenModifier,
    SemanticTokenType,
    SemanticTokens, SignatureHelp, SignatureInformation, SymbolInformation, SymbolKind, TextEdit,
    Uri, WorkspaceEdit,
};

use bone_lints::LintFinding;
use bone_semantic::inference::{self, Gamma};
use bone_semantic::prelude::{BodyInference, InferenceResult, ModuleId, Span, TyId, Workspace};
use bone_incremental::session::{Session, UpdateTier};
use skull_parser::CompilerError;
use ustr::Ustr;

use crate::completion::classify::*;
use crate::completion::rank::*;
use crate::completion::select::*;
use crate::completion::sources::*;
use crate::completion::match_policy::admit;
use crate::query::{clean_match, SymCat};
use crate::rank_key::RankKey;

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

/// Canonical on-disk path of a stdlib file, keyed exactly like a project file
/// (`canon`/`root_for` produce the same form), so the warm module a user opens
/// resolves to the SAME `ModuleId` -- no duplicate, no self-collision -- and
/// source-based queries find the stdlib in `texts`.
fn stdlib_path(name: &str) -> String {
    canonicalize_lenient(&rock_host::stdlib_loader::stdlib_dir().join(name).to_string_lossy())
}

/// Stdlib sources keyed by canonical path, seeded into every project's `texts`
/// so default-value slicing, `workspace_symbols`, and span->text see the stdlib
/// like any other file.
fn stdlib_texts() -> Vec<(String, String)> {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| (stdlib_path(f.name), f.contents.to_string()))
        .collect()
}

/// The real stdlib, parsed through skull-parser (bone's front end) keyed by each
/// file's REAL canonical path. Opening a stdlib buffer then interns to the same
/// `ModuleId` as the warm copy (`warm_mids` still distinguishes warm from user
/// code for lints/ctags policy), so it's navigable and diagnosable without a
/// second colliding module. Parsing with the path stamps every span, so bone's
/// `ModuleId` (the first item's `span.file`) lands on it directly. Parsed once.
fn warm_stdlib() -> Vec<skull_parser::Ast> {
    static WARM: OnceLock<Vec<skull_parser::Ast>> = OnceLock::new();
    WARM.get_or_init(|| {
        rock_host::stdlib_loader::stdlib_files()
            .iter()
            .filter_map(|f| {
                let id = Ustr::from(stdlib_path(f.name).as_str());
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
    /// Workspace reference counts per name (`build_freq_index`) -- the
    /// `freq` prior. Rebuilt with the other type caches.
    freq: Option<HashMap<ustr::Ustr, u32>>,
    /// Per-file unfilled-hole diagnostics, cached across edits. `hole_diagnostics_for`
    /// REPARSES a file, so recomputing it for every tracked file on every keystroke
    /// reparses the whole compilation unit (~130 files on mag) when only the edited
    /// file + its reinferred dependents can have changed holes. Invalidated for
    /// exactly those files in `update`.
    hole_cache: HashMap<String, Vec<Diagnostic>>,
    /// Per-file `unused_var` lint diagnostics, cached across edits. Unlike
    /// holes, the lint never reads inference, so a file's findings change ONLY
    /// when that file's own text changes -- recompute the edited file, serve
    /// the cache for every other tracked file. Gives whole-project coverage
    /// cheaply. Invalidated for the edited file in `update`, dropped on remove.
    lint_cache: HashMap<String, Vec<Diagnostic>>,
    /// Per-file `transmute_of_nullable` lint diagnostics, cached across edits.
    /// This lint READS inference (arg/result nullability), so unlike
    /// `lint_cache` a finding can change when a *dependency* edit re-infers this
    /// file even though its own text is unchanged -- a return type flipping to
    /// `int?` newly strips at an untouched `cast` site. So it invalidates on the
    /// hole-cache rule (edited file + reinferred dependents = `changed`), never
    /// the text-only rule; and a finding-free dependent can gain findings, so
    /// (unlike holes) an empty cache entry grants no skip.
    transmute_cache: HashMap<String, Vec<Diagnostic>>,
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
    /// Whether this project is a `bin` (executable/game), from its `stick.toml`
    /// `type` (default `bin`). The `unused_fn` lint is closed-world -- a `lib`'s
    /// public API has no in-project caller yet isn't dead -- so it runs only
    /// when this is true. Computed once at load, not per keystroke.
    is_bin: bool,
    /// Last time each module was (re)inferred, keyed by module file. Stamped for
    /// every file at load and for the `update.reinferred` set on each edit, so
    /// `rock/modules` can render "how long ago did this recompile" -- the
    /// incremental-perf watch view. Modules never yet touched are absent.
    module_touched: HashMap<String, std::time::Instant>,
}

/// One module's row for the `rock/modules` snapshot: its id (file path) and how
/// long ago it last (re)inferred, `None` if never touched this session.
#[derive(Clone, Debug)]
pub struct ModuleRecency {
    pub id: String,
    pub ago_ms: Option<u64>,
}

/// True iff the deepest `stick.toml` at or above `root` is a `bin` package
/// (the default when `type` is absent). False for a `lib`, or when no
/// `stick.toml` exists (a standalone scratch file is not a closed world).
fn stick_is_bin(root: &Path) -> bool {
    matches!(
        rock_host::stick::StickConfig::find_from_dir(root),
        Ok(Some((_, cfg))) if cfg.package.package_type() == rock_host::stick::PackageType::Bin
    )
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

/// The "fill struct fields" edit: a text replacement that inserts each missing
/// required field -- shorthand `field` when an in-scope binding matches its name
/// and type, else a `field: ?` hole -- and how many fields it adds.
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

/// Whether the initializer SOURCE has a binary/unary operator at bracket depth 0
/// (outside strings) -- e.g. a constant-folded `1 + 2`. Postfix `.`/`::`/`?.` and
/// operators inside a call/index/literal don't count, so `g(v)` / `a.b` / `S{..}`
/// stay unparenthesized while `a + b` / `-x` / `a as int` get wrapped.
fn text_has_top_level_operator(s: &str) -> bool {
    let b = s.trim().as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'!' | b'<' | b'>' | b'&' | b'|' | b'^'
                if depth == 0 =>
            {
                return true
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// The "inline local" edit set: replace every use of a `let` binding with its
/// initializer and delete the `let`. `var` names the binding (for the action
/// title); `edits` are non-overlapping `TextEdit`s (uses + the `let` deletion).
pub struct InlineLocalEdits {
    pub var: String,
    pub edits: Vec<TextEdit>,
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
        let is_bin = stick_is_bin(root);
        self.projects
            .entry(root.to_path_buf())
            .or_insert_with(|| BoneProject {
                // Warm-only session is always valid even if the first file
                // doesn't parse; the file is added by the update_file below.
                session: Session::new(warm_stdlib(), Vec::new())
                    .expect("bone session warm-init (stdlib) must succeed"),
                texts: stdlib_texts().into_iter().collect(),
                freq: None, hole_cache: HashMap::new(), lint_cache: HashMap::new(),
                transmute_cache: HashMap::new(),
                reach: None,
                fn_sigs: None,
                call_through: None,
                is_bin,
                module_touched: HashMap::new(),
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

    /// Per-project module summary for the `rock/modules` introspection request:
    /// `(root, sorted [ModuleRecency])` for every indexed project, each module
    /// carrying how long ago it last (re)inferred. The LSP serves one root per
    /// server, but a session can hold several (scratch files keyed by parent
    /// dir), so this returns all of them as separate contexts.
    pub fn projects_summary(&self) -> Vec<(String, Vec<ModuleRecency>)> {
        let now = std::time::Instant::now();
        let mut out: Vec<(String, Vec<ModuleRecency>)> = self
            .projects
            .iter()
            .map(|(root, proj)| {
                let mut mods: Vec<ModuleRecency> = proj
                    .texts
                    .keys()
                    .map(|id| ModuleRecency {
                        id: id.clone(),
                        ago_ms: proj
                            .module_touched
                            .get(id)
                            .map(|t| now.duration_since(*t).as_millis() as u64),
                    })
                    .collect();
                mods.sort_by(|a, b| a.id.cmp(&b.id));
                (root.display().to_string(), mods)
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
        let is_bin = stick_is_bin(&root);
        match Session::new(warm_stdlib(), files.clone()) {
            Ok(session) => {
                let texts: HashMap<String, String> =
                    stdlib_texts().into_iter().chain(files).collect();
                // Baseline every loaded file at load time, so `ago_ms` is
                // meaningful for modules not yet re-inferred by an edit.
                let now = std::time::Instant::now();
                let module_touched = texts.keys().map(|k| (k.clone(), now)).collect();
                self.projects.insert(
                    root,
                    BoneProject { session, texts, freq: None, hole_cache: HashMap::new(), lint_cache: HashMap::new(), transmute_cache: HashMap::new(), reach: None, fn_sigs: None, call_through: None, is_bin, module_touched },
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
                let touched: Vec<String> = upd
                    .reinferred
                    .iter()
                    .map(|(mid, _)| ws.module(*mid).file.as_str().to_string())
                    .collect();
                let now = std::time::Instant::now();
                for file in touched {
                    proj.module_touched.insert(file.clone(), now);
                    changed.insert(file);
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
            proj.freq = Some(build_freq_index(proj.session.workspace()));
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
            // One squiggle per underlined label, each filed under its own label's
            // file (labels may be cross-file).
            for (file, diag) in compiler_error_to_diagnostics(&e) {
                let file = file.as_str();
                // Errors with no/synthetic location are attributed to the edited file.
                let key = if file.is_empty() || !proj.texts.contains_key(file) {
                    path.to_string()
                } else {
                    file.to_string()
                };
                by_file.entry(key).or_default().push(diag);
            }
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

        // `unused_var` lint diagnostics. A file's findings depend only on its
        // own body text (the lint never reads inference), so recompute only the
        // edited file and serve the cache for every other tracked file --
        // whole-project coverage without re-walking every body per keystroke.
        for (p, diags) in by_file.iter_mut() {
            // Broken edited file: it shows only its parse error, and the session
            // holds stale bodies for it -- drop the cache, append no lints.
            if p == path && this_file_parse_err.is_some() {
                proj.lint_cache.remove(p);
                continue;
            }
            let lints = if p == path || !proj.lint_cache.contains_key(p) {
                let v = lint_diagnostics_for(proj, p);
                proj.lint_cache.insert(p.clone(), v.clone());
                v
            } else {
                proj.lint_cache.get(p).cloned().unwrap_or_default()
            };
            diags.extend(lints);
        }

        // `transmute_of_nullable` lint diagnostics. Reads inference, so a
        // finding can change in any re-inferred file (`changed` = edited file +
        // reinferred dependents), not just the edited one -- recompute those,
        // serve the cache for everything else. A finding-free entry grants no
        // skip: a dependency edit can newly strip at an untouched call site.
        for (p, diags) in by_file.iter_mut() {
            if p == path && this_file_parse_err.is_some() {
                proj.transmute_cache.remove(p);
                continue;
            }
            let must_recompute = changed.contains(p.as_str()) || !proj.transmute_cache.contains_key(p);
            let lints = if must_recompute {
                let v = transmute_diagnostics_for(proj, p);
                proj.transmute_cache.insert(p.clone(), v.clone());
                v
            } else {
                proj.transmute_cache.get(p).cloned().unwrap_or_default()
            };
            diags.extend(lints);
        }

        // `unused_fn` lint (bin projects only). Unlike `unused_var`, reachability
        // is whole-project: a finding in ANY file can flip from an edit ANYWHERE
        // (adding or removing a call keeps or kills a function), so there is no
        // per-file cache -- recompute the whole workspace each edit and
        // distribute by file. A `lib`'s public API has external callers we can't
        // see, so the lint would false-flag it; hence the `is_bin` gate.
        if proj.is_bin {
            let by_fn = bone_lints::unused_fns_by_file(
                proj.session.workspace(),
                &proj.session.warm_mids,
            );
            for (p, diags) in by_file.iter_mut() {
                if p == path && this_file_parse_err.is_some() {
                    continue;
                }
                let Some(findings) = by_fn.get(&Ustr::from(p.as_str())) else {
                    continue;
                };
                let src = proj.texts.get(p).map(String::as_str).unwrap_or("");
                let kept = bone_lints::filter_suppressed(findings.clone(), src);
                diags.extend(kept.iter().map(lint_finding_diagnostic));
            }
        }

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
            proj.lint_cache.remove(path);
            proj.transmute_cache.remove(path);
            let _ = proj.session.remove_file(path);
        }
    }

    /// Current text of `path` as this workspace sees it -- the editor-buffer
    /// view the last `update` applied, not the disk copy. Recorder/debug
    /// surface (the completion corpus records the exact text it ranked).
    pub fn file_text(&self, path: &str) -> Option<&str> {
        let path_c = self.canon(path);
        self.project(path_c.as_str())?
            .texts
            .get(path_c.as_str())
            .map(|s| s.as_str())
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
            .or_else(|| crate::query::hover_at(ws, mid, inf, offset, Some(&proj.texts)))
            .or_else(|| crate::query::fn_signature_hover(ws, mid, src, offset, Some(&proj.texts)))?;
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
        // Expand to one diagnostic per label, then keep those whose label lands
        // in this file (a multi-label error's labels may span files).
        let mut diags: Vec<Diagnostic> = proj
            .session
            .diagnostics()
            .into_iter()
            .flat_map(|e| compiler_error_to_diagnostics(&e))
            .filter(|(file, _)| file.as_str() == path || file.as_str().is_empty())
            .map(|(_, d)| d)
            .collect();
        diags.extend(hole_diagnostics_for(proj, path).unwrap_or_default());
        diags.extend(self.lint_diagnostics(path));
        diags
    }

    /// Only the bone LINT findings for a single file (warnings) -- `unused_var`,
    /// plus `unused_fn` for a `bin` project. The lint-only counterpart to
    /// `diagnostics_for`, for the `rock lsp lint` command; excludes typecheck
    /// errors and holes.
    pub fn lint_diagnostics(&self, path: &str) -> Vec<Diagnostic> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Vec::new();
        };
        let mut diags = lint_diagnostics_for(proj, path);
        diags.extend(transmute_diagnostics_for(proj, path));
        if proj.is_bin {
            diags.extend(fn_lint_diagnostics_for(proj, path));
        }
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
        // Enum-variant match patterns resolve from the raw AST FIRST (the HIR
        // pattern node isn't in `node_index`), mirroring the hover path.
        let pat_def = proj
            .session
            .files
            .get(path)
            .and_then(|uf| reparse_user_file(uf, path))
            .and_then(|ast| crate::query::enum_variant_pattern_def(ws, &ast, offset));
        let span = pat_def.or_else(|| gaps::definition(ws, mid, inf, offset))?;
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
        // Find-usages scopes to the specific overload under the cursor.
        self.all_reference_spans(proj, path, offset, crate::query::FnRefScope::ThisOverload)
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
    /// `Name { ... }` missing required fields. For each missing field inserts the
    /// shorthand `field` when an in-scope binding matches its name and type, else
    /// a `field: ?` hole, keeping the literal's single- or multi-line shape, or
    /// None when nothing's missing. See `query::struct_fields_fill`.
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

        // `struct_fields_fill` already returns ready field snippets -- a bare
        // `field` when an in-scope binding matches (shorthand), else `field: ?`.
        let fields: Vec<String> = missing.clone();

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

    /// The "inline local" edit set when the cursor is on a `let x = <init>`
    /// binding or one of its uses: replace every use with the initializer (parens
    /// added when it isn't atomic) and delete the `let` line. None otherwise.
    /// See `query::inline_local`.
    pub fn inline_local(&self, path: &str, pos: Position) -> Option<InlineLocalEdits> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let proj = self.project(path)?;
        let src = proj.texts.get(path)?;
        let offset = offset_at_line_col(src, pos.line, pos.character);
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let sites = crate::query::inline_local(ws, mid, offset)?;

        let (istart, iend) = sites.init_span.offset_range;
        let init_text = src.get(istart..iend)?;
        // The HIR flag catches structural non-primaries (if/match/lambda/cast/
        // range). A constant-folded literal (`1 + 2` -> `Int(3)`) reports atomic
        // but its SOURCE still has a top-level operator, so back it with a text
        // scan; either signal means splicing needs parens to keep precedence.
        let parenthesize = sites.parenthesize || text_has_top_level_operator(init_text);
        let replacement =
            if parenthesize { format!("({init_text})") } else { init_text.to_string() };

        let mut edits = Vec::with_capacity(sites.uses.len() + 1);
        for u in &sites.uses {
            let (s, e) = u.offset_range;
            edits.push(TextEdit {
                range: Range {
                    start: offset_to_position(src, s),
                    end: offset_to_position(src, e),
                },
                new_text: replacement.clone(),
            });
        }
        // Delete the whole `let` line: leading indent through the trailing newline.
        let (ls, le) = sites.let_span.offset_range;
        let line_start = src.get(..ls)?.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = src.get(le..)?.find('\n').map(|i| le + i + 1).unwrap_or(src.len());
        edits.push(TextEdit {
            range: Range {
                start: offset_to_position(src, line_start),
                end: offset_to_position(src, line_end),
            },
            new_text: String::new(),
        });

        Some(InlineLocalEdits { var: sites.var.to_string(), edits })
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
        fn_scope: crate::query::FnRefScope,
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
        // Text lookup for the walks whose HIR carries no token span (named-arg
        // labels, `?.method` names) -- `proj.texts` holds every tracked file.
        gaps::references(
            ws,
            mid,
            inf,
            offset,
            &|f| proj.texts.get(f).map(|s| s.as_str()),
            fn_scope,
        )
    }

    // -- Feature: completion (stubbed: crate::query::completion) ---------------------

    pub fn completion(&mut self, path: &str, pos: Position) -> Vec<CompletionItem> {
        self.completion_with_trigger(path, pos, false)
    }

    /// Completion, told whether this request was auto-fired by the space trigger
    /// character. A space trigger on a line whose prefix (up to the cursor) is
    /// only whitespace is auto-indentation inserted by `<CR>`, not a real
    /// "space after a token" context (`let x = |`); we suppress it so a bare
    /// newline doesn't pop the menu (and `<CR><CR>` doesn't accept item 1).
    /// Manual invokes / identifier typing pass `false` and are never suppressed.
    pub fn completion_with_trigger(
        &mut self,
        path: &str,
        pos: Position,
        space_triggered: bool,
    ) -> Vec<CompletionItem> {
        let mut items = self.completion_with_trigger_impl(path, pos, space_triggered);
        // Reconcile every insert with the buffer text after the cursor: a dotted
        // reach insert (`PANEL.rect()`) accepted over an existing `.rect()` tail
        // must not double it. Done once here so it covers every return path
        // (member/enum/struct/reach), off the same canon path + offset the impl used.
        let path_c = self.canon(path);
        if let Some(proj) = self.projects.get(&Self::root_for(path_c.as_str())) {
            if let Some(src) = proj.texts.get(path_c.as_str()) {
                let offset = offset_at_line_col(src, pos.line, pos.character);
                let tail = member_tail(src, offset);
                reconcile_suffix(&mut items, tail);
            }
        }
        items
    }

    fn completion_with_trigger_impl(
        &mut self,
        path: &str,
        pos: Position,
        space_triggered: bool,
    ) -> Vec<CompletionItem> {
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
        // Space-triggered on an all-whitespace line prefix == `<CR>` auto-indent,
        // not a real space-after-token context: suppress the popup.
        if space_triggered && line_prefix_is_blank(src, offset) {
            return Vec::new();
        }
        // Inside a `// rock: ...` magic comment: a separate early-exit context
        // (the `allow` verb / known lint names), NOT a name-gate through `admit`.
        // A comment is not code, so generic symbol/type completion is
        // short-circuited here.
        if let Some(items) = magic_comment_completions(src, offset) {
            return items;
        }
        let prefix_raw = ident_prefix(src, offset).to_string();
        // A token starting with a digit is a numeric literal, never an identifier
        // being typed (Rock idents are `[A-Za-z_][A-Za-z0-9_]*`) -- don't
        // subsequence-match it against symbols (`0` -> `remap_to_01`).
        if prefix_raw.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            return Vec::new();
        }
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
        // The cursor classification, computed ONCE (CompletionCx: the AST
        // classifier over the munge ladder plus every context consultation;
        // docs/cursor-context-plan.md + completion-v2-design.md M2b). The
        // dispatch below tries the slots most-specific-first; a `recv.field`
        // even inside a struct-literal field value is member completion.
        let cx = classify(src, offset);
        // Member access `recv.<cursor>`, one dispatch for every receiver shape.
        // In a call-arg position this also surfaces TRANSITIVE suffix paths
        // whose deep type matches the expected param (`recv.movable.grid`); the
        // typed session is built INSIDE the variant (member TyIds live in that
        // workspace), so here we just pass the enclosing (callee, arg index).
        if let Some(recv_end) = cx.member_recv_end {
            let arg_slot = cx.arg_call.as_ref().map(|(callee, idx, _)| (callee.clone(), *idx as usize));
            // PRIMARY: resolve the receiver's type via inference (`type_at`).
            // Handles generic-substituted fields, tuple/newtype projections, and
            // call/index receivers the structural name-walk can't express. `None`
            // means the receiver isn't a members-bearing type (`scale(x)` -> int).
            let by_recv = member_completions_by_receiver(
                &mut proj.session, path, src, offset, recv_end, &prefix, arg_slot.clone(), cx.anchor,
                proj.reach.as_ref(), cx.bool_chain,
            );
            let resolved = by_recv.is_some();
            let mut items = by_recv.unwrap_or_default();
            // FALLBACK: a pure-ident receiver whose type inference can't see --
            // the enclosing call is to an unknown fn (`newHelper(thing.|`), or a
            // parse error dropped the statement. The name-walk resolves the
            // receiver by binding lookup, no inference needed.
            if items.is_empty() {
                if let Some(segments) = cx.member.clone() {
                    items = member_completions(
                        &mut proj.session, path, src, offset, &segments, &prefix, arg_slot,
                        cx.anchor, proj.reach.as_ref(), cx.bool_chain,
                    );
                }
            }
            // This is member access: `recv.` never completes to a bare global
            // (Rock has no namespaces). Suppress general completion whenever the
            // receiver resolved to a members-bearing type (even if the prefix
            // matched none -- offering globals would be invalid UFCS suggestions)
            // or it's a name path. Only a receiver that ISN'T a members type
            // (`scale(x).|` -> int) falls through to general.
            if resolved || cx.member.is_some() || !items.is_empty() {
                if next_is_open_paren {
                    strip_call_template(&mut items);
                }
                path_ms("member path", items.len());
                return items;
            }
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
        if let Some((callee, arg_idx, _)) = cx.arg_call.clone() {
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
        if let Some((enum_name, variant)) = cx.enum_record.clone() {
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
        if let Some(enum_name) = cx.qualified.clone() {
            let mut items = enum_qualified_completions(
                &mut proj.session, path, src, offset, &enum_name, &prefix,
            );
            if next_is_open_paren {
                strip_call_template(&mut items);
            }
            path_ms("enum-qualified path", items.len());
            return items;
        }
        if let Some((struct_name, brace_open)) = cx.struct_literal.clone() {
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
        if cx.decorator {
            let items = decorator_completions(proj.session.workspace(), &prefix);
            path_ms("decorator path", items.len());
            return items;
        }

        // Scope + global namespace needs a workspace with the user module.
        // Prefer the live session; but if the buffer is broken so the strict
        // session dropped the module, reparse it with RECOVERY so scope
        // completion still works mid-edit (backlog #21).
        // PatternPos: a match-arm pattern HEAD is a slot for the scrutinee's
        // enum. Resolve the scrutinee type at the classifier's anchor (through
        // the variant munges -- the half-typed head usually breaks the live
        // parse); when it names an enum, its variants take the type rung and
        // reach/call-through synths (not valid patterns) stay out. Fails open:
        // an unresolved scrutinee or a non-enum match changes nothing.
        let pattern_pos = cx.pattern_scrutinee.is_some();
        let pattern_enum: Option<String> = cx.pattern_scrutinee.and_then(|a| {
            pattern_scrutinee_enum(&mut proj.session, path, src, offset, a)
        });
        let live = proj.session.workspace();
        let fallback_ws: Option<Workspace> = if user_module_for_path(live, path).is_none() {
            let t = std::time::Instant::now();
            // A half-typed arm HEAD (`match s { co`) is what broke the parse:
            // complete it (` => {}` at the cursor) so the recovery reparse
            // keeps the match -- and its scrutinee, which the pattern rung
            // below resolves -- instead of dropping the statement.
            let fixed = if pattern_pos {
                let mut s = String::with_capacity(src.len() + 8);
                s.push_str(&src[..offset.min(src.len())]);
                s.push_str(" => {}");
                s.push_str(&src[offset.min(src.len())..]);
                s
            } else {
                src.to_string()
            };
            let ws = proj.session.combined_unexpanded().ok().and_then(|combined| {
                reparse_with_project(&combined, path, &balance_delimiters(fixed))
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
        let expr_position = cx.expr_position;
        // After a `yield`/`await`, coroutine-returning fns rank first (the
        // `await frame(1)` delegation pattern).
        let after_yield = cx.after_yield;
        // Test-scoped symbols demote outside test files (kind bucket 4).
        let demote_tests = !crate::query::is_test_path(path);
        // Proximity tier of a defining file vs the request (late prior:
        // 0 same file, 1 same project root, 2 other workspace code,
        // 3 stdlib/builtins), memoized per file per request.
        let root = Self::root_for(path).to_string_lossy().into_owned();
        let mut prox_memo: HashMap<ustr::Ustr, u8> = HashMap::new();
        let mut prox_of = move |def_file: ustr::Ustr| -> u8 {
            *prox_memo.entry(def_file).or_insert_with(|| {
                let f = def_file.as_str();
                if f == path {
                    0
                } else if f.starts_with('<') || f.split(['/', '\\']).any(|s| s == "stdlib") {
                    3
                } else if f.starts_with(&root) {
                    1
                } else {
                    2
                }
            })
        };
        // ForIn: `for x in |` wants an iterable. Local candidate TyIds may
        // index the cursor body's BodyInference, so resolve it once for the
        // `is_iterable_ty` checks in the ranking loop.
        let for_in = cx.for_in;
        let cursor_bi = for_in
            .then(|| {
                ws.modules
                    .get(mid.index())
                    .and_then(|m| m.body_at(offset))
                    .and_then(|o| inf.per_fn.get(&o))
            })
            .flatten();
        // Param-name + fn-prefix type heuristic (param TYPE slot only): a param
        // named `e`/`entity` suggests the (generic) `Entity` type; the enclosing
        // fn's name prefix picks the payload (`mob_attack_co` -> `Mob` ->
        // `Entity<Mob>`). With no inferable payload the suggestion is the snippet
        // `Entity<$0>` (cursor parked in the generic slot), never bare `Entity`.
        let mut entity_hint = false;
        let mut entity_payload: Option<Ustr> = None;
        {
            if let Some((pn, fn_name)) = cx.param_type.clone() {
                if is_entity_param_name(&pn) {
                    entity_hint = true;
                    entity_payload = fn_prefix_struct(ws, &fn_name);
                }
            }
        }
        // The prefix filter happens inside `crate::query::completion`, before per-candidate
        // detail rendering -- so typing narrows the work, not just the result.
        let t_cands = std::time::Instant::now();
        let mut cands = crate::query::completion(ws, mid, inf, offset, &prefix, expr_position);
        // Macros are expanded away before bone lowering, so `ws` has no record
        // of them -- without this every macro in the tree is invisible to
        // completion. The Session's parse-time capture is the only source (the
        // same one ctags and semantic tokens read). Value-position only: a macro
        // invocation is an expression, never a type.
        if expr_position {
            let show_private = prefix.starts_with('_');
            let pfx = prefix.as_bytes();
            let mut already: std::collections::HashSet<Ustr> =
                cands.iter().map(|c| c.name).collect();
            for (def_file, md) in proj.session.all_macro_defs() {
                let name = md.name;
                if name.as_str().starts_with('_') && !show_private {
                    continue;
                }
                if !crate::completion::match_policy::admit(name.as_str().as_bytes(), pfx) {
                    continue;
                }
                // Overloaded macros (`log_once` has a 2- and a 3-param form)
                // share one name -- offer the first, like an overloaded fn.
                if !already.insert(name) {
                    continue;
                }
                let params: Vec<String> = md
                    .params
                    .iter()
                    .map(|p| {
                        let variadic = if p.is_variadic { "..." } else { "" };
                        format!("{}: {}{}", p.name, variadic, p.ty)
                    })
                    .collect();
                let ret = md.ret.as_ref().map_or_else(String::new, |r| format!(" -> {r}"));
                cands.push(crate::query::CompletionEntry {
                    name,
                    kind: SymCat::Macro,
                    detail: Some(format!("macro {name}({}){ret}", params.join(", "))),
                    ty: None,
                    takes_params: !md.params.is_empty(),
                    variant_shape: None,
                    test_scoped: crate::query::is_test_path(def_file.as_str())
                        || name.as_str().starts_with("test_"),
                    def_file,
                    rec: 0,
                });
            }
        }
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
        let mut expected = cx.expected(ws, mid, inf, offset);
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
        // The fn whose body holds the cursor. Inside its own body it must not be
        // PROMOTED as a way to produce a value -- a call-through autofill, a reach
        // path, or a fn-returning-the-slot-type -- because that is a recursive
        // self-call, which buries the direct constructor the user is naming
        // (mag blood.rock:16:39). It stays offered (plain candidate) for genuine
        // recursion, just not lifted above type-directed candidates.
        let enclosing_fn: Option<Ustr> = ws.modules.get(mid.index()).and_then(|m| {
            match m.body_at(offset) {
                Some(bone_semantic::hir::BodyOwner::Function(fid)) => Some(m.functions[fid].name),
                _ => None,
            }
        });
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
        // Frequency prior: workspace ref counts, log-bucketed. Locals ride
        // the recency rung instead (uniform 0 here).
        let freq_owned;
        let freq: &HashMap<ustr::Ustr, u32> = match proj.freq.as_ref() {
            Some(f) => f,
            None => {
                freq_owned = build_freq_index(ws);
                &freq_owned
            }
        };
        let freq_bucket = |n: u32| -> u8 {
            if n >= FREQ_HOT {
                0
            } else if n >= FREQ_WARM {
                1
            } else {
                2
            }
        };
        // Rank EVERY candidate (full accuracy) as a fact-shaped `Cand`, then
        // materialize a CompletionItem only for the post-cap survivors
        // (`materialize_cand`). The rank is a `RankKey` (rank_key.rs) whose
        // `Ord` IS the `sort_text` encoding, so the O(n) select and the
        // final string sort cannot disagree. Ranking allocates NOTHING per
        // candidate: `fuzzy_score` lowercases the name inline and the tiebreak
        // compares names case-insensitively inline (`ci_cmp`). Only the
        // sort_text string, the fn signature `detail`,
        // `insert_text`/`label_details`, and the `CompletionItem` itself are
        // built for the ~200 we keep, not the ~2500 we cap away. `type_rank` is
        // the typed ladder (0 = var/param/global/variant of the expected type,
        // 1 = fn returning it, 2 = else; all 0 when no type is inferable) and
        // `kind` the locality ladder -- primary bucket without a type, the
        // post-band tiebreak with one.
        let q = prefix.as_bytes();
        let q_raw = prefix_raw.as_bytes();
        let mut general: Vec<Cand> = cands
            .into_iter()
            .map(|c| {
                // Locality/kind ladder, always computed: variables first
                // (locals/params, then globals), then functions / structs /
                // bare enums / newtypes, then enum-member VARIANTS last. In a
                // typed slot it is the post-band tiebreak (a right-type local
                // beats a right-type global); with no expected type it is the
                // primary kind bucket. Structs sit in bucket 2 (NOT below
                // functions): on an EMPTY prefix the cap keeps the best
                // `max_items`, and burying types under every stdlib
                // fn would truncate them away entirely; within bucket 2 the
                // `method` tiebreak already orders non-fns ahead of functions.
                let kind = match c.kind {
                    // Test-scoped symbols (tests/-dir or test_-named) sink to
                    // the bottom kind bucket OUTSIDE test files; the band rung
                    // still leads, so an explicitly typed `test_` prefix
                    // surfaces them (docs/completion-v2-design.md heuristics).
                    _ if demote_tests && c.test_scoped => 4,
                    SymCat::Local | SymCat::Param => 0,
                    // After `yield`/`await`, a coroutine fn is the expected
                    // operand -- promote it above plain fns/types.
                    SymCat::Function if after_yield && fn_is_coroutine(ws, c.name.as_str()) => 0,
                    SymCat::Global => 1,
                    SymCat::Variant => 3,
                    _ => 2,
                };
                let type_rank = if let Some(pe) = pattern_enum.as_deref() {
                    // Pattern slot: only a variant of the scrutinee's enum IS
                    // a pattern of the expected type (locals/fns don't pattern-
                    // match; they stay offered but below, fail-open). The
                    // variant's enum name rides its `detail`, same fact the
                    // typed-slot rung reads.
                    match c.kind {
                        SymCat::Variant if c.detail.as_deref() == Some(pe) => 0,
                        _ => 2,
                    }
                } else if for_in {
                    // ForIn slot: an ITERABLE value (bone's `for_elem_ty`
                    // judgment, mirrored in `is_iterable_ty`) rides rung 0,
                    // a fn returning one rung 1 -- the expected-type ladder
                    // with a type PREDICATE instead of a single slot type.
                    match c.kind {
                        SymCat::Local | SymCat::Param | SymCat::Global
                            if c.ty.is_some_and(|t| is_iterable_ty(ws, cursor_bi, t)) =>
                        {
                            0
                        }
                        SymCat::Function
                            if fn_unique_return_ty(ws, c.name.as_str())
                                .is_some_and(|t| is_iterable_ty(ws, None, t)) =>
                        {
                            1
                        }
                        _ => 2,
                    }
                } else if let Some(exp) = expected.as_mut() {
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
                        SymCat::Function
                            if enclosing_fn.as_deref() != Some(c.name.as_str())
                                && exp.accepts_fn_returning(ws, c.name.as_str()) =>
                        {
                            1
                        }
                        _ => 2,
                    }
                } else {
                    // No expected type: the kind ladder is the whole bucket.
                    0
                };
                let method = u8::from(c.kind == SymCat::Function);
                let name = c.name.as_str();
                // Smart-case leading rank, NON-arg path only: measure against the
                // variant NAME (the user types it; `Enum::` uppercasing would
                // skew it). In arg position (and the pattern / for-in slots)
                // the expected-TYPE bucket must stay dominant over case, so
                // leave case neutral (0) there.
                let case_rank = if !has_expected && pattern_enum.is_none() && !for_in {
                    let case_target =
                        if c.kind == SymCat::Variant { name.rsplit("::").next().unwrap_or(name) } else { name };
                    case_mismatch_rank(case_target.as_bytes(), q_raw)
                } else {
                    0
                };
                let (score, lead) = if c.kind == SymCat::Variant {
                    // The user types the variant NAME (`nog` -> `NotGround`);
                    // score against it, but keep the qualified score too so a
                    // `tarnog` (Enum + variant) query still ranks.
                    let vname = name.rsplit("::").next().unwrap_or(name);
                    let s = fuzzy_score(vname.as_bytes(), q).max(fuzzy_score(name.as_bytes(), q));
                    let l = leading_run_len(vname.as_bytes(), q).max(leading_run_len(name.as_bytes(), q));
                    (s, l)
                } else {
                    (fuzzy_score(name.as_bytes(), q), leading_run_len(name.as_bytes(), q))
                };
                // A leading contiguous run (you typed the start of the name)
                // beats a scattered all-boundary match on a longer name -- the
                // fine score alone over-rewards boundary-rich names
                // (docs/completion-ranking-design.md Divergence 1). Only reorders
                // within a band; well below fuzzy_score's whole-prefix +1000.
                let score = score + (lead as i32) * 100;
                // Coarse match band + exact-name flag: classified against the
                // variant tail too (the user types `Combat`, not
                // `MobAiState::Combat`), mirroring the score's vname handling.
                let (exact, band) = match_band_exact(name, q, q_raw);
                let key = RankKey {
                    exact,
                    type_rank,
                    band,
                    case: case_rank,
                    kind,
                    dist: 0,
                    method,
                    fine: RankKey::fine(score),
                    prox: prox_of(c.def_file),
                    freq: match c.kind {
                        SymCat::Local | SymCat::Param => 0,
                        _ => freq_bucket(freq.get(&c.name).copied().unwrap_or(0)),
                    },
                    rec: c.rec,
                    len: RankKey::len_key(score, name.len()),
                };
                Cand::new(key, CandName::Sym(c.name), CandShape::Entry(c))
            })
            .collect();
        // We only need the BEST `max_items`, not a full ordering --
        // the final sort below re-orders the kept set anyway. So select the
        // top-N in O(n) (`select_nth_unstable_by`) rather than sort all in
        // O(n log n); at empty prefix that is ~2500 candidates. `cand_cmp` IS
        // the sort_text encoding order, so the select agrees with the final
        // sort by construction.
        let score_leads = !has_expected;
        let max_items = max_completion_items();
        if general.len() > max_items {
            general.select_nth_unstable_by(max_items, cand_cmp);
            general.truncate(max_items);
        }
        let mut sink = Sink::new();
        sink.cands = general;
        // Synthesized generic `Entity<..>` from the param-name + fn-prefix
        // heuristic. Emitted directly (not a workspace symbol) with the minimal
        // rank key so it ranks first among the type candidates. With an inferred
        // payload it's the concrete `Entity<Mob>`; without one the `Entity<$0>`
        // snippet parks the cursor in the generic slot (materialize_cand).
        if entity_hint {
            let label = match entity_payload {
                Some(p) => format!("Entity<{p}>"),
                None => "Entity<>".to_string(),
            };
            if admit(label.as_bytes(), prefix.as_bytes()) {
                let key = RankKey {
                    len: RankKey::len_key(9999, label.len()),
                    ..Default::default()
                };
                sink.push(Cand::new(
                    key,
                    CandName::Owned(label),
                    CandShape::EntitySynth { payload: entity_payload },
                ));
            }
        }
        // Named-arg completions: at a DIRECT call arg slot, offer the callee's
        // still-unsupplied params as `name:` items. Additive to the value-
        // expression candidates and ranked in the top (type_rank 0) bucket, so
        // `greet(gr|)` surfaces `greeting:` above the identifier fallback while
        // `grid` etc. stay. Skips params already passed positionally or by name.
        if let Some(nargs) = cx.named_arg.as_ref() {
            // Params already supplied by a `..spread` operand's fields.
            let spread_covered: std::collections::HashSet<Ustr> = nargs
                .spread_probes
                .iter()
                .flat_map(|&p| crate::query::struct_field_names_at(ws, mid, inf, p))
                .collect();
            for (i, (pname, pty)) in callee_param_names(ws, &nargs.callee).into_iter().enumerate() {
                if i < nargs.positional
                    || nargs.named.iter().any(|n| n.as_str() == pname.as_str())
                    || spread_covered.contains(&pname)
                {
                    continue;
                }
                if !prefix.is_empty() && !admit(pname.as_str().as_bytes(), prefix.as_bytes()) {
                    continue;
                }
                let score = fuzzy_score(pname.as_str().as_bytes(), prefix.as_bytes());
                let (ex, bd) = match_band_exact(pname.as_str(), prefix.as_bytes(), prefix_raw.as_bytes());
                let key = RankKey {
                    exact: ex,
                    band: bd,
                    fine: RankKey::fine(score),
                    len: RankKey::len_key(score, pname.as_str().len()),
                    ..Default::default()
                };
                sink.push(Cand::new(
                    key,
                    CandName::Sym(pname),
                    CandShape::NamedArg { pname, pty },
                ));
            }
        }
        // Coroutine ergonomics: `Wait` is the dominant Stone yield type
        // (~75% of `Coroutine<..>` annotations), so surface it. All gated on a
        // `Wait` type existing (the Stone `Wait` is an enum); otherwise the
        // generic `Coroutine<$0>` snippet stands in.
        let has_wait = nominal_exists(ws, "Wait");
        let coroutine_slot = cx.coroutine_generic;
        if coroutine_slot {
            // (a) first generic arg of `Coroutine<..>` -> `Wait` ranks #0,
            // superseding the plain struct symbol.
            if has_wait && admit(b"Wait", prefix.as_bytes()) {
                sink.retain_not_named("Wait");
                let key = RankKey {
                    len: RankKey::len_key(9999, "Wait".len()),
                    ..Default::default()
                };
                sink.push(Cand::new(
                    key,
                    CandName::Owned("Wait".to_string()),
                    CandShape::Wait,
                ));
            }
        } else if !expr_position && !prefix.is_empty() && admit(b"Coroutine", prefix.as_bytes()) {
            // (b) completing the `Coroutine` type name -> snippet forms replace
            // the bare type: `Coroutine<Wait>` first, `Coroutine<$0>` second.
            sink.retain_not_named("Coroutine");
            if has_wait {
                let key = RankKey {
                    len: RankKey::len_key(9999, "Coroutine<Wait>".len()),
                    ..Default::default()
                };
                sink.push(Cand::new(
                    key,
                    CandName::Owned("Coroutine<Wait>".to_string()),
                    CandShape::Coroutine { with_wait: true },
                ));
            }
            let key = RankKey {
                fine: RankKey::fine(9998),
                len: RankKey::len_key(9998, "Coroutine<>".len()),
                ..Default::default()
            };
            sink.push(Cand::new(
                key,
                CandName::Owned("Coroutine<>".to_string()),
                CandShape::Coroutine { with_wait: false },
            ));
        }
        // (c) `co fn NAME()|` with no return type -> offer the `-> Coroutine<..>`
        // return type to the right of `)`.
        if prefix.is_empty() && co_fn_return_slot(src.as_bytes(), offset) {
            let label = if has_wait { "-> Coroutine<Wait>" } else { "-> Coroutine<>" };
            let key = RankKey {
                len: RankKey::len_key(9999, label.len()),
                ..Default::default()
            };
            sink.push(Cand::new(
                key,
                CandName::Owned(label.to_string()),
                CandShape::CoReturn { has_wait },
            ));
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
        // A pattern head is not a value slot: a dotted reach path is not a
        // valid pattern, so the expansion is suppressed there.
        if !expected_types.is_empty() && !pattern_pos {
            let reach_owned;
            let reach = match &proj.reach {
                Some(r) => r,
                None => {
                    reach_owned = crate::reachability::ReachabilityIndex::build(ws);
                    &reach_owned
                }
            };
            let pfx = prefix.as_bytes();
            // Roots: in-scope locals/params PLUS every global (Rock has one global
            // namespace across files, so `player.movable.grid` works where
            // `player` is a global from another file -- `locals_in_scope` omits
            // globals).
            // Root locality rides the kind rung: a path seeded from a LOCAL
            // ranks with locals (a method on a local yielding the type is a
            // top-tier candidate), a global-rooted path with globals -- so a
            // deep chain off some global can't outrank a direct right-type
            // global (docs/completion-ranking-design.md, principle 3).
            let mut roots: Vec<(String, String, u8)> = crate::query::locals_in_scope(ws, mid, inf, offset)
                .into_iter()
                .map(|(n, _k, ty, _ty_id, _decl)| (n.to_string(), ty, 0))
                .collect();
            for m in ws.modules.iter() {
                for (gname, gidx) in m.global_by_name.iter() {
                    roots.push((gname.to_string(), ws.render(None, m.globals[*gidx].ty), 1));
                }
            }
            for (vname, vty, root_kind) in roots {
                let res = reach.paths_to(&vty, &expected_types, REACH_CAP);
                if res.truncated && lsp_profile_enabled() {
                    eprintln!("[bone completion] reach paths from `{vname}: {vty}` clipped at {REACH_CAP}");
                }
                for p in res.paths {
                    // A path that CALLS the enclosing fn (`k.partition_style()`
                    // inside `partition_style`) is a recursive self-reference --
                    // skip it, same reason the fn is not promoted as a candidate.
                    if enclosing_fn.is_some_and(|f| reach_suffix_calls(&p.suffix, &f)) {
                        continue;
                    }
                    // Dedup / fuzzy-match / sort on the bare access form; the
                    // final hop's parens/snippet are rendered from the flags.
                    let key = format!("{vname}.{}", p.suffix);
                    // Require a CLEAN match (word-boundary / run), not a
                    // coincidental scatter, so `nog` doesn't drag in
                    // `necro_cast.ima(g)e_dims()...` reach paths.
                    if !pfx.is_empty() && !clean_match(key.as_bytes(), pfx) {
                        continue;
                    }
                    if !sink.admit(&key) {
                        continue;
                    }
                    // A FIELD-final reach path IS a stored value of the expected
                    // type (type_rank 0, root locality). A METHOD-final path
                    // COMPUTES the value via a UFCS call -- it is morally "a fn
                    // returning the slot type" (type_rank 1, fn-kind 2), same
                    // tier as the direct fn it wraps, so `RED.wisp_trail_new()`
                    // can't bury the direct `wisp_trail_new($0)` (it loses on the
                    // hop-distance rung instead). The shared key still folds in
                    // match quality, the hop distance, and field-before-method.
                    let (type_rank, kind) = if p.is_method { (1, 2) } else { (0, root_kind) };
                    let rk = arg_rank_key(type_rank, kind, &key, pfx, p.dist, u8::from(p.is_method), 0);
                    sink.push(Cand::new(
                        rk,
                        CandName::Owned(key),
                        CandShape::Reach {
                            final_ty: p.final_ty,
                            is_method: p.is_method,
                            takes_args: p.takes_args,
                        },
                    ));
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
        // fill the cap: MOST call-through items are bucket 4 (sort_text "1..."),
        // sorting AFTER every general (bucket 0-3) / reach item, so the final
        // cap truncates them -- computing them is waste. Removes the
        // worst keystrokes (high-general-count short prefixes, `sp` -> 229). Known
        // gap: a LOCAL-SEEDED promoted call-through (bucket 0/1) could survive the
        // cap but is skipped here at a >=MAX short prefix; acceptable under the
        // `isIncomplete` contract (the user narrows and it reappears).
        // Suppressed in a pattern head like reach: `f(arg)` is not a pattern.
        if expr_position && !pattern_pos && sink.len() < max_items {
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
            ct_counts = call_through_param_completions(
                ws,
                mid,
                inf,
                offset,
                &prefix,
                ct,
                reach,
                score_leads,
                expected.as_mut(),
                enclosing_fn.as_deref(),
                &mut sink,
            );
        }
        let call_through_ms = t_ct.elapsed().as_secs_f64() * 1000.0;
        LAST_COMPLETION_PHASES.with(|c| {
            c.set(CompletionPhaseTimings {
                candidates_ms: cands_ms,
                rank_ms,
                reach_ms,
                call_through_ms,
                n_general: n_cands,
                n_out: sink.len(),
                ct_fns: ct_counts.fns,
                ct_arg_iters: ct_counts.arg_iters,
                ct_survivors: ct_counts.survivors,
            })
        });
        if lsp_profile_enabled() {
            eprintln!(
                "[bone completion] candidates {cands_ms:.2}ms ({n_cands} cands)  rank {rank_ms:.2}  reach {reach_ms:.2}  call-through {call_through_ms:.2}  ({} items, prefix {:?})",
                sink.len(),
                prefix
            );
        }
        // ONE select over the facts. The list is served `isIncomplete: true`,
        // so cap it: the client re-queries per keystroke and the server
        // re-scores, so returning the whole workspace (2539 items at empty
        // prefix) is pure wasted serialization -- the dominant end-to-end
        // completion cost.
        if sink.cands.len() > max_items && lsp_profile_enabled() {
            eprintln!(
                "[bone completion] completion truncated {} -> {max_items}",
                sink.cands.len()
            );
        }
        sink.cands.sort_by(cand_cmp);
        sink.cands.truncate(max_items);
        sink.cands
            .into_iter()
            .map(|c| materialize_cand(c, fn_sigs, next_is_open_paren, pattern_pos))
            .collect()
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
            .all_reference_spans(proj, path, offset, crate::query::FnRefScope::AllOverloads)
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
        // Rename rewrites the whole (shared) overloaded name, not one overload.
        let sites = self.all_reference_spans(proj, path, offset, crate::query::FnRefScope::AllOverloads);
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
    /// LSP calls this on `textDocument/didSave`; the stdlib IS included (warm
    /// modules carry real paths), so its symbols are jump-to-by-name. A path with
    /// no session yet writes nothing and returns 0. Returns the number of symbols
    /// written.
    pub fn write_ctags(&self, path: &str) -> std::io::Result<usize> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let root = Self::root_for(path);
        let Some(proj) = self.projects.get(&root) else {
            return Ok(0);
        };
        self.write_ctags_for(&root, proj)
    }

    /// Regenerate `.rock.tags` for every loaded STICK project root, not just the
    /// edited file's. The editor holds one tags file per open package and any
    /// of them may be stale (a sibling package edited last session, never
    /// re-saved this one); refreshing all of them on save keeps cross-package
    /// tag jumps accurate. Standalone-file roots (a scratch `.rock` with no
    /// `stick.toml`, keyed by its parent dir) are skipped -- an all-roots sweep
    /// must not scatter `.rock.tags` into `~` or `/tmp`. Per-root `(root, result)`
    /// so the caller can log individual failures; one root's error never blocks
    /// the others.
    pub fn write_all_ctags(&self) -> Vec<(PathBuf, std::io::Result<usize>)> {
        self.projects
            .iter()
            .filter(|(root, _)| root.join("stick.toml").exists())
            .map(|(root, proj)| (root.clone(), self.write_ctags_for(root, proj)))
            .collect()
    }

    fn write_ctags_for(&self, root: &Path, proj: &BoneProject) -> std::io::Result<usize> {
        let ws = proj.session.workspace();

        let mut entries = Vec::new();
        // Every USER module the session loaded -- the whole compilation unit
        // (project + stick.toml deps), NOT just the files open in the editor.
        // Iterating the workspace modules directly (vs `texts.keys()` +
        // `user_module_for_path`) can't silently drop a file on a path mismatch
        // and needs no canonicalization round-trip. The warm stdlib is included
        // (its modules carry real canonical paths), so `vec_push` & co. are
        // jump-to-by-name; only the builtin (pathless) module is excluded.
        for m in ws.modules.iter() {
            if m.id == ModuleId::builtin() {
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
        let (sigs, call_span) = crate::query::signature_help(ws, mid, inf, offset, Some(&proj.texts))?;
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
        // D-form receiver hints come off the surface AST (bone drops the
        // `receiver_surface` marker at lowering), so reparse the buffer here and
        // fold them into the bone-native hint stream as ordinary type hints.
        let recv_hints = skull_parser::parse_module(src, Ustr::from(path))
            .ok()
            .map(|ast| crate::query::receiver_param_hints(&ast, start, end))
            .unwrap_or_default()
            .into_iter()
            .map(|(span, label)| gaps::HintEntry { span, label, leading: false });
        gaps::inlay_hints(ws, mid, inf, start, end)
            .into_iter()
            .chain(recv_hints)
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

    /// Resolve a span's file to its LSP [`Uri`] and source text: the session
    /// buffer for a tracked file (incl. the stdlib, whose sources are seeded into
    /// `texts`), else a disk read (untracked sibling). `Cow` borrows in the common
    /// (tracked) case and only owns on the disk read.
    fn span_uri_and_text<'a>(
        &self,
        proj: &'a BoneProject,
        span: &Span,
    ) -> Option<(Uri, Cow<'a, str>)> {
        let file = span.file.as_str();
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
        for (_, md) in proj.session.all_macro_defs() {
            names.entry(md.name).or_insert(TT_MACRO);
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
            // A `// rock: ...` directive comment colors as MACRO so themes set
            // it apart from a plain comment.
            let token_type = match &tok.kind {
                rock_tokenizer::TokenKind::Comment(body) if is_magic_rock_directive(body.as_str()) => {
                    TT_MACRO
                }
                _ => token_type,
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
        // Contextual `static` keyword — lexed as an ident, recognized before the
        // binding name (`static n = ..`). Only a `static <ident>` in non-member
        // position is the keyword; a value named `static` (`static + 1`) is not.
        T::Ident(n)
            if !is_member
                && n.as_str() == "static"
                && matches!(next_kind, Some(T::Ident(_))) =>
        {
            (TT_KEYWORD, 0)
        }
        // A macro invocation (`get_speed()`): the call-site ident is a bare token
        // whose name resolves to a MACRO in the name map. Macro expansion retags
        // the expanded template body onto the whole invocation span, so the HIR
        // offset map can carry the template's first ref (e.g. a global ->
        // VARIABLE+STATIC) at the call-site offset and would otherwise win. Claim
        // the call site as MACRO here, gated on call position (`(` next) and
        // non-member, so a local that merely shadows a macro name (used as a
        // value, no following paren) keeps its HIR classification.
        T::Ident(n)
            if !is_member
                && matches!(next_kind, Some(T::OpenParen))
                && names.get(n) == Some(&TT_MACRO) =>
        {
            (TT_MACRO, 0)
        }
        T::Fn | T::Operator | T::Table | T::Let | T::Return | T::Defer | T::Yield | T::Await
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
            // A named-argument label (`f(width: 3)`): an ident the HIR doesn't
            // claim, directly followed by `:`. It names a PARAMETER -- classify
            // it as one, BEFORE the workspace name map, which would otherwise
            // color a label that collides with a global fn (`fn width`) as
            // FUNCTION while non-colliding labels fell to VARIABLE (inconsistent).
            // HIR-claimed `ident:` positions (struct-lit fields -> property,
            // param/typed-let bindings) already returned above, so only genuine
            // call labels reach here.
            .or_else(|| matches!(next_kind, Some(T::Colon)).then_some((TT_PARAMETER, 0)))
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
        BindingKind::Let | BindingKind::Static | BindingKind::Fn => TT_VARIABLE,
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
pub(crate) fn callee_slot_is_field_of(ws: &Workspace, callee: &str, arg_index: usize) -> Option<bool> {
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

/// Typed expected-type oracle for one completion request's arg slot
/// (docs/assignable-plan.md). The callee's surviving overloads' params are
/// instantiated ONCE per overload (fresh vars) in a scratch
/// `Gamma::for_query` seeded from the cursor body's inference; candidate
/// compatibility is bone's REAL `coerce` under trial, rolled back per
/// candidate so one candidate's bindings never leak into the next. A UFCS
/// receiver coerces into each overload's first param and its bindings are
/// KEPT -- the receiver's instantiation is what the arg slot must agree with
/// (`nums.add_all(|)`: receiver `[int]` binds T, so the slot expects `[int]`).
pub(crate) struct ExpectedArg<'a> {
    gamma: Gamma<'a>,
    span: Span,
    slots: Vec<TyId>,
    /// DECLARED-form renders of the surviving slots ("Color", "[T]") -- the
    /// string-keyed reachability index's targets, receiver-filtered by
    /// construction (only overloads the receiver coerces into contribute). The
    /// reach graph is nominal-name keyed, so consuming these is a graph lookup,
    /// not a type compare -- bone's `coerce` is the only type-comparison path.
    pub(crate) rendered: Vec<String>,
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
    pub(crate) fn for_call(
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
    pub(crate) fn for_slot(
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
    pub(crate) fn accepts(&mut self, ty: TyId) -> bool {
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
    pub(crate) fn accepts_fn_returning(&mut self, ws: &Workspace, name: &str) -> bool {
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
    pub(crate) fn expects_nominal(&self, name: &str) -> bool {
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
pub(crate) fn fn_unique_return_ty(ws: &Workspace, name: &str) -> Option<TyId> {
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

/// Start offsets of the arg segments that begin before `offset`, within the
/// innermost enclosing call -- used to resolve a callee generic (`T`) from a
/// sibling arg's inferred type. `[]` if not inside a call.
pub(crate) fn preceding_arg_starts(src: &str, offset: usize) -> Vec<usize> {
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
pub(crate) fn ty_of(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> bone_semantic::hir::TypeExpr {
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
pub(crate) fn resolved_tyid(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> TyId {
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
pub(crate) fn bind_generic(
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

/// Structural mirror of bone-inference's `for_elem_ty` ITERABLE judgment
/// (keep the shape list in sync): `[T]`, `Map<K, V>`, `Arena<T>`, and the
/// native buffer nominals. Drives the ForIn completion rung.
pub(crate) fn is_iterable_ty(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> bool {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    match ty_of(ws, body, ty) {
        TypeExpr::App { ctor: TypeCtor::Array, .. }
        | TypeExpr::App { ctor: TypeCtor::Map, .. } => true,
        TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } => matches!(
            n.as_str(),
            "Arena"
                | "NativeBuffer"
                | "NativeBufferU8"
                | "NativeBufferI8"
                | "NativeBufferU16"
                | "NativeBufferI16"
                | "NativeBufferU32"
                | "NativeBufferI32"
        ),
        _ => false,
    }
}

/// The base nominal (struct) name of `ty`, if it is a `Nominal` App.
pub(crate) fn nominal_base(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> Option<Ustr> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    match ty_of(ws, body, ty) {
        TypeExpr::App { ctor: TypeCtor::Nominal(name), .. } => Some(name),
        _ => None,
    }
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
/// THIS THREAD. For the in-process profiler (`rock iter-completion`) and the
/// corpus recorder.
pub fn last_completion_phase_timings() -> CompletionPhaseTimings {
    LAST_COMPLETION_PHASES.with(|c| c.get())
}

/// The identifier prefix ending at `pos` -- THE prefix definition completion's
/// own filter uses (`ident_prefix`), exposed for the corpus recorder so the
/// recorded prefix can never diverge from what the ranker matched against.
pub fn ident_prefix_at(src: &str, pos: Position) -> &str {
    crate::completion::classify::ident_prefix(src, offset_at_line_col(src, pos.line, pos.character))
}

/// True when the current line from its start up to `offset` is empty or only
/// spaces/tabs -- a bare/indented newline, where a space trigger is `<CR>`
/// auto-indent rather than a token-following space.
fn line_prefix_is_blank(src: &str, offset: usize) -> bool {
    let line_start = src.get(..offset).and_then(|s| s.rfind('\n')).map_or(0, |i| i + 1);
    src.get(line_start..offset)
        .is_some_and(|p| p.chars().all(|c| c == ' ' || c == '\t'))
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
pub(crate) fn analyze_variant<R>(
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

/// Whether a reach-path suffix (`a().b().c`, UFCS-chained) has a CALL hop to
/// `fname` -- i.e. the path produces its value by calling that fn. Each `.`-
/// separated hop's identifier is the text before its `(`; compared exactly, so
/// `my_style()` does not match `style`.
pub(crate) fn reach_suffix_calls(suffix: &str, fname: &str) -> bool {
    suffix.split('.').any(|hop| hop.split('(').next().unwrap_or(hop) == fname)
}

/// Resolve a `SlotAnchor` to a typed expected-slot session against `ws` --
/// the live workspace on the general path, the munge variant on the member
/// path (member TyIds only resolve against the variant's Gamma).
pub(crate) fn expected_for_anchor<'a>(
    ws: &'a Workspace,
    mid: ModuleId,
    inf: &'a InferenceResult,
    offset: usize,
    anchor: SlotAnchor,
) -> Option<ExpectedArg<'a>> {
    let slot = match anchor {
        SlotAnchor::TypeAt(a) => bone_semantic::query::type_at(ws, mid, inf, a)?.ty,
        SlotAnchor::EnclosingFnReturn => {
            let m = ws.modules.get(mid.index())?;
            let bone_semantic::hir::BodyOwner::Function(fid) = m.body_at(offset)? else {
                return None;
            };
            m.functions[fid].return_ty?
        }
    };
    ExpectedArg::for_slot(ws, mid, inf, offset, slot)
}

/// bone's `CompilerError.location` is 0-based in both line and col -- the same
/// convention as LSP -- so it passes through unshifted (a 1-based correction
/// would report every error one line high, masked on line 0 by saturation).
fn severity_from_level(level: rock_core::errors::CompilerErrorLevel) -> DiagnosticSeverity {
    use rock_core::errors::CompilerErrorLevel::*;
    match level {
        Error => DiagnosticSeverity::ERROR,
        Warning => DiagnosticSeverity::WARNING,
        Advice => DiagnosticSeverity::HINT,
    }
}

fn bone_diagnostic(range: Range, message: String, level: rock_core::errors::CompilerErrorLevel) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity_from_level(level)),
        source: Some("bone".to_string()),
        message,
        ..Default::default()
    }
}

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
    bone_diagnostic(Range::new(start, end), e.message.clone(), e.level)
}

/// One `(file, Diagnostic)` per label the compiler underlined, so every flagged
/// location becomes its own squiggle carrying that label's note (a multi-label
/// error like the `./` hint -- the `/` cause plus the use site -- surfaces at
/// both). Labels can live in different files (returned per-label so the caller
/// files each correctly). A label-less error falls back to the error location.
pub fn compiler_error_to_diagnostics(e: &CompilerError) -> Vec<(Ustr, Diagnostic)> {
    let labelled: Vec<_> = e.labels.iter().filter(|l| !l.span.is_empty()).collect();
    if labelled.is_empty() {
        let loc = &e.location;
        let range = Range::new(
            Position::new(loc.line as u32, loc.col as u32),
            Position::new(loc.line as u32, loc.col as u32 + 1),
        );
        return vec![(loc.file, bone_diagnostic(range, e.message.clone(), e.level))];
    }
    labelled
        .iter()
        .map(|l| {
            let s = &l.span;
            let range = Range::new(
                Position::new(s.line_range.0 as u32, s.col_range.0 as u32),
                Position::new(s.line_range.1 as u32, s.col_range.1 as u32),
            );
            // label[0].message == the error message (new_with_label); a stray
            // empty label message falls back to it too.
            let msg = if l.message.is_empty() { e.message.clone() } else { l.message.clone() };
            (s.file, bone_diagnostic(range, msg, e.level))
        })
        .collect()
}

/// Render a lint finding as an LSP warning tagged with its lint name in the
/// `code` field, so the "Suppress with `// rock: allow(<lint>)`" code action
/// identifies lint squiggles robustly (by `code`, not by message-sniffing).
fn lint_finding_diagnostic<F: LintFinding>(f: &F) -> Diagnostic {
    let mut d = compiler_error_to_diagnostic(&f.to_diagnostic());
    d.code = Some(NumberOrString::String(F::LINT.to_string()));
    d
}

/// Completion inside a `// rock: ...` magic comment. `Some(items)` when the
/// cursor sits in such a directive -- the `allow` verb after `// rock: `, or the
/// known lint names inside `// rock: allow(`. `None` when the cursor isn't in a
/// magic comment, so the caller runs normal completion.
fn magic_comment_completions(src: &str, offset: usize) -> Option<Vec<CompletionItem>> {
    let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
    let before = &src[line_start..offset];
    // The LAST `//` before the cursor, so a trailing `x() // rock: allow(` works.
    let after_slashes = before.rsplit_once("//")?.1;
    let body = after_slashes.trim_start().strip_prefix("rock")?;
    let body = body.trim_start().strip_prefix(':')?.trim_start();
    if let Some(args) = allow_open_args(body) {
        return Some(lint_name_items(current_lint_token(args)));
    }
    Some(directive_verb_items(body))
}

/// True iff a `//` comment body (the text after `//`) opens a `rock:` directive
/// (`// rock: allow(...)`). Shared by semantic-token coloring.
fn is_magic_rock_directive(comment_body: &str) -> bool {
    comment_body
        .trim_start()
        .strip_prefix("rock")
        .and_then(|s| s.trim_start().strip_prefix(':'))
        .is_some()
}

/// The argument text after `allow(` while the parens are still open (no `)`
/// before the cursor). `body` is the text after `// rock: `.
fn allow_open_args(body: &str) -> Option<&str> {
    let args = body.strip_prefix("allow")?.trim_start().strip_prefix('(')?;
    (!args.contains(')')).then_some(args)
}

/// The partial lint name being typed inside `allow(a, b, c|)`: the tail after
/// the last comma, leading identifier chars only.
fn current_lint_token(args: &str) -> &str {
    let tail = args.rsplit(',').next().unwrap_or(args).trim_start();
    let end = tail.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(tail.len());
    &tail[..end]
}

fn lint_name_items(typed: &str) -> Vec<CompletionItem> {
    let typed_lc = typed.to_lowercase();
    bone_lints::KNOWN_LINTS
        .iter()
        .filter(|name| name.to_lowercase().starts_with(&typed_lc))
        .enumerate()
        .map(|(i, name)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            sort_text: Some(format!("{i:03}")),
            ..Default::default()
        })
        .collect()
}

fn directive_verb_items(typed: &str) -> Vec<CompletionItem> {
    // Only `allow` is ported to bone; the accept-inserts `allow(` open-paren.
    ["allow"]
        .into_iter()
        .filter(|verb| verb.starts_with(typed))
        .map(|verb| CompletionItem {
            label: verb.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some(format!("{verb}(")),
            sort_text: Some("000".to_string()),
            ..Default::default()
        })
        .collect()
}

/// Per-file lint diagnostics (warnings): run the flat-HIR lints over the target
/// module's bodies, drop `// rock: allow(<lint>)`-suppressed findings, and
/// render each as a warning. Both the live `update` path and the one-shot
/// `diagnostics_for` share this, so a new flat-HIR lint added here reaches every
/// path and the `lint_cache` automatically.
fn lint_diagnostics_for(proj: &BoneProject, path: &str) -> Vec<Diagnostic> {
    let ws = proj.session.workspace();
    let Some(mid) = user_module_for_path(ws, path) else {
        return Vec::new();
    };
    // A warm (stdlib) file the user opened is a read-only reference: don't lint
    // it (every stdlib helper would false-flag as an unused var).
    if proj.session.warm_mids.contains(&mid) {
        return Vec::new();
    }
    let module = &ws.modules[mid.index()];
    let variants = bone_lints::enum_variant_names(ws, &proj.session.warm_mids);
    let unused_vars: Vec<_> = module
        .bodies
        .values()
        .flat_map(|b| bone_lints::unused_vars_in_body(b, &variants))
        .collect();
    let unused_pure: Vec<_> = module
        .bodies
        .values()
        .flat_map(bone_lints::unused_pure_exprs_in_body)
        .collect();
    let src = proj.texts.get(path).map(String::as_str).unwrap_or("");
    let mut out: Vec<Diagnostic> =
        bone_lints::filter_suppressed(unused_vars, src).iter().map(lint_finding_diagnostic).collect();
    out.extend(bone_lints::filter_suppressed(unused_pure, src).iter().map(lint_finding_diagnostic));
    out
}

/// `transmute_of_nullable` lint diagnostics (warnings) for one file: run the
/// lint over the target module's bodies -- reading that module's per-body
/// inference for arg/result nullability -- then drop
/// `// rock: allow(transmute_of_nullable)`-suppressed findings.
fn transmute_diagnostics_for(proj: &BoneProject, path: &str) -> Vec<Diagnostic> {
    let ws = proj.session.workspace();
    let Some(mid) = user_module_for_path(ws, path) else {
        return Vec::new();
    };
    if proj.session.warm_mids.contains(&mid) {
        return Vec::new();
    }
    let module = &ws.modules[mid.index()];
    let inf = ws.inference(mid);
    let findings: Vec<_> = module
        .bodies
        .iter()
        .flat_map(|(owner, body)| {
            let bi = inf.and_then(|i| i.per_fn.get(owner));
            bone_lints::transmute_of_nullable_in_body(ws, bi, body)
        })
        .collect();
    let src = proj.texts.get(path).map(String::as_str).unwrap_or("");
    bone_lints::filter_suppressed(findings, src)
        .iter()
        .map(lint_finding_diagnostic)
        .collect()
}

/// `unused_fn` lint diagnostics for one file. Reachability is whole-project, so
/// this runs the workspace walk and keeps only `path`'s dead functions; the
/// caller gates it to `bin` projects. The one-shot CLI path -- the `update`
/// path distributes the same by-file result across every tracked file.
fn fn_lint_diagnostics_for(proj: &BoneProject, path: &str) -> Vec<Diagnostic> {
    let by_fn =
        bone_lints::unused_fns_by_file(proj.session.workspace(), &proj.session.warm_mids);
    let Some(findings) = by_fn.get(&Ustr::from(path)) else {
        return Vec::new();
    };
    let src = proj.texts.get(path).map(String::as_str).unwrap_or("");
    bone_lints::filter_suppressed(findings.clone(), src)
        .iter()
        .map(lint_finding_diagnostic)
        .collect()
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
pub(crate) fn user_module_for_path(ws: &Workspace, path: &str) -> Option<ModuleId> {
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

/// Single-letter extended-ctags kind for a top-level declaration, or None for
/// categories that aren't file-level symbols (locals, params, fields, enum
/// variants, keywords) and so don't belong in a tags file.
fn ctags_kind(k: SymCat) -> Option<char> {
    match k {
        SymCat::Function => Some('f'),
        SymCat::Macro => Some('M'),
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
        SymCat::Macro => SymbolKind::FUNCTION,
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

    // A two-label error (the `./` hint: the `/` cause + the use site) expands to
    // one diagnostic per label, each at its label's span with its label's note,
    // so nvim paints a squiggle on both lines.
    #[test]
    fn multi_label_error_expands_to_one_diagnostic_per_label() {
        let mut cause = Span::unknown();
        cause.file = Ustr::from("a.rock");
        cause.line_range = (118, 118);
        cause.col_range = (18, 50);
        cause.offset_range = (200, 232);
        let mut use_site = Span::unknown();
        use_site.file = Ustr::from("a.rock");
        use_site.line_range = (123, 123);
        use_site.col_range = (35, 50);
        use_site.offset_range = (400, 415);

        let e = CompilerError::new_with_label(cause, "use `./`".to_string())
            .with_label(use_site, "this `float` is used where `int` is required".to_string());
        let diags = compiler_error_to_diagnostics(&e);

        assert_eq!(diags.len(), 2, "one diagnostic per label");
        assert_eq!(diags[0].1.range.start.line, 118);
        assert_eq!(diags[0].1.message, "use `./`");
        assert_eq!(diags[1].1.range.start.line, 123);
        assert_eq!(diags[1].1.message, "this `float` is used where `int` is required");
        assert!(diags.iter().all(|(f, _)| f.as_str() == "a.rock"));
    }

    // A plain single-label error is unchanged: exactly one diagnostic.
    #[test]
    fn single_label_error_stays_one_diagnostic() {
        let mut span = Span::unknown();
        span.file = Ustr::from("a.rock");
        span.line_range = (10, 10);
        span.col_range = (7, 15);
        span.offset_range = (100, 108);
        let e = CompilerError::new_with_label(span, "Undefined global `ris_shot`".to_string());
        let diags = compiler_error_to_diagnostics(&e);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].1.message, "Undefined global `ris_shot`");
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
