//! The LSP's facade over the `bone` typechecker.
//!
//! Each `*_workspace` handler calls the matching method here; the persistent
//! server holds one [`BoneWorkspace`]. [`gaps`] holds the thin delegations into
//! `crate::query`.
//!
//! Consumes bone's public pipeline (`bone_incremental::session::Session`,
//! `bone_semantic::query`) plus this crate's IDE-navigation queries
//! (`crate::query`).

mod completion;
pub mod gaps;
mod macro_refs;
mod rename_guard;

pub use rename_guard::RenameRefusal;
use rename_guard::check_new_name;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use lsp_types::{
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity, DocumentHighlight,
    DocumentSymbol, Hover, HoverContents, InlayHint, InlayHintKind, InlayHintLabel, Location,
    MarkupContent, MarkupKind, NumberOrString, ParameterInformation, ParameterLabel, Position,
    Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SignatureHelp,
    SignatureInformation, SymbolInformation, SymbolKind, TextEdit, Uri, WorkspaceEdit,
};

use bone_core::type_equiv::TyRef;
use bone_incremental::session::{Session, SessionFile, UpdateTier};
use bone_semantic::inference::{self, Gamma};
use bone_semantic::prelude::{
    BodyInference, FxHashMap, FxHashSet, InferenceResult, ModuleId, Span, TyId, Workspace,
    is_field_ref_former, is_intrinsic_key_in,
};
use skull_parser::CompilerError;
use ustr::Ustr;

use crate::completion::classify::*;
use crate::completion::match_policy::admit;
use crate::completion::rank::*;
use crate::completion::select::*;
use crate::completion::sources::*;
use crate::query::{SymCat, clean_match};
use crate::rank_key::RankKey;

/// A fallback is reported once per request, file and stage until project text
/// changes. Keep this with the project so closing it also releases the keys.
#[derive(Default)]
struct FallbackReports {
    reported: Mutex<FxHashSet<(&'static str, String, &'static str)>>,
}

impl FallbackReports {
    fn report(
        &self,
        request: &'static str,
        path: &str,
        stage: &'static str,
        reason: &str,
        fallback: &str,
    ) {
        if self
            .reported
            .lock()
            .expect("LSP fallback reports lock")
            .insert((request, path.to_string(), stage))
        {
            log::warn!(target: "LSP",
                "{request}: `{path}`: {stage} failed: {reason}; fallback: {fallback}");
        }
    }

    fn result<T>(
        &self,
        result: Result<T, CompilerError>,
        request: &'static str,
        path: &str,
        stage: &'static str,
        fallback: &str,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.report(request, path, stage, &error.message, fallback);
                None
            }
        }
    }

    fn clear(&mut self) {
        self.reported
            .get_mut()
            .expect("LSP fallback reports lock")
            .clear();
    }
}

// ---------------------------------------------------------------------------
// Warm stdlib (parsed once, cloned into every session)
// ---------------------------------------------------------------------------

/// Each stdlib file's session id and canonical display path (its file on
/// disk, or `/stdlib/<name>` for the embedded wasm copy).
/// Computed once: `canonicalize` is a syscall and the id <-> path translation
/// below runs on every request.
fn stdlib_paths() -> &'static [(String, String)] {
    static PATHS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    PATHS.get_or_init(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .map(|f| {
                (
                    rock_stdlib::stdlib_file_id(f.name),
                    canonicalize_lenient(f.display_path),
                )
            })
            .collect()
    })
}

/// The session keys the stdlib by the loader's stdlib ids (`<stdlib>/name`,
/// the CLI's spelling), because bone reads std ownership -- the v1 prelude,
/// the std-only primitives -- from that spelling. The editor names the same
/// files by their real paths. These two functions are the only translation
/// between the two: every public entry maps an editor path in through
/// [`session_file_id`] (via `canon`), and everything handed back to the editor
/// -- a location, a diagnostic's file, a symbol -- goes out through
/// [`editor_path`].
fn session_file_id(canonical_path: String) -> String {
    match stdlib_paths().iter().find(|(_, p)| *p == canonical_path) {
        Some((id, _)) => id.clone(),
        None => canonical_path,
    }
}

/// The editor's path for a session file id (see [`session_file_id`]).
pub fn editor_path(id: &str) -> Cow<'_, str> {
    stdlib_paths()
        .iter()
        .find(|(sid, _)| sid == id)
        .map_or(Cow::Borrowed(id), |(_, p)| Cow::Borrowed(p.as_str()))
}

/// Stdlib sources keyed by their session ids, seeded into every project's
/// `texts` so default-value slicing, `workspace_symbols`, and span->text see
/// the stdlib like any other file.
fn stdlib_texts() -> Vec<(String, String)> {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| (rock_stdlib::stdlib_file_id(f.name), f.contents.to_string()))
        .collect()
}

/// The real stdlib, parsed through skull-parser (bone's front end) under its
/// session ids. Opening a stdlib buffer maps its path to the same id and so
/// interns to the same `ModuleId` as the warm copy (`warm_mids` still
/// distinguishes warm from user code for lints/ctags policy), so it's
/// navigable and diagnosable without a second colliding module. Parsed once.
fn warm_stdlib() -> Vec<skull_parser::Ast> {
    static WARM: OnceLock<Vec<skull_parser::Ast>> = OnceLock::new();
    WARM.get_or_init(|| {
        rock_stdlib::stdlib_files()
            .iter()
            .filter_map(|f| {
                let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
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
    fallbacks: FallbackReports,
    lint_options: bone_lints::LintOptions,
    session: Session,
    texts: HashMap<String, String>,
    /// Workspace reference counts per name (`build_freq_index`) -- the
    /// `freq` prior. Rebuilt with the other type caches.
    freq: Option<HashMap<ustr::Ustr, u32>>,
    /// Per-file unfilled-hole diagnostics, cached across edits. `hole_diagnostics_for`
    /// REPARSES a file, so recomputing it for every tracked file on every keystroke
    /// reparses the whole compilation unit (over a hundred files in a large project) when only the edited
    /// file + its reinferred dependents can have changed holes. Invalidated for
    /// exactly those files in `update`.
    hole_cache: HashMap<String, Vec<Diagnostic>>,
    /// The shared lint derivation cache. It is also the cache used by the live
    /// compiler; this project stores only the LSP rendering policy at the edge.
    lint_cache: bone_lints::IncrementalLintCache,
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
    /// Fragment-scoped spellings and visibility for `call_through`. Reused on
    /// body edits and cleared with the type caches when a scope can change.
    scoped_call_through: HashMap<Ustr, ScopedCallThroughIndex>,
    /// Resolved module-v1 bare bindings and declaration metadata for
    /// completion, keyed by fragment file. Body-only updates keep the
    /// namespace fixed; interface/scope rebuilds clear the cache.
    visible_bindings: HashMap<Ustr, Vec<crate::query::VisibleBinding>>,
    /// Whether this project is a `bin` (executable/game), from its `stick.toml`
    /// `type` (default `bin`). The `unused_fn` lint is closed-world -- a `lib`'s
    /// public API has no in-project caller yet isn't dead -- so it runs only
    /// when this is true. Computed once at load, not per keystroke.
    is_bin: bool,
    /// This project's root (the `stick.toml` directory keying the session). The
    /// REPORT SET: a tracked file outside it is a DEPENDENCY -- scanned, so the
    /// call graph is whole, but never reported on. Same rule as the build pass
    /// (`rock-cli/src/lint_pass.rs`), and it matters most for `unused_fn`: a
    /// session holds its whole compilation unit, so without this a game would
    /// judge the shared engine's reachability against one consumer's world.
    root: PathBuf,
    /// Last time each module was (re)inferred, keyed by module file. Stamped for
    /// every file at load and for the `update.reinferred` set on each edit, so
    /// `rock/modules` can render "how long ago did this recompile" -- the
    /// incremental-perf watch view. Modules never yet touched are absent.
    module_touched: HashMap<String, bone_core::prelude::Instant>,
    /// The package graph of this root's manifest, built once when the
    /// project is created. It supplies the resolver's alias tables and the
    /// dialect and package of a file that enters after the load (see
    /// `update`). `None` for a root with no manifest, or one whose graph does
    /// not build (logged): every file is then a standalone legacy source,
    /// as in a compile.
    graph: Option<rock_host::stick::graph::PackageGraph>,
}

impl BoneProject {
    /// Queries needing surface syntax reparse the stored file; recovery errors
    /// stay on the partial AST and are already published by Session diagnostics.
    fn reparse_user_file(&self, path: &str, request: &'static str) -> Option<skull_parser::Ast> {
        let uf = self.session.files.get(path)?;
        self.fallbacks.result(
            skull_parser::parse_module_with_dialect(&uf.text, Ustr::from(path), uf.dialect),
            request,
            path,
            "parse stored source",
            "omit surface-syntax details; use live inference",
        )
    }
}

/// The package graph for the project rooted at `root`, if it has a manifest.
fn project_graph(root: &Path) -> Option<rock_host::stick::graph::PackageGraph> {
    if !root.join("stick.toml").exists() {
        return None;
    }
    match rock_host::stick::graph::PackageGraph::build(root) {
        Ok(graph) => Some(graph),
        Err(e) => {
            log::warn!(
                "bone LSP: package graph for `{}` does not build ({e}); its files read as legacy",
                root.display()
            );
            None
        }
    }
}

/// The `a::b::` path written right before `at` (the start of the identifier
/// being typed), as `(::-anchored, segments)`. `None` when no `::` precedes.
fn qualified_prefix_before(src: &str, at: usize) -> Option<(bool, Vec<Ustr>)> {
    let b = src.as_bytes();
    let mut end = at.min(b.len());
    let mut segments = Vec::new();
    while end >= 2 && &b[end - 2..end] == b"::" {
        let colon = end - 2;
        let mut start = colon;
        while start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
            start -= 1;
        }
        if start == colon {
            // A `::` with nothing before it anchors the path at the root.
            segments.reverse();
            return (!segments.is_empty()).then_some((true, segments));
        }
        segments.push(Ustr::from(&src[start..colon]));
        end = start;
    }
    segments.reverse();
    (!segments.is_empty()).then_some((false, segments))
}

/// Completion after a qualified path in a modules-v1 fragment
/// (`modules_scope::path_members`). `None` for a legacy file or a prefix
/// that names neither a module nor an enum, which leaves the legacy
/// `Enum::` route to try.
fn v1_path_completions(
    ws: &Workspace,
    path: &str,
    src: &str,
    at: usize,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    let frag = crate::modules_scope::fragment(ws, Ustr::from(path))?;
    let (from_root, segments) = qualified_prefix_before(src, at)?;
    let members = crate::modules_scope::path_members(ws, frag, from_root, &segments)?;
    let pfx = prefix.as_bytes();
    let mut items: Vec<CompletionItem> = members
        .into_iter()
        .filter(|(name, _)| {
            prefix.is_empty() || crate::completion::match_policy::admit(name.as_bytes(), pfx)
        })
        .enumerate()
        .map(|(i, (name, member))| {
            let kind = match member {
                crate::modules_scope::Member::Module => CompletionItemKind::MODULE,
                crate::modules_scope::Member::Variant => CompletionItemKind::ENUM_MEMBER,
                crate::modules_scope::Member::Def(key) => {
                    if ws.functions_by_name.contains_key(&key) {
                        CompletionItemKind::FUNCTION
                    } else if ws.struct_by_name.contains_key(&key) {
                        CompletionItemKind::STRUCT
                    } else if ws.enums_by_name.contains_key(&key) {
                        CompletionItemKind::ENUM
                    } else if ws.newtypes_by_name.contains_key(&key) {
                        CompletionItemKind::STRUCT
                    } else {
                        CompletionItemKind::VARIABLE
                    }
                }
            };
            CompletionItem {
                sort_text: Some(closed_set_sort_text(name.as_str(), pfx, 0, i)),
                label: name.to_string(),
                kind: Some(kind),
                ..Default::default()
            }
        })
        .collect();
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    Some(items)
}

/// The identifier at or ending at `offset` in `src`.
fn ident_around(src: &str, offset: usize) -> &str {
    let b = src.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut start = offset.min(b.len());
    while start > 0 && is_ident(b[start - 1]) {
        start -= 1;
    }
    let mut end = offset.min(b.len());
    while end < b.len() && is_ident(b[end]) {
        end += 1;
    }
    &src[start..end]
}

/// Where module `node` of `package` is declared: a fragment's `mod` header
/// naming it, else an inline `mod` block. `None` for a package root, which
/// no item declares.
fn module_declaration(session: &Session, package: u32, node: Ustr) -> Option<Span> {
    fn inline_block(
        items: &[bone_semantic::semantic_passes::ScopeItem],
        path: &mut Vec<Ustr>,
        node: Ustr,
    ) -> Option<Span> {
        for item in items {
            if let bone_semantic::semantic_passes::ScopeItem::Module {
                name,
                item_span,
                items,
                ..
            } = item
            {
                path.push(*name);
                if bone_core::modules::NsPackage::path_key(path) == node {
                    return Some(*item_span);
                }
                let found = inline_block(items, path, node);
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }
    if node.is_empty() {
        return None;
    }
    let ours = session
        .files
        .values()
        .map(|uf| &uf.contribution)
        .filter(|c| c.package.as_ref().is_some_and(|p| p.id == package));
    let mut block = None;
    for c in ours {
        let mut path: Vec<Ustr> = c
            .header
            .iter()
            .flat_map(|h| h.path.iter().map(|i| i.name))
            .collect();
        if let Some(h) = &c.header
            && bone_core::modules::NsPackage::path_key(&path) == node
        {
            return Some(h.span);
        }
        if block.is_none() {
            block = inline_block(&c.items, &mut path, node);
        }
    }
    block
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
    rock_host::stick::StickConfig::is_bin_at(root)
}

/// All bone-backed LSP state for the running server. Holds one [`Session`] per
/// stick-project root, created lazily on first edit. Multi-root publish-authority
/// routing (diamond handling) is intentionally not
/// modeled here yet -- documented gap, closes as bone grows.
#[derive(Default)]
pub struct BoneWorkspace {
    lint_options: bone_lints::LintOptions,
    projects: HashMap<PathBuf, BoneProject>,
    /// Project roots already batch-indexed from disk. The first `update` for a
    /// file in a `stick.toml` project bulk-loads its siblings so cross-file
    /// symbols resolve; later edits are incremental and must NOT re-load (that
    /// would clobber unsaved buffers).
    loaded_roots: std::collections::HashSet<PathBuf>,
    /// Discovery/read failures are distinct from a standalone source. Failed
    /// roots remain retryable so fixing the manifest or dependency can recover.
    source_load_errors: HashMap<PathBuf, String>,
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
/// Whether `src[start..end]` fills a whole delimited slot: a call argument,
/// tuple or array element, or index, with only `(`/`[`/`,` before it and
/// `)`/`]`/`,` after it. An expression spliced there keeps its precedence
/// without parens. A method receiver is followed by `.`, so it is not one.
fn is_delimited_slot(src: &str, start: usize, end: usize) -> bool {
    let before = src[..start].trim_end().as_bytes().last();
    let after = src[end..].trim_start().as_bytes().first();
    matches!(before, Some(b'(' | b'[' | b',')) && matches!(after, Some(b')' | b']' | b','))
}

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
                return true;
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
        // Unsaved editor buffers have no disk path to canonicalize; retain their identity.
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
    /// Set host lint policy before loading any project.
    pub fn with_lint_options(mut self, options: bone_lints::LintOptions) -> Self {
        assert!(
            self.projects.is_empty(),
            "lint options must precede project loading"
        );
        self.lint_options = options;
        self
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Canonicalize `path` (collapse `/./`, resolve to the OS canonical form),
    /// falling back to the input verbatim if it doesn't exist on disk (a
    /// not-yet-saved buffer). Idempotent: discovery and `root_for`/`stick_root`
    /// already canonicalize, so re-canonicalizing here yields the same key.
    fn canon_str(path: &str) -> String {
        session_file_id(canonicalize_lenient(path))
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
        self.path_canon
            .get(path)
            .cloned()
            .unwrap_or_else(|| Self::canon_str(path))
    }

    /// The deepest enclosing `stick.toml` directory for `path`, or None for a
    /// standalone file with no project. Determines whether `update` should
    /// batch-index siblings (real project) or stay single-file (scratch file).
    fn stick_root(path: &str) -> Option<PathBuf> {
        let canon = PathBuf::from(canonicalize_lenient(&editor_path(path)));
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
        let canon = PathBuf::from(canonicalize_lenient(&editor_path(path)));
        canon.parent().map(|d| d.to_path_buf()).unwrap_or(canon)
    }

    fn project_mut(&mut self, root: &Path) -> &mut BoneProject {
        let is_bin = stick_is_bin(root);
        self.projects.entry(root.to_path_buf()).or_insert_with(|| {
            let graph = project_graph(root);
            let aliases = graph
                .as_ref()
                .map(rock_host::stick::graph::PackageGraph::ns_aliases)
                // Scratch roots need no aliases; graph failures were logged by project_graph.
                .unwrap_or_default();
            BoneProject {
                fallbacks: Default::default(),
                lint_options: self.lint_options.clone(),
                // Warm-only session is always valid even if the first file
                // doesn't parse; the file is added by the update_file below.
                session: Session::new(warm_stdlib(), Vec::new(), &aliases)
                    .expect("bone session warm-init (stdlib) must succeed"),
                texts: stdlib_texts().into_iter().collect(),
                freq: None,
                hole_cache: HashMap::new(),
                lint_cache: Default::default(),
                reach: None,
                fn_sigs: None,
                call_through: None,
                scoped_call_through: HashMap::new(),
                visible_bindings: HashMap::new(),
                is_bin,
                root: root.to_path_buf(),
                module_touched: HashMap::new(),
                graph,
            }
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
        let now = bone_core::prelude::Instant::now();
        let mut out: Vec<(String, Vec<ModuleRecency>)> = self
            .projects
            .iter()
            .map(|(root, proj)| {
                let mut mods: Vec<ModuleRecency> = proj
                    .texts
                    .keys()
                    .map(|id| ModuleRecency {
                        id: editor_path(id).into_owned(),
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
        let files = match Self::project_sources(file_path) {
            Ok(files) => files,
            Err(cause) => {
                let message = format!("Cannot load project sources for `{file_path}`: {cause}");
                log::error!("bone LSP: {message}");
                self.source_load_errors.insert(root.clone(), message);
                self.loaded_roots.remove(&root);
                return;
            }
        };
        self.source_load_errors.remove(&root);
        self.loaded_roots.insert(root.clone());
        let is_bin = stick_is_bin(&root);
        let file_texts: Vec<(String, String)> = files
            .iter()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        // The alias table the resolver needs comes from the same graph that
        // selected these files, so a v1 package in the editor resolves its
        // dependencies exactly as a compile does.
        // `root` is the deepest enclosing `stick.toml` directory, or a scratch
        // file's own parent -- where there is no manifest the graph build fails
        // and the empty table is correct (a standalone source owns no package).
        let graph = project_graph(&root);
        let aliases = graph
            .as_ref()
            .map(rock_host::stick::graph::PackageGraph::ns_aliases)
            // Scratch roots need no aliases; graph failures were logged by project_graph.
            .unwrap_or_default();
        match Session::new(warm_stdlib(), files, &aliases) {
            Ok(session) => {
                let texts: HashMap<String, String> =
                    stdlib_texts().into_iter().chain(file_texts).collect();
                // Baseline every loaded file at load time, so `ago_ms` is
                // meaningful for modules not yet re-inferred by an edit.
                let now = bone_core::prelude::Instant::now();
                let module_touched = texts.keys().map(|k| (k.clone(), now)).collect();
                let mut project = BoneProject {
                    fallbacks: Default::default(),
                    lint_options: self.lint_options.clone(),
                    session,
                    texts,
                    freq: None,
                    hole_cache: HashMap::new(),
                    lint_cache: Default::default(),
                    reach: None,
                    fn_sigs: None,
                    call_through: None,
                    scoped_call_through: HashMap::new(),
                    visible_bindings: HashMap::new(),
                    is_bin,
                    root: root.clone(),
                    module_touched,
                    graph,
                };
                prime_lint_cache(&mut project);
                self.projects.insert(root, project);
            }
            Err(e) => {
                log::warn!(
                    "batch load failed for `{}` ({e:?}); falling back to per-file indexing",
                    root.display()
                );
                for (p, src) in file_texts {
                    for (file, diagnostics) in self.update(&p, src) {
                        let errors: Vec<_> = diagnostics
                            .iter()
                            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                            .map(|d| d.message.as_str())
                            .collect();
                        if !errors.is_empty() {
                            if let Some(proj) = self.project(&file) {
                                proj.fallbacks.report(
                                    "load_project",
                                    &file,
                                    "per-file update",
                                    &errors.join("; "),
                                    "retain last good session; continue indexing other files",
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// The (canonical-path, contents) of every source file in `file_path`'s
    /// compilation unit: the project plus its `stick.toml` dependencies, the same
    /// resolution `rock compile` performs. Paths are canonicalized so they match
    /// the canonical paths the LSP queries with (`root_for` / the CLI's
    /// `canonical`); a mismatch would leave the opened file's module unfindable.
    fn project_sources(file_path: &str) -> Result<Vec<SessionFile>, String> {
        let unit = rock_host::compilation_units::determine_compilation_unit_for_file(file_path)
            .map_err(|e| format!("source discovery failed: {e:#}"))?;
        let sources = rock_host::compilation_units::load_sources_for_compilation_unit(&unit)
            .map_err(|e| format!("source loading failed: {e:#}"))?;
        let mut out = Vec::new();
        for f in &sources.files {
            let f = f.borrow();
            let mut file = SessionFile::from_source(&f);
            file.path = canonicalize_lenient(&file.path);
            out.push(file);
        }
        Ok(out)
    }

    fn source_load_diagnostic(&self, path: &str) -> Option<Diagnostic> {
        self.source_load_errors
            .get(&Self::root_for(path))
            .map(|message| {
                bone_diagnostic(
                    Range::new(Position::new(0, 0), Position::new(0, 1)),
                    message.clone(),
                    rock_core::errors::CompilerErrorLevel::Error,
                )
            })
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

        if let Some(diagnostic) = self.source_load_diagnostic(path) {
            // A broken project cannot be checked as a standalone file. Retry
            // discovery on the next update instead of caching a partial session.
            return vec![(editor_path(path).into_owned(), vec![diagnostic])];
        }

        let root = Self::root_for(path);
        // Package roots use filesystem spelling, including Windows' verbatim prefix.
        let on_disk = editor_path(path);
        let graph_path = fs_err::canonicalize(on_disk.as_ref())
            .unwrap_or_else(|_| PathBuf::from(on_disk.as_ref()));
        let proj = self.project_mut(&root);
        if proj.graph.as_ref().is_some_and(|graph| {
            graph
                .package_of_file(&graph_path)
                .is_some_and(|owner| !graph.includes_file(owner, &graph_path))
        }) {
            return vec![(editor_path(path).into_owned(), Vec::new())];
        }
        if proj.texts.get(path) != Some(&text) {
            proj.fallbacks.clear();
        }
        proj.texts.insert(path.to_string(), text.clone());

        // A parse/lower failure of *this* file reverts the session to its last
        // good state for it; surface that one error on this file directly.
        // Holes can only change in files that were re-typechecked, so collect
        // the changed set (edited file + reinferred dependents) to drive the
        // hole cache below.
        let mut this_file_parse_err: Option<CompilerError> = None;
        let mut changed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut lint_changed_files = FxHashSet::default();
        let mut lint_reinferred = Vec::new();
        let mut lint_edits = Vec::new();
        let mut lint_full = false;
        changed.insert(path.to_string());
        // Tier governs the type-graph-derived caches (reachability index +
        // completion signature cache): both depend only on the type graph
        // (struct fields + fn signatures), which a `BodyOnly` edit cannot
        // change -- so rebuild only on a non-`BodyOnly` edit (or the very first
        // build), and reuse them across body keystrokes.
        let mut rebuild_type_caches = proj.reach.is_none() || proj.fn_sigs.is_none();
        // A file the session does not hold yet (created after the batch load,
        // or entering one by one after a failed batch) takes its dialect and
        // package from the manifest, exactly as the batch load's files did.
        if !proj.session.is_tracked(path) {
            let node = proj
                .graph
                .as_ref()
                .and_then(|g| rock_host::compilation_units::source_package(g, &graph_path));
            let (package, dialect, global_policy) = match node {
                Some(node) => (Some(node.tag()), node.dialect, node.global_policy),
                None => (
                    None,
                    skull_parser::Dialect::Legacy,
                    skull_parser::GlobalPolicy::Any,
                ),
            };
            if let Err(e) = proj
                .session
                .declare_file_facts(path, dialect, package, global_policy)
            {
                log::error!("bone LSP: {}", e.message);
            }
        }
        match proj.session.update_file(path, &text) {
            Ok(upd) => {
                if !matches!(upd.tier, UpdateTier::BodyOnly | UpdateTier::NoChange) {
                    rebuild_type_caches = true;
                    lint_full = true;
                }
                let ws = proj.session.workspace();
                let touched: Vec<String> = upd
                    .reinferred
                    .iter()
                    .map(|(mid, _)| ws.module(*mid).file.as_str().to_string())
                    .collect();
                let now = bone_core::prelude::Instant::now();
                for file in touched {
                    proj.module_touched.insert(file.clone(), now);
                    changed.insert(file);
                }
                match upd.tier {
                    UpdateTier::BodyOnly => {
                        lint_changed_files.insert(Ustr::from(path));
                        lint_reinferred.extend(upd.reinferred.iter().copied());
                        if let Some(edit) = upd.body_update.edit {
                            lint_edits.push(bone_lints::TextEditDelta {
                                file: edit.file,
                                at: edit.at,
                                byte_delta: edit.byte_delta,
                                line_delta: edit.line_delta,
                            });
                        }
                    }
                    UpdateTier::Interface | UpdateTier::Scope | UpdateTier::FullUser => {
                        lint_full = true
                    }
                    UpdateTier::NoChange => {}
                }
            }
            Err(e) => this_file_parse_err = Some(e),
        }
        if rebuild_type_caches {
            proj.scoped_call_through.clear();
            proj.visible_bindings.clear();
            proj.reach = Some(crate::reachability::ReachabilityIndex::build(
                proj.session.workspace(),
            ));
            proj.fn_sigs = Some(build_fn_sigs(proj.session.workspace()));
            proj.call_through = Some(build_call_through(
                proj.session.workspace(),
                proj.reach.as_ref().unwrap(),
            ));
            proj.freq = Some(build_freq_index(proj.session.workspace()));
        }

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
                // A cache miss is not persisted, so the next update retries hole analysis.
                proj.hole_cache.get(p).cloned().unwrap_or_default()
            };
            diags.extend(holes);
        }

        if this_file_parse_err.is_some() {
            proj.lint_cache.invalidate_file(Ustr::from(path));
        } else {
            let lint_change = if lint_full {
                bone_lints::LintCacheUpdate::full()
            } else {
                bone_lints::LintCacheUpdate::body_only(
                    lint_changed_files,
                    lint_reinferred,
                    lint_edits,
                )
            };
            let findings = update_lint_cache(proj, &lint_change);
            for finding in findings {
                by_file
                    .entry(finding.file().as_str().to_string())
                    .or_default()
                    .push(finding_diagnostic(&finding));
            }
        }

        by_file
            .into_iter()
            .map(|(file, diags)| (editor_path(&file).into_owned(), diags))
            .collect()
    }

    /// Drop a closed/removed file from its session.
    pub fn remove(&mut self, path: &str) {
        let path_c = self.canon_cached(path);
        let path = path_c.as_str();
        let root = Self::root_for(path);
        if let Some(proj) = self.projects.get_mut(&root) {
            proj.fallbacks.clear();
            proj.texts.remove(path);
            proj.hole_cache.remove(path);
            proj.lint_cache.invalidate_file(Ustr::from(path));
            match proj.session.remove_file(path) {
                Ok(update) if !matches!(update.tier, UpdateTier::NoChange) => {
                    // Removal changes the workspace interface just like an
                    // interface edit. Drop every workspace-derived index,
                    // including fragment-scoped spellings, before the next
                    // completion request rebuilds them.
                    proj.reach = None;
                    proj.fn_sigs = None;
                    proj.call_through = None;
                    proj.scoped_call_through.clear();
                    proj.visible_bindings.clear();
                    proj.freq = None;
                }
                Ok(_) => {}
                Err(e) => log::error!("bone LSP: removing `{path}`: {}", e.message),
            }
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
        if src
            .as_bytes()
            .get(offset)
            .is_none_or(|b| b.is_ascii_whitespace())
        {
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
        let raw_ast = proj.reparse_user_file(path, "hover");
        if let Some(hi) = raw_ast
            .as_ref()
            .and_then(|ast| crate::query::hole_info(ws, mid, inf, ast, offset))
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

        // Enum-variant match patterns (`MobKind::Skeleton`, incl. the `::`) are
        // resolved from the raw AST FIRST, so a pattern wins over the enclosing
        // match expr (which now renders its own value for its keyword/braces).
        // bone's HIR pattern carries no span, hence the raw-AST read.
        let pat_hover = raw_ast
            .as_ref()
            .and_then(|ast| crate::query::enum_variant_pattern_hover(ws, ast, offset));
        // Node resolution runs first (so a param NAME/TYPE in the signature
        // resolves to itself); the fn-signature hover is the FALLBACK for the
        // signature tokens no node covers (`fn`/parens/`->`), bounded by `src`
        // so body keywords (`let`/`for`) past the brace aren't swallowed.
        let (contents, span) = pat_hover
            .or_else(|| crate::query::hover_at(ws, mid, inf, offset, Some(&proj.texts)))
            .or_else(|| {
                crate::query::fn_signature_hover(ws, mid, src, offset, Some(&proj.texts))
            })?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: contents,
            }),
            range: Some(range_from_offsets(
                src,
                span.offset_range.0,
                span.offset_range.1,
            )),
        })
    }

    /// All diagnostics for a single file (used by the one-shot CLI handler):
    /// bone's typecheck errors plus the unfilled-hole warnings/hints.
    pub fn diagnostics_for(&self, path: &str) -> Vec<Diagnostic> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        if let Some(diagnostic) = self.source_load_diagnostic(path) {
            return vec![diagnostic];
        }
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
        // Unavailable hole analysis contributes no hints; hard parse failures are logged inside.
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
        proj.lint_cache
            .findings_for_file(Ustr::from(path))
            .iter()
            .map(finding_diagnostic)
            .collect()
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
        // Raw-AST resolutions run FIRST (the HIR node isn't in `node_index`):
        // a macro INVOCATION name (erased by expansion -- otherwise the cursor
        // lands on a spliced body token), then enum-variant match patterns.
        let raw = proj.reparse_user_file(path, "definition");
        if let Some(ast) = &raw {
            if let Some(span) =
                macro_refs::macro_invocation_def(ast, &proj.session.registry, offset)
            {
                return self.location_from_span(proj, &span);
            }
        }
        // A modules-v1 path segment navigates by what the resolver recorded
        // for that segment: a module to its header, a definition or variant
        // to its declaration -- not to whatever the whole path lowered to.
        if let Some(seg) = crate::query::path_segment_at(ws, mid, offset) {
            let span = match seg.target {
                bone_semantic::hir::SegmentTarget::Module { package, node } => {
                    module_declaration(&proj.session, package, node)
                }
                target => crate::query::segment_target_def(ws, target),
            }?;
            return self.location_from_span(proj, &span);
        }
        let pat_def = raw
            .as_ref()
            .and_then(|ast| crate::query::enum_variant_pattern_def(ws, ast, offset));
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
        self.all_reference_spans(
            proj,
            path,
            offset,
            crate::query::FnRefScope::ThisOverload,
            "references",
        )
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
        // A path with no URI has no locations either: `references` reports
        // that failure when it builds them.
        let Ok(want) = Uri::from_file_path(&editor_path(path)) else {
            return Vec::new();
        };
        self.references(path, pos)
            .into_iter()
            .filter(|loc| loc.uri == want)
            .map(|loc| DocumentHighlight {
                range: loc.range,
                kind: None,
            })
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
        let ast = proj.reparse_user_file(path, "hole_info")?;
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
        let parenthesized = format!("({init_text})");

        let mut edits = Vec::with_capacity(sites.uses.len() + 1);
        for u in &sites.uses {
            let (s, e) = u.offset_range;
            let new_text = if parenthesize && !is_delimited_slot(src, s, e) {
                parenthesized.clone()
            } else {
                init_text.to_string()
            };
            edits.push(TextEdit {
                range: Range {
                    start: offset_to_position(src, s),
                    end: offset_to_position(src, e),
                },
                new_text,
            });
        }
        // Delete the whole `let` line: leading indent through the trailing newline.
        let (ls, le) = sites.let_span.offset_range;
        let line_start = src.get(..ls)?.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = src
            .get(le..)?
            .find('\n')
            .map(|i| le + i + 1)
            .unwrap_or(src.len());
        edits.push(TextEdit {
            range: Range {
                start: offset_to_position(src, line_start),
                end: offset_to_position(src, line_end),
            },
            new_text: String::new(),
        });

        Some(InlineLocalEdits {
            var: sites.var.to_string(),
            edits,
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
        // No concrete result type means no generated return annotation.
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
        request: &'static str,
    ) -> Vec<crate::query::RefSite> {
        if let Some(ast) = proj.reparse_user_file(path, request) {
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

    /// The sites a rename at `offset` rewrites: [`Self::v1_rename_sites`]
    /// for a modules-v1 definition or import alias, else every reference of
    /// the symbol, all overloads of a function included.
    fn rename_sites(
        &self,
        proj: &BoneProject,
        path: &str,
        offset: usize,
    ) -> Vec<crate::query::RefSite> {
        self.v1_rename_sites(proj, path, offset).unwrap_or_else(|| {
            self.all_reference_spans(
                proj,
                path,
                offset,
                crate::query::FnRefScope::AllOverloads,
                "rename",
            )
        })
    }

    /// Rename sites that tell a definition from an import alias.
    /// Renaming a modules-v1 definition rewrites every site that spells its
    /// name -- its declaration, bare uses, the naming segment of each path,
    /// `use` items and re-exports -- and not the local name an alias binds.
    /// Renaming an alias (`use a::f as g`, at the `g` or at a use of it)
    /// rewrites that alias and the uses of `g` in its own file. `None` when
    /// the cursor is on neither.
    fn v1_rename_sites(
        &self,
        proj: &BoneProject,
        path: &str,
        offset: usize,
    ) -> Option<Vec<crate::query::RefSite>> {
        use bone_semantic::hir::{PathSegmentRef, SegmentTarget};
        let ws = proj.session.workspace();
        let mid = user_module_for_path(ws, path)?;
        let text_at = |span: &Span| {
            proj.texts
                .get(span.file.as_str())
                .and_then(|t| t.get(span.offset_range.0..span.offset_range.1))
        };
        let (key, alias) = match crate::query::path_segment_at(ws, mid, offset) {
            Some(PathSegmentRef {
                target: SegmentTarget::Def(key),
                alias: true,
                span,
            }) => (key, Some(text_at(&span)?.to_string())),
            Some(PathSegmentRef {
                target: SegmentTarget::Def(key),
                ..
            }) => (key, None),
            Some(_) => return None,
            None => {
                let key =
                    crate::query::v1_def_named_at(ws, Ustr::from(path), offset).or_else(|| {
                        let def = gaps::definition(ws, mid, ws.inference(mid)?, offset)?;
                        crate::query::v1_def_declared_at(ws, &def)
                    })?;
                let src = proj.texts.get(path)?;
                let word = ident_around(src, offset);
                let is_alias = word != ws.defs.get(&key)?.name.as_str()
                    && crate::modules_scope::fragment(ws, Ustr::from(path))
                        .is_some_and(|frag| frag.named.contains_key(&Ustr::from(word)));
                (key, is_alias.then(|| word.to_string()))
            }
        };
        let name = ws.defs.get(&key)?.name;
        let decl = crate::query::segment_target_def(ws, SegmentTarget::Def(key))?;
        // Every site of the definition: the reference walks from its
        // declaration, then each recorded path segment naming it (a walk
        // reports a qualified call by the whole path's span, which no site
        // spelling check below keeps).
        let mut sites: Vec<Span> = self
            .all_reference_spans(
                proj,
                decl.file.as_str(),
                decl.offset_range.0,
                crate::query::FnRefScope::AllOverloads,
                "rename",
            )
            .into_iter()
            .map(|site| site.span)
            .collect();
        sites.extend(
            crate::query::segments_naming(ws, SegmentTarget::Def(key))
                .into_iter()
                .filter(|seg| alias.is_some() || !seg.alias)
                .map(|seg| seg.span),
        );
        let spelling = alias.as_deref().unwrap_or(name.as_str());
        sites.retain(|span| {
            text_at(span) == Some(spelling) && (alias.is_none() || span.file.as_str() == path)
        });
        sites.sort_by_key(|span| (span.file.as_str().to_string(), span.offset_range));
        sites.dedup_by_key(|span| (span.file, span.offset_range));
        Some(
            sites
                .into_iter()
                .map(|span| crate::query::RefSite {
                    span,
                    shorthand: None,
                })
                .collect(),
        )
    }

    // -- Feature: completion (the request lives in `completion.rs`) ---------------

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
        let mut items = self.completion_items(path, pos, space_triggered);
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

    // -- Feature: rename + prepare-rename (stubbed: gaps::references) --------

    /// The identifier range a rename at `pos` would rewrite: `Ok(None)` when
    /// nothing there is renameable, `Err` when the symbol is renameable in
    /// principle but not safely (the reason says why).
    pub fn prepare_rename(
        &self,
        path: &str,
        pos: Position,
    ) -> Result<Option<Range>, RenameRefusal> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Ok(None);
        };
        let Some(src) = proj.texts.get(path) else {
            return Ok(None);
        };
        let offset = offset_at_line_col(src, pos.line, pos.character);
        // Reuse the exact reference resolution `rename` uses, so prepare-rename
        // and rename can never disagree: the token is renameable iff it has
        // reference sites, and the editable range is the cursor's own site (the
        // identifier token). `gaps::definition` + `type_at` did neither -- None
        // at a declaration name ("nothing to rename" on `fn foo`), and the whole
        // enclosing `Call` span at a callee (prefilling `foo(args)`, not `foo`).
        let here = self
            .rename_plan(proj, path, offset)?
            .into_iter()
            .map(|s| s.span)
            .find(|s| {
                s.file.as_str() == path && s.offset_range.0 <= offset && offset < s.offset_range.1
            });
        Ok(here.map(|here| range_from_offsets(src, here.offset_range.0, here.offset_range.1)))
    }

    /// The edits renaming the symbol at `pos` to `new_name`: `Ok(None)` when
    /// nothing there is renameable, `Err` when the rename cannot be done
    /// safely or `new_name` is not an identifier.
    pub fn rename(
        &self,
        path: &str,
        pos: Position,
        new_name: &str,
    ) -> Result<Option<WorkspaceEdit>, RenameRefusal> {
        let path_c = self.canon(path);
        let path = path_c.as_str();
        let Some(proj) = self.project(path) else {
            return Ok(None);
        };
        let Some(src) = proj.texts.get(path) else {
            return Ok(None);
        };
        let offset = offset_at_line_col(src, pos.line, pos.character);
        // Rename rewrites the whole (shared) overloaded name, not one overload.
        let sites = self.rename_plan(proj, path, offset)?;
        if sites.is_empty() {
            return Ok(None);
        }
        check_new_name(new_name)?;

        #[expect(
            clippy::mutable_key_type,
            reason = "WorkspaceEdit Uri keys use immutable text for equality and hashing"
        )]
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
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    /// The sites a rename at `offset` rewrites, or why it must not run.
    fn rename_plan(
        &self,
        proj: &BoneProject,
        path: &str,
        offset: usize,
    ) -> Result<Vec<crate::query::RefSite>, RenameRefusal> {
        Ok(self.rename_sites(proj, path, offset))
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
                    kind: entry_symbol_kind(s.kind, s.is_const),
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

    /// All top-level symbols across every loaded project whose own name matches
    /// `query` (case-insensitive substring; empty query = everything). Queries
    /// containing `::` match the qualified name instead. Builtin `@test`
    /// functions follow non-test symbols, preserving order within each group.
    /// Returned names retain their qualification. Visits every tracked `.rock`
    /// file, reusing the per-module `crate::query::document_symbols` walk. Spans every
    /// project the session has loaded (the one-shot CLI loads exactly one; the
    /// interactive server accumulates each opened root).
    pub fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        let needle = query.to_lowercase();
        let qualified = needle.contains("::");
        let mut out = Vec::new();
        let mut tests = Vec::new();
        for proj in self.projects.values() {
            let ws = proj.session.workspace();
            for (file, src) in proj.texts.iter() {
                let Some(mid) = user_module_for_path(ws, file) else {
                    continue;
                };
                let uri = match Uri::from_file_path(&editor_path(file)) {
                    Ok(uri) => uri,
                    Err(error) => {
                        proj.fallbacks.report(
                            "workspace symbols",
                            file,
                            "construct URI",
                            &error.to_string(),
                            "omit symbols from this file; keep other files",
                        );
                        continue;
                    }
                };
                // Built lazily on the first match in this file: a Ctrl-T query
                // skips most files, so don't index a file with no hit. Without it
                // every matched symbol re-scanned from the file start (O(offset)).
                let mut line_index: Option<LineIndex> = None;
                let test_spans: FxHashSet<_> = ws.modules[mid.index()]
                    .functions
                    .iter()
                    .filter(|(_, f)| f.annotations.iter().any(|a| a.is_builtin("test")))
                    .map(|(_, f)| f.span.offset_range)
                    .collect();
                for s in crate::query::document_symbols(ws, mid) {
                    let name = if qualified {
                        s.name.as_str()
                    } else {
                        s.name
                            .rsplit_once("::")
                            .map_or(s.name.as_str(), |(_, name)| name)
                    };
                    if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                        continue;
                    }
                    let li = line_index.get_or_insert_with(|| LineIndex::new(src));
                    let range = li.range(src, s.span.offset_range.0, s.span.offset_range.1);
                    let target = if matches!(s.kind, SymCat::Function)
                        && test_spans.contains(&s.span.offset_range)
                    {
                        &mut tests
                    } else {
                        &mut out
                    };
                    #[allow(deprecated)]
                    target.push(SymbolInformation {
                        name: s.name,
                        kind: entry_symbol_kind(s.kind, s.is_const),
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: uri.clone(),
                            range,
                        },
                        container_name: None,
                    });
                }
            }
        }
        out.extend(tests);
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
        // (written under its real path, `editor_path`), so `vec_push` & co. are
        // jump-to-by-name; only the builtin (pathless) module is excluded.
        for m in ws.modules.iter() {
            if m.id == ModuleId::builtin() {
                continue;
            }
            let mid = m.id;
            let file = editor_path(m.file.as_str()).into_owned();
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
            if let Some(uf) = proj.session.files.get(m.file.as_str()) {
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
        let (sigs, call_span) =
            crate::query::signature_help(ws, mid, inf, offset, Some(&proj.texts))?;
        let call_start = call_span.offset_range.0;
        let paren_start = active_call_paren_start(src, call_start, offset);
        // Active argument = top-level commas between the call's OWN `(` (not
        // necessarily the first `(` after `call_start` -- a UFCS receiver
        // that is itself a call, `get_screen().rect_outline(|)`, has its own
        // sibling paren pair) and the cursor.
        let mut active = paren_start.map_or(0, |p| active_param(src, p, offset));
        // UFCS: `recv.callee(..)` folds the receiver into HIR arg 0 (the
        // signature's params include it, e.g. `dl: DrawList` ahead of
        // `pos: Vec2`), but the WRITTEN paren list the cursor sits in only
        // has the trailing args -- `active_param`'s comma count is 0-based
        // over that written list alone. Shift by one so a dot-call cursor in
        // the first written arg highlights the SECOND declared param, not
        // the receiver's.
        if paren_start.is_some_and(|p| call_is_ufcs_dot_syntax(src, call_start, p)) {
            active += 1;
        }
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
            reparse_with_project(
                &proj.session,
                &proj.fallbacks,
                path,
                &balance_delimiters(src.to_string()),
                "inlay",
            )
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
        let recv_hints = proj
            .fallbacks
            .result(
                skull_parser::parse_module(src, Ustr::from(path)),
                "inlay",
                path,
                "parse receiver hints",
                "omit receiver hints; keep inferred hints",
            )
            .map(|ast| crate::query::receiver_param_hints(&ast, start, end))
            // The parse refusal above was logged; retain other inferred hints.
            .unwrap_or_default()
            .into_iter()
            .map(|(span, label)| gaps::HintEntry {
                span,
                label,
                leading: false,
            });
        gaps::inlay_hints(ws, mid, inf, start, end)
            .into_iter()
            .chain(recv_hints)
            .map(|h| {
                // A leading (parameter-name) hint renders before its span with
                // trailing padding; a type hint renders after with leading padding.
                let (offset, kind, pad_left, pad_right) = if h.leading {
                    (
                        h.span.offset_range.0,
                        InlayHintKind::PARAMETER,
                        None,
                        Some(true),
                    )
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
        let uri = match Uri::from_file_path(&editor_path(file)) {
            Ok(uri) => uri,
            Err(error) => {
                proj.fallbacks.report(
                    "span location",
                    file,
                    "construct URI",
                    &error.to_string(),
                    "omit location",
                );
                return None;
            }
        };
        let text = match proj.texts.get(file) {
            Some(text) => Cow::Borrowed(text.as_str()),
            None => match fs_err::read_to_string(file) {
                Ok(text) => Cow::Owned(text),
                Err(error) => {
                    proj.fallbacks.report(
                        "span location",
                        file,
                        "read source",
                        &error.to_string(),
                        "omit location",
                    );
                    return None;
                }
            },
        };
        Some((uri, text))
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
        let Some(toks) = proj.fallbacks.result(
            rock_tokenizer::Tokenizer::new(src, Ustr::from(path)).tokenize(),
            "semantic tokens",
            path,
            "tokenize source",
            "empty token response",
        ) else {
            return SemanticTokens::default();
        };
        let ws = proj.session.workspace();
        let mut names = semantic_name_map(ws);
        // Macros are expanded away before bone lowering, so the workspace HIR
        // has no record of them; the Session's parse-time capture supplies the
        // names (def sites and call sites are both bare ident tokens).
        for (_, md) in proj.session.all_macro_defs() {
            names.entry(md.name).or_insert(TT_MACRO);
        }
        let hir = user_module_for_path(ws, path)
            .map(|mid| semantic_classifications(ws, mid, range))
            // A missing HIR module still permits lexical tokens for a partial buffer.
            .unwrap_or_default();

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
            // A GLUED `@ident` is an annotation
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
                rock_tokenizer::TokenKind::Comment(body)
                    if is_magic_rock_directive(body.as_str()) =>
                {
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
        SemanticTokenType::KEYWORD,        // 0
        SemanticTokenType::VARIABLE,       // 1
        SemanticTokenType::FUNCTION,       // 2
        SemanticTokenType::STRUCT,         // 3
        SemanticTokenType::ENUM,           // 4
        SemanticTokenType::TYPE,           // 5
        SemanticTokenType::NUMBER,         // 6
        SemanticTokenType::STRING,         // 7
        SemanticTokenType::COMMENT,        // 8
        SemanticTokenType::PROPERTY,       // 9
        SemanticTokenType::PARAMETER,      // 10
        SemanticTokenType::ENUM_MEMBER,    // 11
        SemanticTokenType::TYPE_PARAMETER, // 12
        SemanticTokenType::DECORATOR,      // 13
        SemanticTokenType::MACRO,          // 14
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
            if !is_member && n.as_str() == "static" && matches!(next_kind, Some(T::Ident(_))) =>
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
        T::Fn
        | T::Operator
        | T::Table
        | T::Let
        | T::Return
        | T::Defer
        | T::Yield
        | T::Await
        | T::Struct
        | T::Newtype
        | T::If
        | T::Else
        | T::For
        | T::Loop
        | T::In
        | T::While
        | T::Break
        | T::Continue
        | T::Null
        | T::Enum
        | T::Match
        | T::Extern
        | T::Global
        | T::Const
        | T::Type
        | T::As
        | T::Deco
        | T::Macro
        | T::True
        | T::False
        | T::DirectiveIf
        | T::DirectiveElseIf
        | T::DirectiveElse
        | T::DirectiveEnd => (TT_KEYWORD, 0),
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
            // annotations that never reach hir.
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
                    for fi in *fields {
                        let f = &body.struct_lit_field_arena[fi];
                        map.insert(f.name_span.offset_range.0, (TT_PROPERTY, 0));
                    }
                }
                // Interpolation holes: each `{expr}`'s expr span (real in-file
                // offsets from the parser) lets the emitter split the string
                // token and color the hole as the expression it is.
                Expr::InterpolatedString(parts) => {
                    for pi in *parts {
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
    SemanticClassifications {
        by_offset: map,
        interp_holes: holes,
    }
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
    Range::new(
        offset_to_position(src, start),
        offset_to_position(src, stop),
    )
}

/// Leading whitespace of the line containing byte `pos`. Used to indent
/// generated code (fill-struct-fields) to match the surrounding source.
fn line_indent_at(src: &str, pos: usize) -> String {
    let pos = pos.min(src.len());
    let line_start = src
        .get(..pos)
        .and_then(|s| s.rfind('\n'))
        .map_or(0, |i| i + 1);
    src.get(line_start..)
        .unwrap_or("")
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// Byte offset of the call's OWN opening `(` -- the top-level paren group
/// that is still unclosed at `offset` (i.e. actually contains the cursor).
/// `call_start` is the byte offset of the whole call EXPRESSION, which for a
/// UFCS/method call includes the receiver (`get_screen().rect_outline(|)`
/// starts at `get_screen`, not `rect_outline`); a receiver that is itself a
/// call has its own top-level paren pair, fully closed before the real one
/// opens, so tracking depth alone and taking the FIRST `(` would anchor on
/// the receiver's parens instead. Depth resets to 0 between sibling groups,
/// so the position is retaken (and forgotten again if that group closes
/// too) every time depth returns to 0 -- the group left open at `offset` is
/// the answer. `None` when no paren is still open at `offset` (shouldn't
/// happen for a real call/struct-literal site, but the caller degrades to
/// arg 0 / non-UFCS rather than panic).
fn active_call_paren_start(src: &str, call_start: usize, offset: usize) -> Option<usize> {
    let b = src.as_bytes();
    let end = offset.min(b.len());
    let start = call_start.min(end);
    let mut depth = 0i32;
    let mut active: Option<usize> = None;
    let mut in_str = false;
    for (i, &c) in b[start..end].iter().enumerate() {
        match c {
            b'"' => in_str = !in_str,
            b'(' if !in_str => {
                if depth == 0 {
                    active = Some(start + i);
                }
                depth += 1;
            }
            b')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    active = None;
                }
            }
            _ => {}
        }
    }
    active
}

/// Active argument index for signature help: count of top-level commas
/// between the call's own opening `(` (`paren_start`, from
/// `active_call_paren_start`) and the cursor. Depth tracks nested parens so a
/// comma inside a nested call doesn't advance the outer arg; a naive
/// string-skip keeps commas inside string literals from counting (no escape
/// handling -- fine for a cursor heuristic).
fn active_param(src: &str, paren_start: usize, offset: usize) -> u32 {
    let end = offset.min(src.len());
    let start = paren_start.min(end);
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

/// Whether the call whose own opening paren sits at `paren_start` is written
/// as `recv.name(..)` (a UFCS/method dot-call) rather than a bare `name(..)`:
/// the identifier immediately before `paren_start` is itself preceded by a
/// `.`. `call_start` (the whole call expression's start) bounds the
/// backward walk so it can't run past the receiver into unrelated code.
fn call_is_ufcs_dot_syntax(src: &str, call_start: usize, paren_start: usize) -> bool {
    let b = src.as_bytes();
    let start = call_start.min(b.len());
    let mut j = paren_start.min(b.len()).max(start);
    while j > start && (b[j - 1].is_ascii_alphanumeric() || b[j - 1] == b'_') {
        j -= 1;
    }
    while j > start && (b[j - 1] == b' ' || b[j - 1] == b'\t') {
        j -= 1;
    }
    j > start && b[j - 1] == b'.'
}

/// Whether any accessible overload of `callee` declares arg `arg_index` as a
/// `FieldRef<..>` slot, decided on the param's nominal head. None when no
/// overload is known live (unknown callee / arity) -- the caller treats that
/// as "can't rule it out".
pub(crate) fn callee_slot_is_field_of_in_scope(
    ws: &Workspace,
    frag: Option<&bone_core::modules::FragmentScope>,
    callee: &str,
    arg_index: usize,
) -> Option<bool> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    let name = Ustr::from(callee);
    let mut any = false;
    let mut found = false;
    for m in ws.modules.iter() {
        let Some(fns) = m.functions_by_name.get(&name) else {
            continue;
        };
        for (fmid, fid) in fns {
            if frag.is_some_and(|frag| {
                !crate::modules_scope::callable_member_admitted(ws, frag, name, *fmid, *fid)
            }) {
                continue;
            }
            let f = &ws.modules[fmid.index()].functions[*fid];
            let Some(p) = f.param_decls.get(arg_index) else {
                continue;
            };
            any = true;
            let te = match p.ty {
                TyId::Static(pmid, idx) => ws.module(pmid).ty_arena[idx],
                TyId::Local(_) => continue,
            };
            if matches!(te, TypeExpr::App { ctor: TypeCtor::Nominal(n), .. } if is_field_ref_former(n))
            {
                found = true;
            }
        }
    }
    any.then_some(found)
}

/// Typed expected-type oracle for one completion request's arg slot.
/// The callee's surviving overloads' params are
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
        let frag = crate::modules_scope::fragment(ws, ws.module(mid).file);
        let mut slots = Vec::new();
        let mut rendered: Vec<String> = Vec::new();
        for m in ws.modules.iter() {
            let Some(fns) = m.functions_by_name.get(&name) else {
                continue;
            };
            for (fmid, fid) in fns {
                if frag.is_some_and(|frag| {
                    !crate::modules_scope::callable_member_admitted(ws, frag, name, *fmid, *fid)
                }) {
                    continue;
                }
                let f = &ws.modules[fmid.index()].functions[*fid];
                if f.param_decls.len() <= arg_idx {
                    continue;
                }
                let params: Vec<TyId> = f.param_decls.iter().map(|p| p.ty).collect();
                // Speculative overload instantiation may reject a candidate; try the next overload.
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
            let hit = self
                .gamma
                .trial(|g| inference::coerce(g, ty, slot, span).is_ok());
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
        let frag = crate::modules_scope::fragment(ws, ws.module(self.gamma.current_module_id).file);
        for m in ws.modules.iter() {
            let Some(fns) = m.functions_by_name.get(&n) else {
                continue;
            };
            for (fmid, fid) in fns {
                if frag.is_some_and(|frag| {
                    !crate::modules_scope::callable_member_admitted(ws, frag, n, *fmid, *fid)
                }) {
                    continue;
                }
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
                    // A rejected speculative instantiation is a type non-match, not a query failure.
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

/// The return TyId shared by ALL accessible overloads of `name`, or None if the
/// overloads disagree / any lacks a return type. Agreement is semantic type
/// equivalence across the overloads' Static arenas, so two overloads' own
/// generic `T` (or a generic `T` and a struct `T`) disagree.
pub(crate) fn fn_unique_return_ty_in_scope(
    ws: &Workspace,
    name: &str,
    frag: Option<&bone_core::modules::FragmentScope>,
) -> Option<TyId> {
    let n = Ustr::from(name);
    let mut ret: Option<TyId> = None;
    for m in ws.modules.iter() {
        let Some(fns) = m.functions_by_name.get(&n) else {
            continue;
        };
        for (fmid, fid) in fns {
            if frag.is_some_and(|frag| {
                !crate::modules_scope::callable_member_admitted(ws, frag, n, *fmid, *fid)
            }) {
                continue;
            }
            let f = &ws.modules[fmid.index()].functions[*fid];
            let t = f.return_ty?;
            match ret {
                None => ret = Some(t),
                Some(prev) if ws.types_equivalent(TyRef::of_static(prev), TyRef::of_static(t)) => {}
                Some(_) => return None, // overloads disagree -> can't decide
            }
        }
    }
    ret
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
pub(crate) fn ty_of(
    ws: &Workspace,
    body: Option<&BodyInference>,
    ty: TyId,
) -> bone_semantic::hir::TypeExpr {
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
                    return pp.into_iter().zip(ap).find_map(|(pi, ai)| {
                        bind_generic(
                            ws,
                            target,
                            p_body,
                            param.with_idx(pi),
                            a_body,
                            arg.with_idx(ai),
                        )
                    });
                }
            }
            None
        }
        TypeExpr::Nullable(pi) => match ty_of(ws, a_body, arg) {
            TypeExpr::Nullable(ai) => bind_generic(
                ws,
                target,
                p_body,
                param.with_idx(pi),
                a_body,
                arg.with_idx(ai),
            ),
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
        TypeExpr::App {
            ctor: TypeCtor::Array,
            ..
        }
        | TypeExpr::App {
            ctor: TypeCtor::Map,
            ..
        } => true,
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            ..
        } => is_intrinsic_key_in(
            n,
            &[
                "Arena",
                "NativeBuffer",
                "NativeBufferU8",
                "NativeBufferI8",
                "NativeBufferU16",
                "NativeBufferI16",
                "NativeBufferU32",
                "NativeBufferI32",
            ],
        ),
        _ => false,
    }
}

/// The base nominal (struct) name of `ty`, if it is a `Nominal` App.
pub(crate) fn nominal_base(ws: &Workspace, body: Option<&BodyInference>, ty: TyId) -> Option<Ustr> {
    use bone_semantic::hir::{TypeCtor, TypeExpr};
    match ty_of(ws, body, ty) {
        TypeExpr::App {
            ctor: TypeCtor::Nominal(name),
            ..
        } => Some(name),
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
/// recorder can attribute per-keystroke latency to the
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
/// THIS THREAD. For the corpus recorder.
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
    let line_start = src
        .get(..offset)
        .and_then(|s| s.rfind('\n'))
        .map_or(0, |i| i + 1);
    src.get(line_start..offset)
        .is_some_and(|p| p.chars().all(|c| c == ' ' || c == '\t'))
}

/// Recover completion or inlay queries against the whole project, replacing
/// the current file with balanced source. A single-file semantic pass is the
/// last recovery rung when project analysis fails.
fn reparse_with_project(
    session: &Session,
    fallbacks: &FallbackReports,
    path: &str,
    munged: &str,
    request: &'static str,
) -> Option<Workspace> {
    let combined = fallbacks.result(
        session.combined_unexpanded(),
        request,
        path,
        "combined-module parse",
        "use live workspace",
    )?;
    reparse_with_project_inner(&combined, path, munged, fallbacks, request)
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
                "completion: session refused the variant for `{path}` ({}); single-file fallback",
                e.message
            );
            let fallback = skull_parser::parse_module(munged, Ustr::from(path))
                .and_then(|m| bone_semantic::semantic_passes::semantic_pass(&mut [m]));
            match fallback {
                Ok(ws) => Some(f(&ws)),
                Err(error) => {
                    log::warn!(
                        "completion: single-file fallback also failed for `{path}`: {}; \
                         this munge yields no completions",
                        error.message
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
    reports: &FallbackReports,
    request: &'static str,
) -> Option<Workspace> {
    // Project-wide pass: every module except the current file, plus the munge.
    // Resolves cross-file / dependency types (an `IVec2` engine param, etc.).
    let mut modules: Vec<skull_parser::Ast> = combined
        .iter()
        .filter(|a| a.file.as_str() != path)
        .cloned()
        .collect();
    // The munge is a half-typed buffer (missing bracket, `recv.` mid-edit). The
    // parse is total -- skull-parser drops the broken statement and keeps the rest
    // (the recovery error stays on `recovery_errors`, which the LSP path never
    // gates on) -- so completion still resolves from the best-effort tree (backlog
    // #21). Only a hard tokenizer failure is `Err`/`None` here.
    let parsed = reports.result(
        skull_parser::parse_module(munged, Ustr::from(path)),
        request,
        path,
        "parse recovery source",
        "use live workspace",
    )?;
    modules.push(parsed.clone());
    if let Some(ws) = reports.result(
        bone_semantic::semantic_passes::semantic_pass(&mut modules),
        request,
        path,
        "project semantic recovery",
        "try single-file semantic recovery",
    ) {
        return Some(ws);
    }
    reports.result(
        bone_semantic::semantic_passes::semantic_pass(&mut [parsed]),
        request,
        path,
        "single-file semantic recovery",
        "use live workspace",
    )
}

/// Whether a reach-path suffix (`a().b().c`, UFCS-chained) has a CALL hop to
/// `fname` -- i.e. the path produces its value by calling that fn. Each `.`-
/// separated hop's identifier is the text before its `(`; compared exactly, so
/// `my_style()` does not match `style`.
pub(crate) fn reach_suffix_calls(suffix: &str, fname: &str) -> bool {
    suffix
        .split('.')
        .any(|hop| hop.split('(').next().unwrap_or(hop) == fname)
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

fn bone_diagnostic(
    range: Range,
    message: String,
    level: rock_core::errors::CompilerErrorLevel,
) -> Diagnostic {
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
            let msg = if l.message.is_empty() {
                e.message.clone()
            } else {
                l.message.clone()
            };
            (s.file, bone_diagnostic(range, msg, e.level))
        })
        .collect()
}

/// Render a lint finding as an LSP warning tagged with its lint name in the
/// `code` field, so the "Suppress with `// rock: allow(<lint>)`" code action
/// identifies lint squiggles robustly (by `code`, not by message-sniffing).
fn finding_diagnostic(f: &bone_lints::Finding) -> Diagnostic {
    let mut d = compiler_error_to_diagnostic(&f.error);
    d.code = Some(NumberOrString::String(f.lint.to_string()));
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
    let end = tail
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(tail.len());
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

fn update_lint_cache(
    proj: &mut BoneProject,
    change: &bone_lints::LintCacheUpdate,
) -> Vec<bone_lints::Finding> {
    let session = &proj.session;
    let texts = &proj.texts;
    let root = &proj.root;
    let is_bin = proj.is_bin;
    let sources: FxHashMap<Ustr, &str> = texts
        .iter()
        .filter_map(|(path, text)| {
            Path::new(path)
                .starts_with(root)
                .then_some((Ustr::from(path.as_str()), text.as_str()))
        })
        .collect();
    proj.lint_cache
        .update(
            session.workspace(),
            &session.warm_mids,
            is_bin,
            &sources,
            &proj.lint_options,
            change,
        )
        .0
}

fn prime_lint_cache(proj: &mut BoneProject) {
    // Findings remain in the cache for lint_diagnostics; loading has no response.
    drop(update_lint_cache(
        proj,
        &bone_lints::LintCacheUpdate::full(),
    ));
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
    let ast = proj.reparse_user_file(path, "hole_diagnostics")?;
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
                code: matches!(kind, crate::query::HoleKind::Expr)
                    .then(|| lsp_types::NumberOrString::String("unfilled_expression_hole".into())),
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
        for m in ws
            .modules
            .iter()
            .filter(|m| !rock_stdlib::is_stdlib_module_file(m.file.as_str()))
        {
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

fn entry_symbol_kind(kind: SymCat, is_const: bool) -> SymbolKind {
    if is_const {
        SymbolKind::CONSTANT
    } else {
        symbol_kind(kind)
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
#[path = "../tests/support/failure_log.rs"]
mod failure_log;

#[cfg(test)]
mod tests {
    use super::*;

    fn span_test_workspace(path: &str) -> BoneWorkspace {
        let mut bone = BoneWorkspace::new();
        bone.update(path, "fn main() {}".into());
        bone
    }

    #[test]
    fn span_location_source_failure_reports_cause_once_and_recovers() {
        let capture = super::failure_log::Recording::start();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.rock").to_string_lossy().into_owned();
        let target = dir
            .path()
            .join("target.rock")
            .to_string_lossy()
            .into_owned();
        let mut bone = span_test_workspace(&main);
        let mut span = Span::unknown();
        span.file = Ustr::from(target.as_str());
        span.offset_range = (0, 6);
        capture.take();

        // Both a disappearing target and unreadable source must keep the cause.
        for unreadable in [false, true] {
            let proj = bone.projects.get(&BoneWorkspace::root_for(&main)).unwrap();
            fs_err::write(
                &target,
                if unreadable {
                    &b"\xff"[..]
                } else {
                    &b"target"[..]
                },
            )
            .unwrap();
            if !unreadable {
                fs_err::remove_file(&target).unwrap();
            }
            let cause = fs_err::read_to_string(&target).unwrap_err().to_string();
            assert!(bone.location_from_span(proj, &span).is_none());
            let records = capture.take();
            assert_eq!(records.len(), 1, "missing source failure: {records:?}");
            assert!(
                records[0].contains("LSP")
                    && records[0].contains("span location")
                    && records[0].contains(&target)
                    && records[0].contains("read source")
                    && records[0].contains(&cause)
                    && records[0].contains("omit location"),
                "source failure lost context: {records:?}"
            );
            assert!(
                bone.location_from_span_indexed(proj, &span, &mut HashMap::new())
                    .is_none()
            );
            assert!(
                capture.take().is_empty(),
                "unchanged failure must not repeat"
            );
            fs_err::write(&target, "target").unwrap();
            assert!(bone.location_from_span(proj, &span).is_some());
            assert!(capture.take().is_empty());
            // Text changes release the existing project-scoped report keys.
            bone.update(
                &main,
                if unreadable {
                    "fn main() {}"
                } else {
                    "fn main() { let x = 1 }"
                }
                .into(),
            );
            capture.take();
        }
    }

    #[test]
    fn span_location_invalid_uri_reports_cause_once() {
        let capture = super::failure_log::Recording::start();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main.rock").to_string_lossy().into_owned();
        let bone = span_test_workspace(&main);
        let proj = bone.projects.get(&BoneWorkspace::root_for(&main)).unwrap();
        let target = "relative-span.rock";
        let mut span = Span::unknown();
        span.file = Ustr::from(target);
        let cause = Uri::from_file_path(target).unwrap_err().to_string();
        capture.take();
        assert!(bone.location_from_span(proj, &span).is_none());
        let records = capture.take();
        assert_eq!(records.len(), 1, "missing URI failure: {records:?}");
        assert!(
            records[0].contains("span location")
                && records[0].contains(target)
                && records[0].contains("construct URI")
                && records[0].contains(&cause)
                && records[0].contains("omit location"),
            "URI failure lost context: {records:?}"
        );
        assert!(
            bone.location_from_span_indexed(proj, &span, &mut HashMap::new())
                .is_none()
        );
        assert!(capture.take().is_empty());
    }

    #[test]
    fn span_location_tracked_unsaved_source_wins_over_disk() {
        let capture = super::failure_log::Recording::start();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rock").to_string_lossy().into_owned();
        let source = "\nfn main() {}";
        fs_err::write(&path, "different disk source").unwrap();
        let mut bone = BoneWorkspace::new();
        bone.update(&path, source.into());
        let proj = bone.projects.get(&BoneWorkspace::root_for(&path)).unwrap();
        let mut span = Span::unknown();
        // Real spans use the session file id, not the editor path (which may
        // traverse a symlink, including the temporary directory on macOS).
        let file_id = bone.canon(&path);
        span.file = Ustr::from(file_id.as_str());
        span.offset_range = (4, 8);
        capture.take();
        for deleted in [false, true] {
            if deleted {
                fs_err::remove_file(&path).unwrap();
            }
            let (_, text) = bone.span_uri_and_text(proj, &span).unwrap();
            assert!(matches!(text, Cow::Borrowed(_)));
            assert_eq!(text, source);
            let location = bone.location_from_span(proj, &span).unwrap();
            assert_eq!(
                location.uri,
                Uri::from_file_path(&editor_path(&file_id)).unwrap()
            );
            assert_eq!(
                location.range,
                Range::new(Position::new(1, 3), Position::new(1, 7))
            );
            assert_eq!(
                bone.location_from_span_indexed(proj, &span, &mut HashMap::new()),
                Some(location)
            );
        }
        assert!(capture.take().is_empty());
    }

    #[test]
    fn span_location_recording_and_temp_target_are_released_on_unwind() {
        let mut target = PathBuf::new();
        let mut reached_assertion = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Keep the recording guard alive until the intentional unwind.
            let _capture = super::failure_log::Recording::start();
            let dir = tempfile::tempdir().unwrap();
            target = dir.path().join("target.rock");
            fs_err::write(&target, "temporary source").unwrap();
            let main = dir.path().join("main.rock").to_string_lossy().into_owned();
            let bone = span_test_workspace(&main);
            let proj = bone.projects.get(&BoneWorkspace::root_for(&main)).unwrap();
            let mut span = Span::unknown();
            span.file = Ustr::from("/invalid\nspan.rock");
            assert!(bone.location_from_span(proj, &span).is_none());
            reached_assertion = true;
            panic!("intentional assertion failure to verify cleanup");
        }));
        assert!(result.is_err());
        assert!(
            reached_assertion,
            "cleanup probe failed before its intentional assertion"
        );
        assert!(
            !target.parent().unwrap().exists(),
            "temporary directory survived unwind"
        );
        let capture = super::failure_log::Recording::start();
        assert!(capture.take().is_empty(), "recording survived unwind");
        log::warn!(target: "LSP", "fresh recording after unwind");
        assert_eq!(capture.take().len(), 1);
    }

    #[test]
    fn combined_parse_failure_reports_cause_and_repaired_request_succeeds() {
        let capture = super::failure_log::Recording::start();
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir
            .path()
            .join("sibling.rock")
            .to_string_lossy()
            .into_owned();
        let path = dir.path().join("main.rock").to_string_lossy().into_owned();
        let good = "fn sibling() { let count = 1 }";
        let broken = "fn sibling() { let text = \"unterminated";
        let mut bone = BoneWorkspace::new();
        bone.update(&sibling, good.into());
        bone.update(&path, "struct Clash {} struct Clash {}".into());
        capture.take();

        // Session normally rejects hard parses before storing text. Corrupt
        // one stored sibling to exercise the defensive combined-parse edge.
        let root = BoneWorkspace::root_for(&sibling);
        bone.projects
            .get_mut(&root)
            .unwrap()
            .session
            .files
            .get_mut(&sibling)
            .unwrap()
            .text = broken.into();
        let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cause = skull_parser::parse_module(broken, Ustr::from(sibling.as_str()))
                .expect_err("hard parse fixture")
                .message;
            let hints = bone.inlay_hints(
                &path,
                Range::new(Position::new(0, 0), Position::new(100, 0)),
            );
            assert!(hints.is_empty());
            let records = capture.take();
            assert!(
                records.iter().any(|record| record.contains("inlay")
                    && record.contains(&path)
                    && record.contains("combined-module")
                    && record.contains(&cause)
                    && record.contains("live workspace")),
                "combined parse cause was lost: {records:?}"
            );
            let proj = bone.projects.get(&root).unwrap();
            assert!(proj.reparse_user_file(&sibling, "hover").is_none());
            assert!(
                capture.take().iter().any(|record| record.contains("hover")
                    && record.contains(&sibling)
                    && record.contains(&cause)),
                "stored source parse cause was lost"
            );
        }));
        bone.projects
            .get_mut(&root)
            .unwrap()
            .session
            .files
            .get_mut(&sibling)
            .unwrap()
            .text = good.into();
        if let Err(panic) = check {
            std::panic::resume_unwind(panic);
        }
        bone.update(&path, "fn main() { let count = 1 }".into());
        assert!(
            !bone
                .inlay_hints(
                    &path,
                    Range::new(Position::new(0, 0), Position::new(100, 0))
                )
                .is_empty()
        );
        assert!(capture.take().is_empty());
    }

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
            (
                "scene_get_text_baseline",
                "outline_detect_shader",
                "scene_draw_r",
            ),
            (
                "scene_canvas_save_png",
                "tile_floor_atlas.full_image_rect().w",
                "scene_draw_r",
            ),
            ("set_x", "p.pos.x", "set_w"),
            (
                "scene_measure_text_vec2",
                "outline_detect_shader",
                "scene_draw_r",
            ),
            ("draw_circle", "world.rect", "draw_t"),
            ("get_tile_at", "world.grid().origin", "get_tx"),
        ];
        for (name, arg, q) in cases {
            // Sanity: these must be PASSING pairs -- the name absorbs strictly
            // fewer than |q| chars (else the noise guard rejects the fn and the
            // +1000 prefix bonus, which the decomposition omits, would apply).
            let k = prefix_subseq_len(q.as_bytes(), &name.to_lowercase());
            assert!(
                k < q.len(),
                "test case not a passing pair: name={name:?} q={q:?} k={k}"
            );
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

        let e = CompilerError::new_with_label(cause, "use `./`".to_string()).with_label(
            use_site,
            "this `float` is used where `int` is required".to_string(),
        );
        let diags = compiler_error_to_diagnostics(&e);

        assert_eq!(diags.len(), 2, "one diagnostic per label");
        assert_eq!(diags[0].1.range.start.line, 118);
        assert_eq!(diags[0].1.message, "use `./`");
        assert_eq!(diags[1].1.range.start.line, 123);
        assert_eq!(
            diags[1].1.message,
            "this `float` is used where `int` is required"
        );
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
        assert_eq!(
            balance_delimiters("fn f() { g(".to_string()),
            "fn f() { g()}"
        );
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
        assert_eq!(
            balance_delimiters("let s = \"a {{ b\"".to_string()),
            "let s = \"a {{ b\""
        );
        assert_eq!(
            balance_delimiters("let s = \"a {x} b\"".to_string()),
            "let s = \"a {x} b\""
        );
        // A bare `{` in a string OPENS an interpolation (tokenizer rule): an
        // unterminated `"foo {` must close as `}"` or tokenize still fatals.
        assert_eq!(
            balance_delimiters("let s = \"foo {".to_string()),
            "let s = \"foo {}\""
        );
        // An unterminated string is closed before any brace closers.
        assert_eq!(
            balance_delimiters("fn f() { let s = \"hi".to_string()),
            "fn f() { let s = \"hi\"}"
        );
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

    fn equivalence_ws(files: &[(&str, &str)]) -> Workspace {
        let mut modules: Vec<_> = files
            .iter()
            .map(|(file, src)| skull_parser::parse_module_clean(src, Ustr::from(file)).unwrap())
            .collect();
        bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap()
    }

    fn unique_return(files: &[(&str, &str)], name: &str) -> Option<String> {
        let ws = equivalence_ws(files);
        fn_unique_return_ty_in_scope(&ws, name, None).map(|t| ws.render(None, t))
    }

    fn first_param(ws: &Workspace, name: &str) -> TyId {
        ws.modules
            .iter()
            .flat_map(|m| m.functions.iter())
            .find(|(_, f)| f.name.as_str() == name)
            .map(|(_, f)| f.param_decls[0].ty)
            .unwrap()
    }

    #[test]
    fn vec_ufcs_methods_match_the_element_by_type_equivalence() {
        let ws = equivalence_ws(&[
            (
                "a",
                "struct T { x: int }\n\
                 fn on_t(xs: [T]) {}\n\
                 fn on_int(xs: [int]) {}",
            ),
            ("b", "fn nominal(xs: [T]) {}\nfn generic<T>(xs: [T]) {}"),
        ]);
        let elem_of = |name: &str| {
            let p0 = first_param(&ws, name);
            let bone_semantic::hir::TypeExpr::App { mut params, .. } = ty_of(&ws, None, p0) else {
                panic!("`{name}` takes a Vec");
            };
            TyRef::of_static(p0.with_idx(params.next().unwrap()))
        };
        let methods = |elem| {
            let mut names: Vec<String> = crate::query::ufcs_methods(&ws, Ustr::from("Vec"), elem)
                .into_iter()
                .map(|m| m.name)
                .filter(|n| n.starts_with("on_"))
                .collect();
            names.sort();
            names
        };
        // The struct `T` element spelled in another module's arena.
        assert_eq!(methods(Some(elem_of("nominal"))), ["on_t"]);
        // A generic `T` element renders like the struct but is not it.
        assert!(methods(Some(elem_of("generic"))).is_empty());
    }

    #[test]
    fn field_ref_fields_match_by_type_equivalence() {
        let ws = equivalence_ws(&[
            (
                "a",
                "struct V { x: int }\n\
                 struct V2 { x: float }\n\
                 struct Owner { a: V, b: [V2?], c: int }",
            ),
            ("b", "fn shared(x: [V2?]) {}\nfn generic<V>(x: V) {}"),
        ]);
        let owner = Ustr::from("Owner");
        // The same `[V2?]` spelled in another module's arena.
        assert_eq!(
            fields_of_type(&ws, owner, first_param(&ws, "shared")),
            vec![Ustr::from("b")]
        );
        // A generic `V` renders like the struct `V` but is not it.
        assert!(fields_of_type(&ws, owner, first_param(&ws, "generic")).is_empty());
    }

    #[test]
    fn unique_return_requires_agreeing_overloads() {
        let src = "fn mk(x: int) -> int { x }\nfn mk(x: string) -> float { 1.0 }";
        assert_eq!(unique_return(&[("a", src)], "mk"), None);
        let src = "fn mk(x: int) -> [int] { [x] }\nfn mk(x: string) -> [int] { [] }";
        assert_eq!(unique_return(&[("a", src)], "mk").as_deref(), Some("[int]"));
    }

    #[test]
    fn unique_return_agrees_across_module_arenas() {
        let a = "struct V2 { x: float }\nfn mk(x: int) -> [V2?] { [] }";
        let b = "fn mk(x: string) -> [V2?] { [] }";
        assert_eq!(
            unique_return(&[("a", a), ("b", b)], "mk").as_deref(),
            Some("[V2?]")
        );
    }

    #[test]
    fn unique_return_keeps_a_generic_apart_from_a_same_named_nominal() {
        // Both overloads render their return as `T`: one is the struct, the
        // other the overload's own generic parameter.
        let src = "struct T { x: int }\n\
                   fn mk(x: int) -> T { T { x } }\n\
                   fn mk<T>(x: T) -> T { x }";
        assert_eq!(unique_return(&[("a", src)], "mk"), None);
        // Two overloads' generic parameters are distinct as well.
        let src = "fn mk<T>(x: T, y: int) -> T { x }\nfn mk<T>(x: T) -> T { x }";
        assert_eq!(unique_return(&[("a", src)], "mk"), None);
    }
}

#[cfg(test)]
mod action_policy_tests {
    use super::BoneWorkspace;
    use bone_lints::{ACTION_CALLERS, ActionLayerPolicy, LintOptions};

    static POLICY: ActionLayerPolicy = ActionLayerPolicy {
        production_root: "game",
        production_label: "Example",
        action_prefix: "DO_",
        raw_input_names: &[],
        raw_input_classes: &[],
        registration_functions: &[],
    };

    #[test]
    fn host_action_policy_reaches_loaded_and_incremental_lint_caches() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("game");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(
            project.join("stick.toml"),
            "[package]\nname = \"policy-fixture\"\n",
        )
        .unwrap();
        let file = project.join("main.rock");
        let source = "global DO_MOVE: int = 1\nfn main() {}\n";
        std::fs::write(&file, source).unwrap();
        let file = file.to_str().unwrap();
        let mut workspace = BoneWorkspace::new().with_lint_options(
            LintOptions::only([ACTION_CALLERS.to_owned()]).with_action_policy(Some(&POLICY)),
        );
        workspace.load_project(file);
        let diagnostics = workspace.lint_diagnostics(file);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "production Example action `DO_MOVE` has no production caller"
        );
        workspace.update(
            file,
            source.replace("fn main() {}", "fn main() { let value = DO_MOVE }"),
        );
        assert!(workspace.lint_diagnostics(file).is_empty());
    }
}
