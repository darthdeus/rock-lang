//! `rock modules migrate <package> --check|--apply [--consumer <dir>]...
//! [--format pretty|json]` (modules spec 15.2, plan M5).
//!
//! Converts one legacy package to modules-v1 without moving its files out of
//! the package root. The plan has the parts spec 15.2 names, planned
//! separately: the manifest mode, module headers (none: the default keeps
//! every file at the root), one `imports.rock` facade of package-visible glob
//! re-exports for the dependencies the package's bodies actually reference,
//! `pub` on the definitions the named consumers reference plus the API type
//! closure the compiler demands on top of them, the contextual-ascription
//! rewrite (`x :: T` to `(x : T)`), and qualified-call repairs where a v1 tier
//! would hide the definition a legacy call selected.
//!
//! Every fact is compiler-derived: what a consumer uses and what a fragment
//! needs imported come from the resolved references of checked workspaces
//! (`modules_analysis::references`), the closure from
//! `api_closure_requirements`, and the ascriptions from parse-tree nodes. The
//! candidate is checked in memory in the package's own compilation unit and
//! in each consumer's before anything is written, and every resolved
//! reference site is compared with the legacy baseline: a site that moves is
//! repaired by qualification when that is unambiguous and is otherwise a
//! blocker.
//!
//! Exit status follows `rock modules audit`: 0 nothing to do (the package is
//! already v1) or `--apply` succeeded; 3 `--check` found work to do, or the
//! plan is blocked (nothing is written); 4 an analysis failed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atomic_refcell::AtomicRefCell;
use bone_core::modules::is_stdlib_module_file;
use bone_core::prelude::ModuleId;
use bone_semantic::inference::Workspace;
use rock_core::errors::CompilerError;
use rock_core::source_code::{Dialect, FileContents};
use rock_core::SourceFiles;
use rock_host::stick::graph::{PackageGraph, PackageId};
use rock_host::stick::StickConfig;
use serde_json::{json, Value};
use ustr::Ustr;

use crate::modules_analysis::{self, declared_name, PopMode, Ref, RefKind, Target};
use crate::modules_audit::Format;

const EXIT_NOT_CLEAN: i32 = 3;
const EXIT_ANALYSIS_FAILED: i32 = 4;
/// The facade file the default conversion adds (spec 15.2).
const IMPORTS_FILE: &str = "imports.rock";
/// Qualified-call repairs are found by checking a candidate; each round can
/// only expose calls the previous round's repairs unmasked, so a few suffice.
const MAX_ROUNDS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Check,
    Apply,
}

// --- the package's own source -----------------------------------------------

/// One file of the package being migrated, as read for the baseline.
struct PkgFile {
    id: Ustr,
    path: PathBuf,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DefKind {
    Fn,
    Operator,
    Macro,
    Struct,
    Enum,
    Newtype,
    Global,
    Decorator,
}

impl DefKind {
    fn as_str(self) -> &'static str {
        match self {
            DefKind::Fn => "fn",
            DefKind::Operator => "operator",
            DefKind::Macro => "macro",
            DefKind::Struct => "struct",
            DefKind::Enum => "enum",
            DefKind::Newtype => "newtype",
            DefKind::Global => "global",
            DefKind::Decorator => "decorator",
        }
    }

    fn is_type(self) -> bool {
        matches!(self, DefKind::Struct | DefKind::Enum | DefKind::Newtype)
    }
}

/// An `inline(<backend>)` function body and what the census needs to compile
/// it as the emitter does.
#[derive(Clone)]
struct InlineSource {
    backend: skull_parser::FnBackend,
    params: Vec<Ustr>,
    returns_value: bool,
    src: Ustr,
}

impl InlineSource {
    fn of(fd: &skull_parser::FunctionDef) -> Option<Self> {
        let skull_parser::FunctionBody::Inline(backend, src) = &fd.body else { return None };
        Some(InlineSource {
            backend: *backend,
            params: fd.params.iter().map(|p| p.name.name).collect(),
            returns_value: fd.return_ty.is_some(),
            src: *src,
        })
    }
}

/// One top-level definition of the package, from its legacy parse tree.
struct Def {
    file: usize,
    name: Ustr,
    kind: DefKind,
    /// The item's byte range, annotations included; `pub ` goes at its start
    /// because a visibility leads the item (`parse_item`).
    span: (usize, usize),
    line: usize,
    /// An `inline(<backend>)` body, for the raw-ABI census.
    inline: Option<InlineSource>,
    /// Emitted under its declared name whatever its owner (`@nomangle`,
    /// `extern`, `main`, `test_*`), so a v1 owner does not move its symbol.
    exempt_symbol: bool,
}

fn parse_defs(files: &[PkgFile]) -> Result<(Vec<Def>, Vec<Vec<modules_analysis::Ascription>>, Vec<Vec<(usize, usize)>>), String> {
    let mut defs = Vec::new();
    let mut ascriptions = Vec::new();
    let mut parens = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let ast = skull_parser::parse_module_with_dialect(&f.text, f.id, Dialect::Legacy)
            .map_err(|e| format!("{}: {}", f.path.display(), e.message))?;
        if let Some(e) = ast.recovery_errors.first() {
            return Err(format!("{}: {}", f.path.display(), e.message));
        }
        for item in &ast.items {
            let annotated = |n: &str| item.annotations.iter().any(|a| a.name.name.as_str() == n);
            let (name, kind, inline, extern_body) = match &item.kind {
                skull_parser::ItemKind::Function(fd) => {
                    let kind = if fd.is_macro {
                        DefKind::Macro
                    } else if fd.name.name.starts_with("__op_") {
                        DefKind::Operator
                    } else {
                        DefKind::Fn
                    };
                    let extern_body = !matches!(fd.body, skull_parser::FunctionBody::Rock(_) | skull_parser::FunctionBody::Inline(..));
                    (fd.name.name, kind, InlineSource::of(fd), extern_body)
                }
                skull_parser::ItemKind::Struct(s) => (s.name.name, DefKind::Struct, None, false),
                skull_parser::ItemKind::Enum(e) => (e.name.name, DefKind::Enum, None, false),
                skull_parser::ItemKind::Newtype(n) => (n.name.name, DefKind::Newtype, None, false),
                skull_parser::ItemKind::Global(g) => (g.name.name, DefKind::Global, None, false),
                skull_parser::ItemKind::Decorator(d) => (d.name.name, DefKind::Decorator, None, false),
                _ => continue,
            };
            let exempt_symbol = annotated("nomangle")
                || annotated("test")
                || extern_body
                || name.as_str() == "main"
                || name.as_str().starts_with("test_");
            defs.push(Def {
                file: fi,
                name,
                kind,
                span: item.span.offset_range,
                line: item.span.line_range.0 + 1,
                inline,
                exempt_symbol,
            });
        }
        ascriptions.push(modules_analysis::ascriptions(&ast));
        // A legacy ascription already written inside parentheses keeps them:
        // `(x :: T)` becomes `(x : T)`, not `((x : T))`.
        parens.push(
            ast.exprs
                .iter()
                .filter_map(|(_, e)| match &e.kind {
                    skull_parser::ExprKind::Paren(inner)
                        if matches!(ast.exprs[*inner].kind, skull_parser::ExprKind::TypeAscription(..)) =>
                    {
                        Some(ast.exprs[*inner].span.offset_range)
                    }
                    _ => None,
                })
                .collect(),
        );
    }
    Ok((defs, ascriptions, parens))
}

// --- analysis contexts -------------------------------------------------------

/// One compilation unit the package is analyzed in: the package itself or a
/// named consumer, each with its tests (the `rock test` population). A
/// consumer whose test population does not check is analyzed with its
/// `rock compile` population instead, and its test uses are unknown.
struct Ctx {
    root: PathBuf,
    mode: PopMode,
    graph: PackageGraph,
    sources: SourceFiles,
    /// The migrated package's id in this graph.
    p_id: u32,
    file_pkg: HashMap<Ustr, u32>,
}

impl Ctx {
    fn name(&self) -> &str {
        &self.graph.package(self.graph.root()).manifest_name
    }
}

fn load_ctx(root: &Path, p_root: &Path, mode: PopMode) -> std::result::Result<Ctx, String> {
    let root = root.canonicalize().map_err(|e| format!("`{}`: {e}", root.display()))?;
    let Some((config_path, config)) = StickConfig::find_from_dir(&root).map_err(|e| format!("{e:#}"))? else {
        return Err(format!("no stick.toml at or above `{}`", root.display()));
    };
    let manifest_dir = config_path.parent().unwrap_or(&config_path).to_path_buf();
    if manifest_dir.canonicalize().ok().as_deref() != Some(root.as_path()) {
        return Err(format!(
            "`{}` is not a package root (its nearest stick.toml is `{}`)",
            root.display(),
            config_path.display()
        ));
    }
    let channel = rock_host::stick::resolved_channel_for(&config);
    let graph = PackageGraph::build_with_channel(&manifest_dir, &config, &channel).map_err(|e| format!("{e:#}"))?;
    let Some(p) = graph.packages().iter().find(|n| n.root == p_root) else {
        return Err(format!(
            "`{}` does not depend on the package being migrated (`{}`)",
            graph.package(graph.root()).manifest_name,
            p_root.display()
        ));
    };
    let p_id = p.id.0;
    let pop = modules_analysis::population(&root, &graph, Some(mode))?;
    let (file_pkg, untagged) = modules_analysis::file_packages(&pop.analyzed);
    if !untagged.is_empty() {
        return Err(format!("{} analyzed file(s) carry no package tag: {}", untagged.len(), untagged.join(", ")));
    }
    Ok(Ctx { root, graph, sources: pop.analyzed, p_id, file_pkg, mode })
}

/// A checked context: its workspace and its reference list.
struct Checked {
    ws: Workspace,
    refs: Vec<Ref>,
    errors: Vec<CompilerError>,
}

fn check_sources(sources: &SourceFiles) -> std::result::Result<Checked, (String, Vec<CompilerError>)> {
    let modules_analysis::Checked { ws, errors, .. } =
        modules_analysis::check(sources).map_err(|f| (f.stage.to_string(), f.errors))?;
    let refs = modules_analysis::references(&ws);
    Ok(Checked { ws, refs, errors })
}

// --- edits -------------------------------------------------------------------

/// One span edit: replace `range` (empty for an insertion) with `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    range: (usize, usize),
    text: String,
}

/// Apply non-overlapping edits. Overlap is refused (spec 15.2).
fn apply_edits(text: &str, edits: &[Edit]) -> std::result::Result<String, String> {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.range);
    let mut out = String::with_capacity(text.len() + 64);
    let mut at = 0;
    for e in sorted {
        if e.range.0 < at {
            return Err(format!("overlapping edits at byte {}", e.range.0));
        }
        if e.range.1 > text.len() || !text.is_char_boundary(e.range.0) || !text.is_char_boundary(e.range.1) {
            return Err(format!("edit range {:?} is outside the file", e.range));
        }
        out.push_str(&text[at..e.range.0]);
        out.push_str(&e.text);
        at = e.range.1;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// Where a baseline byte offset lands after `edits`. An insertion AT an offset
/// stays before it (a qualified call starts where its name started); a
/// replaced range collapses onto its start.
fn map_offset(edits: &[Edit], off: usize) -> usize {
    let mut shift: isize = 0;
    for e in edits {
        let (a, b) = e.range;
        if a == b {
            if off > a {
                shift += e.text.len() as isize;
            }
        } else if off >= b {
            shift += e.text.len() as isize - (b - a) as isize;
        } else if off > a {
            shift -= (off - a) as isize;
        }
    }
    (off as isize + shift) as usize
}

// --- identities used to compare a baseline with a candidate -------------------

/// A resolved target, spelled so a legacy key and its v1 DefKey agree.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TargetId {
    Fn { file: Ustr, name: String, ordinal: usize },
    Def { file: Ustr, name: String },
    Family { files: Vec<Ustr>, name: String },
    Unrecorded(String),
}

fn target_id(ws: &Workspace, t: &Target) -> TargetId {
    match t {
        Target::Fn(mid, fid) => {
            let m = ws.module(*mid);
            let name = declared_name(m.functions[*fid].name.as_str()).to_string();
            let ordinal = m
                .functions
                .iter()
                .filter(|(i, f)| u32::from(i.into_raw()) < u32::from(fid.into_raw()) && declared_name(f.name.as_str()) == name)
                .count();
            TargetId::Fn { file: m.file, name, ordinal }
        }
        Target::Key { key, modules } => {
            let name = declared_name(key.as_str()).to_string();
            let mut files: Vec<Ustr> = modules.iter().map(|m| ws.module(*m).file).collect();
            files.sort();
            files.dedup();
            match files.as_slice() {
                [one] => TargetId::Def { file: *one, name },
                _ => TargetId::Family { files, name },
            }
        }
        Target::Unrecorded(name) => TargetId::Unrecorded(name.to_string()),
    }
}

/// A reference site, in the coordinates of the text it was checked in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SiteKey {
    file: Ustr,
    start: usize,
    end: usize,
    kind: RefKind,
}

type Sites = BTreeMap<SiteKey, Vec<TargetId>>;

/// Every package-file reference site; `edits` maps a baseline site into
/// candidate coordinates (empty for a candidate). Types and patterns are
/// compared per file as a multiset rather than by site: a v1 signature type
/// records no span (`span_recordable` tests the spelled name against maps
/// keyed by DefKey), and a type name cannot be overloaded, so a per-file
/// count is as strict.
fn sites(checked: &Checked, file_pkg: &HashMap<Ustr, u32>, edits: &HashMap<Ustr, Vec<Edit>>) -> Sites {
    let mut out: Sites = BTreeMap::new();
    for r in &checked.refs {
        let file = checked.ws.module(r.from).file;
        if !file_pkg.contains_key(&file) {
            continue;
        }
        if matches!(r.kind, RefKind::Type | RefKind::Pattern) {
            out.entry(SiteKey { file, start: 0, end: 0, kind: r.kind })
                .or_default()
                .push(target_id(&checked.ws, &r.target));
            continue;
        }
        let Some(span) = r.span else { continue };
        if span.file != file {
            continue;
        }
        let (start, end) = match edits.get(&file) {
            Some(e) => (map_offset(e, span.offset_range.0), map_offset(e, span.offset_range.1)),
            None => span.offset_range,
        };
        out.entry(SiteKey { file, start, end, kind: r.kind })
            .or_default()
            .push(target_id(&checked.ws, &r.target));
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

// --- the plan ------------------------------------------------------------------

#[derive(Default)]
struct ConsumerUse {
    by_kind: BTreeMap<RefKind, usize>,
    /// First site, `file:line`.
    example: Option<String>,
}

#[derive(Default)]
struct ExportReasons {
    /// consumer package name -> what it uses.
    consumers: BTreeMap<String, ConsumerUse>,
    /// (exporter name, reason) from the API closure.
    closure: BTreeSet<(String, String)>,
}

struct ImportNeed {
    alias: String,
    package: String,
    /// P file index -> references to that package.
    fragments: BTreeMap<usize, usize>,
}

struct Repair {
    file: usize,
    offset: usize,
    line: usize,
    name: String,
    qualifier: String,
    reason: String,
}

struct Verification {
    ctx: String,
    compared: usize,
    changed: Vec<String>,
    errors: Vec<String>,
}

struct Plan {
    package: String,
    package_root: PathBuf,
    /// Each consumer's name, root and the population it was analyzed with.
    consumers: Vec<(String, PathBuf, PopMode)>,
    /// Definitions only a consumer's tests reference: consumer -> uses.
    test_uses: BTreeMap<usize, BTreeMap<String, ConsumerUse>>,
    files: Vec<PkgFile>,
    defs: Vec<Def>,
    manifest: Option<(String, String)>,
    imports: Vec<ImportNeed>,
    exports: BTreeMap<usize, ExportReasons>,
    ascriptions: Vec<(usize, modules_analysis::Ascription, bool)>,
    repairs: Vec<Repair>,
    blockers: Vec<String>,
    notes: Vec<String>,
    verification: Vec<Verification>,
    /// Final per-file edits and the new facade text.
    edits: HashMap<usize, Vec<Edit>>,
    imports_text: Option<String>,
}

enum Outcome {
    AlreadyV1(String),
    Failed { stage: String, message: String },
    Planned(Box<Plan>),
}

/// Run the planner (and, with `--apply`, the writer). Returns the exit code.
pub fn run(package: &Path, consumers: &[PathBuf], export_test_uses: bool, mode: Mode, format: Format) -> Result<i32> {
    let outcome = plan(package, consumers, export_test_uses);
    let code = match &outcome {
        Outcome::AlreadyV1(_) => 0,
        Outcome::Failed { .. } => EXIT_ANALYSIS_FAILED,
        Outcome::Planned(p) if !p.blockers.is_empty() => EXIT_NOT_CLEAN,
        Outcome::Planned(_) if mode == Mode::Check => EXIT_NOT_CLEAN,
        Outcome::Planned(_) => 0,
    };
    let mut applied: Option<std::result::Result<Vec<Verification>, String>> = None;
    if let (Outcome::Planned(p), Mode::Apply) = (&outcome, mode) {
        if p.blockers.is_empty() {
            applied = Some(apply(p));
        }
    }
    let code = match &applied {
        Some(Err(_)) => EXIT_ANALYSIS_FAILED,
        Some(Ok(v)) if v.iter().any(|v| !v.changed.is_empty() || !v.errors.is_empty()) => EXIT_NOT_CLEAN,
        _ => code,
    };
    match format {
        Format::Pretty => print!("{}", render_pretty(&outcome, mode, applied.as_ref())),
        Format::Json => println!("{}", serde_json::to_string_pretty(&render_json(&outcome, mode, applied.as_ref()))?),
    }
    std::io::stdout().flush()?;
    Ok(code)
}

fn plan(package: &Path, consumer_dirs: &[PathBuf], export_test_uses: bool) -> Outcome {
    let failed = |stage: &str, message: String| Outcome::Failed { stage: stage.to_string(), message };
    let p_root = match package.canonicalize() {
        Ok(p) => p,
        Err(e) => return failed("package", format!("`{}`: {e}", package.display())),
    };
    let pctx = match load_ctx(&p_root, &p_root, PopMode::Test) {
        Ok(c) => c,
        Err(e) => return failed("package graph", e),
    };
    let p_node = pctx.graph.package(PackageId(pctx.p_id)).clone();
    if p_node.dialect == Dialect::V1 {
        return Outcome::AlreadyV1(p_node.manifest_name);
    }
    let pbase = match check_sources(&pctx.sources) {
        Ok(b) if b.errors.is_empty() => b,
        Ok(b) => return failed("baseline", format!("{} does not check before migration:\n{}", pctx.name(), render(&pctx.sources, &b.errors))),
        Err((stage, errs)) => return failed("baseline", format!("{} ({stage}) does not check before migration:\n{}", pctx.name(), render(&pctx.sources, &errs))),
    };
    let mut ctxs = vec![pctx];
    let mut baselines = vec![pbase];
    let mut tests_unknown = Vec::new();
    for dir in consumer_dirs {
        let with_tests = match load_ctx(dir, &p_root, PopMode::Test) {
            Ok(c) => c,
            Err(e) => return failed("consumer", format!("consumer `{}`: {e}", dir.display())),
        };
        match check_sources(&with_tests.sources) {
            Ok(b) if b.errors.is_empty() => {
                ctxs.push(with_tests);
                baselines.push(b);
                continue;
            }
            Ok(b) => tests_unknown.push(format!("{} ({} error(s))", with_tests.name(), b.errors.len())),
            Err((stage, errs)) => tests_unknown.push(format!("{} ({stage}, {} error(s))", with_tests.name(), errs.len())),
        }
        let production = match load_ctx(dir, &p_root, PopMode::Production) {
            Ok(c) => c,
            Err(e) => return failed("consumer", format!("consumer `{}`: {e}", dir.display())),
        };
        match check_sources(&production.sources) {
            Ok(b) if b.errors.is_empty() => {
                ctxs.push(production);
                baselines.push(b);
            }
            Ok(b) => return failed("baseline", format!("{} does not check before migration:\n{}", production.name(), render(&production.sources, &b.errors))),
            Err((stage, errs)) => return failed("baseline", format!("{} ({stage}) does not check before migration:\n{}", production.name(), render(&production.sources, &errs))),
        }
    }

    // The package's files, as the package's own unit selects them.
    let mut files: Vec<PkgFile> = Vec::new();
    for f in &ctxs[0].sources.files {
        let f = f.borrow();
        if f.package.as_ref().map(|t| t.id) != Some(ctxs[0].p_id) {
            continue;
        }
        let FileContents::Filesystem { path, .. } = &f.file else {
            return failed("package", format!("package file `{}` is not on disk", f.file.to_id_path()));
        };
        files.push(PkgFile { id: Ustr::from(f.file.to_id_path().as_str()), path: PathBuf::from(path), text: f.contents.clone() });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let file_index: HashMap<Ustr, usize> = files.iter().enumerate().map(|(i, f)| (f.id, i)).collect();
    // A consumer loads the same files; its module ids must agree.
    for c in &ctxs[1..] {
        let theirs: BTreeSet<Ustr> =
            c.file_pkg.iter().filter(|(_, id)| **id == c.p_id).map(|(f, _)| *f).collect();
        for f in &theirs {
            if !file_index.contains_key(f) {
                return failed("consumer", format!("consumer `{}` loads `{f}` as part of the package, which its own unit does not", c.name()));
            }
        }
    }

    let (defs, ascriptions, parens) = match parse_defs(&files) {
        Ok(v) => v,
        Err(e) => return failed("parse", e),
    };


    let mut plan = Plan {
        package: p_node.manifest_name.clone(),
        package_root: p_root.clone(),
        consumers: ctxs[1..].iter().map(|c| (c.name().to_string(), c.root.clone(), c.mode)).collect(),
        test_uses: BTreeMap::new(),
        files,
        defs,
        manifest: None,
        imports: Vec::new(),
        exports: BTreeMap::new(),
        ascriptions: Vec::new(),
        repairs: Vec::new(),
        blockers: Vec::new(),
        notes: Vec::new(),
        verification: Vec::new(),
        edits: HashMap::new(),
        imports_text: None,
    };

    // Manifest.
    let manifest_path = p_root.join("stick.toml");
    match std::fs::read_to_string(&manifest_path) {
        Ok(text) => match manifest_with_v1(&text) {
            Ok(new) => plan.manifest = Some((text, new)),
            Err(e) => plan.blockers.push(format!("stick.toml: {e}")),
        },
        Err(e) => return failed("package", format!("{}: {e}", manifest_path.display())),
    }

    // Definitions by file, for mapping a HIR definition back to its item.
    let mut defs_by_file: HashMap<Ustr, Vec<(usize, usize, usize)>> = HashMap::new();
    for (i, d) in plan.defs.iter().enumerate() {
        defs_by_file.entry(plan.files[d.file].id).or_default().push((d.span.0, d.span.1, i));
    }
    let def_at = |file: Ustr, offset: usize| -> Option<usize> {
        defs_by_file.get(&file)?.iter().find(|(a, b, _)| *a <= offset && offset < *b).map(|(_, _, i)| *i)
    };

    // Imports: what the package's bodies reference in other packages.
    let pctx = &ctxs[0];
    let base = &baselines[0];
    let mut needs: BTreeMap<u32, BTreeMap<usize, usize>> = BTreeMap::new();
    let mut owner_blockers: BTreeSet<String> = BTreeSet::new();
    let mut legacy_referenced: BTreeSet<u32> = BTreeSet::new();
    for r in &base.refs {
        let from_file = base.ws.module(r.from).file;
        let Some(&fi) = file_index.get(&from_file) else { continue };
        for owner in owners(&base.ws, &r.target, &pctx.file_pkg) {
            if owner != pctx.p_id {
                *needs.entry(owner).or_default().entry(fi).or_default() += 1;
            }
        }
    }
    for (owner, fragments) in needs {
        let node = pctx.graph.package(PackageId(owner));
        let edge = p_node.dependencies.iter().find(|e| e.target.0 == owner);
        match edge {
            None => {
                owner_blockers.insert(format!(
                    "the package references `{}`, which it does not declare as a dependency ({} reference(s)); v1 needs a declared edge (spec 4.2)",
                    node.manifest_name,
                    fragments.values().sum::<usize>()
                ));
            }
            Some(e) if node.dialect == Dialect::Legacy && !e.allow_legacy => {
                legacy_referenced.insert(owner);
                owner_blockers.insert(format!(
                    "the package references legacy dependency `{}` ({} reference(s)); a v1 package reaches a legacy one only through an `allow_legacy = true` edge (spec 3.2), which the migrator does not add on the author's behalf",
                    node.manifest_name,
                    fragments.values().sum::<usize>(),
                ));
            }
            Some(e) => plan.imports.push(ImportNeed { alias: e.alias.clone(), package: node.manifest_name.clone(), fragments }),
        }
    }
    for e in &p_node.dependencies {
        let target = pctx.graph.package(e.target);
        if target.dialect == Dialect::Legacy && !e.allow_legacy && !legacy_referenced.contains(&e.target.0) {
            owner_blockers.insert(format!(
                "dependency `{}` is legacy: a v1 package needs `allow_legacy = true` on that edge (spec 3.2); the migrator does not opt in on the author's behalf",
                e.declared_key
            ));
        }
    }
    plan.blockers.extend(owner_blockers);

    // Exports: what each consumer's own files reference in the package.
    for (c, b) in ctxs[1..].iter().zip(&baselines[1..]) {
        for r in &b.refs {
            let from_file = b.ws.module(r.from).file;
            if c.file_pkg.get(&from_file) == Some(&c.p_id) || is_stdlib_module_file(from_file.as_str()) {
                if c.file_pkg.get(&from_file) == Some(&c.p_id) {
                    // The package's own bodies inside a consumer unit: a legacy
                    // body may resolve against the consumer (spec 3.2).
                    for owner in owners(&b.ws, &r.target, &c.file_pkg) {
                        if owner != c.p_id && !c.graph.package(PackageId(c.p_id)).dependencies.iter().any(|e| e.target.0 == owner) {
                            plan.blockers.push(format!(
                                "inside `{}`, the package's `{}` references `{}`, which the package does not declare (a back-reference or an undeclared peer)",
                                c.name(),
                                from_file,
                                c.graph.package(PackageId(owner)).manifest_name
                            ));
                        }
                    }
                }
                continue;
            }
            for (file, offset) in p_def_spans(&b.ws, &r.target, &c.file_pkg, c.p_id) {
                let Some(di) = def_at(file, offset) else {
                    plan.blockers.push(format!("`{}` references a definition of the package at `{file}` byte {offset} that is not a top-level item", c.name()));
                    continue;
                };
                let u = if SourceFiles::is_test_only_path(from_file.as_str()) {
                    plan.test_uses.entry(di).or_default().entry(c.name().to_string()).or_default()
                } else {
                    plan.exports.entry(di).or_default().consumers.entry(c.name().to_string()).or_default()
                };
                *u.by_kind.entry(r.kind).or_default() += 1;
                if u.example.is_none() {
                    if let Some(span) = r.span {
                        u.example = Some(format!("{}:{}", short_path(&span.file), span.line_range.0 + 1));
                    }
                }
            }
        }
    }
    // A definition only a consumer's tests reach: spec 16 says relocate the
    // test or give the owner test support rather than widen the library, so
    // exporting it is the author's explicit choice.
    plan.test_uses.retain(|di, _| !plan.exports.contains_key(di));
    for (&di, uses) in &plan.test_uses {
        let d = &plan.defs[di];
        let who = uses
            .iter()
            .map(|(c, u)| format!("{c} ({}{})", kinds_text(&u.by_kind), u.example.as_ref().map(|e| format!("; first at {e}")).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("; ");
        if export_test_uses {
            let reasons = plan.exports.entry(di).or_default();
            for (c, u) in uses {
                let r = reasons.consumers.entry(format!("{c} tests")).or_default();
                r.by_kind = u.by_kind.clone();
                r.example = u.example.clone();
            }
        } else {
            plan.blockers.push(format!(
                "`{}` ({}:{}) is used only by consumer tests: {who}. Spec 16: relocate the test or give the owner test support rather than widen the library; pass --export-test-uses to export it anyway",
                d.name,
                short_path(&plan.files[d.file].id),
                d.line
            ));
        }
    }
    for t in &tests_unknown {
        plan.notes.push(format!(
            "consumer {t}: its test population does not check, so it was analyzed with its `rock compile` population and what its tests use is unknown"
        ));
    }
    plan.blockers.sort();
    plan.blockers.dedup();

    // Ascriptions.
    for (fi, list) in ascriptions.into_iter().enumerate() {
        for a in list {
            let parenthesized = parens[fi].contains(&a.node);
            plan.ascriptions.push((fi, a, parenthesized));
        }
    }

    // Notes the analysis cannot settle.
    for d in &plan.defs {
        match d.kind {
            DefKind::Macro => plan.notes.push(format!(
                "macro `{}` ({}:{}): macros expand before the namespace tree exists, so a consumer's use is not in the analysis and a macro cannot be `pub`; review its consumers",
                d.name,
                short_path(&plan.files[d.file].id),
                d.line
            )),
            DefKind::Decorator => plan.notes.push(format!(
                "decorator `{}` ({}:{}): `@{}` uses are not derived by this analysis; review whether a consumer needs it exported",
                d.name,
                short_path(&plan.files[d.file].id),
                d.line,
                d.name
            )),
            _ => {}
        }
    }
    if let Err(message) = raw_abi_scan(&mut plan, &ctxs, &file_index) {
        return Outcome::Failed { stage: "raw-ABI census".to_string(), message };
    }

    if !plan.blockers.is_empty() {
        return Outcome::Planned(Box::new(plan));
    }

    verify_candidate(&mut plan, &ctxs, &baselines, &file_index);
    Outcome::Planned(Box::new(plan))
}

/// Candidate rounds: grow the exports by the closure and add call repairs
/// until the candidate checks clean in every unit with every site resolving
/// as the baseline did, or record why it cannot.
fn verify_candidate(plan: &mut Plan, ctxs: &[Ctx], baselines: &[Checked], file_index: &HashMap<Ustr, usize>) {
    // A site inside a rewritten ascription moves by an amount that depends on
    // whether it is the node or its operand, so it is compared by the
    // re-parse (`verify_ascriptions`) and the clean check instead.
    let inside_ascription = |k: &SiteKey| {
        plan.ascriptions.iter().any(|(fi, a, _)| plan.files[*fi].id == k.file && a.node.0 <= k.start && k.end <= a.node.1)
    };
    let base_sites: Vec<Sites> = ctxs
        .iter()
        .zip(baselines)
        .map(|(c, b)| {
            let mut s = sites(b, &c.file_pkg, &HashMap::new());
            s.retain(|k, _| !inside_ascription(k));
            s
        })
        .collect();
    let mut converged = false;
    for _round in 0..MAX_ROUNDS {
        let edits = match candidate_edits(&plan) {
            Ok(e) => e,
            Err(e) => {
                plan.blockers.push(e);
                break;
            }
        };
        let imports_text = imports_text(&plan);
        let mut texts: HashMap<usize, String> = HashMap::new();
        for (fi, f) in plan.files.iter().enumerate() {
            match apply_edits(&f.text, edits.get(&fi).map(Vec::as_slice).unwrap_or(&[])) {
                Ok(t) => {
                    texts.insert(fi, t);
                }
                Err(e) => {
                    plan.blockers.push(format!("{}: {e}", short_path(&f.id)));
                }
            }
        }
        if !plan.blockers.is_empty() {
            break;
        }
        if let Err(e) = verify_ascriptions(&plan, &texts) {
            plan.blockers.push(e);
            break;
        }
        let edits_by_id: HashMap<Ustr, Vec<Edit>> =
            edits.iter().map(|(fi, e)| (plan.files[*fi].id, e.clone())).collect();

        let mut progressed = false;
        let mut verification = Vec::new();
        let mut unrepairable = Vec::new();
        for (ci, c) in ctxs.iter().enumerate() {
            let sources = candidate_sources(c, &plan, &file_index, &texts, imports_text.as_deref());
            let checked = match check_sources(&sources) {
                Ok(v) => v,
                Err((stage, errs)) => {
                    verification.push(Verification {
                        ctx: c.name().to_string(),
                        compared: 0,
                        changed: Vec::new(),
                        errors: vec![format!("{stage}:\n{}", render(&sources, &errs))],
                    });
                    continue;
                }
            };
            // The closure only needs the package's own unit.
            if ci == 0 {
                for req in bone_semantic::semantic_passes::api_closure_requirements(&checked.ws) {
                    if req.satisfied {
                        continue;
                    }
                    let Some(di) = closure_def(&checked.ws, &plan, req.required) else {
                        unrepairable.push(format!(
                            "the API closure requires `{}` ({}), which the package does not own",
                            req.required, req.reason
                        ));
                        continue;
                    };
                    let reasons = plan.exports.entry(di).or_default();
                    if reasons.closure.insert((declared_name(req.exporter.as_str()).to_string(), req.reason.to_string())) {
                        progressed = true;
                    }
                }
            }
            let cand = sites(&checked, &candidate_file_pkg(c, &plan), &HashMap::new());
            let mapped = map_sites(&base_sites[ci], &edits_by_id);
            let mut changed = Vec::new();
            for (key, before) in &mapped {
                let after = cand.get(key).cloned().unwrap_or_default();
                if &after == before {
                    continue;
                }
                match repair_for(&plan, &file_index, &baselines[ci], c, key, &edits_by_id) {
                    Some(r) if !plan.repairs.iter().any(|x| x.file == r.file && x.offset == r.offset) => {
                        plan.repairs.push(r);
                        progressed = true;
                    }
                    Some(_) => changed.push(describe_change(key, before, &after)),
                    None => changed.push(describe_change(key, before, &after)),
                }
            }
            verification.push(Verification {
                ctx: c.name().to_string(),
                compared: mapped.len(),
                changed,
                errors: checked.errors.iter().map(|e| render_one(&sources, e)).collect(),
            });
        }
        plan.verification = verification;
        plan.edits = edits;
        plan.imports_text = imports_text;
        if !unrepairable.is_empty() {
            plan.blockers.extend(unrepairable);
            break;
        }
        if !progressed {
            converged = true;
            break;
        }
    }
    if plan.blockers.is_empty() && !converged {
        plan.blockers.push(format!(
            "the candidate did not settle in {MAX_ROUNDS} rounds of closure exports and call repairs; the last round's edits are unverified"
        ));
    }
    for v in &plan.verification {
        if !v.errors.is_empty() || !v.changed.is_empty() {
            plan.blockers.push(format!(
                "the candidate does not verify in `{}`: {} error(s), {} changed resolution site(s)",
                v.ctx,
                v.errors.len(),
                v.changed.len()
            ));
        }
    }
}

/// Package ids owning a target (std, builtins and untagged files excluded).
fn owners(ws: &Workspace, t: &Target, file_pkg: &HashMap<Ustr, u32>) -> BTreeSet<u32> {
    let mids: Vec<ModuleId> = match t {
        Target::Fn(mid, _) => vec![*mid],
        Target::Key { modules, .. } => modules.clone(),
        Target::Unrecorded(_) => Vec::new(),
    };
    mids.into_iter()
        .filter(|m| *m != ModuleId::builtin())
        .filter_map(|m| file_pkg.get(&ws.module(m).file).copied())
        .collect()
}

/// The definition spans (file, start offset) a target names inside package
/// `p_id`.
fn p_def_spans(ws: &Workspace, t: &Target, file_pkg: &HashMap<Ustr, u32>, p_id: u32) -> Vec<(Ustr, usize)> {
    let in_p = |m: ModuleId| m != ModuleId::builtin() && file_pkg.get(&ws.module(m).file) == Some(&p_id);
    let mut out = Vec::new();
    match t {
        Target::Fn(mid, fid) => {
            if in_p(*mid) {
                let f = &ws.module(*mid).functions[*fid];
                out.push((ws.module(*mid).file, f.span.offset_range.0));
            }
        }
        Target::Key { key, modules } => {
            for &m in modules {
                if !in_p(m) {
                    continue;
                }
                let module = ws.module(m);
                if let Some(s) = ws.struct_by_name.get(key).filter(|s| s.module == m) {
                    out.push((module.file, module.structs[s.def].span.offset_range.0));
                }
                if let Some(e) = ws.enums_by_name.get(key).filter(|e| e.module == m) {
                    out.push((module.file, module.enums[e.def].span.offset_range.0));
                }
                if let Some(n) = ws.newtypes_by_name.get(key).filter(|n| n.module == m) {
                    out.push((module.file, module.newtypes[n.def].span.offset_range.0));
                }
                if let Some(g) = module.global_by_name.get(key) {
                    out.push((module.file, module.globals[*g].span.offset_range.0));
                }
                if let Some(fns) = ws.functions_by_name.get(key) {
                    for (fm, fid) in fns.iter() {
                        if *fm == m {
                            out.push((module.file, module.functions[*fid].span.offset_range.0));
                        }
                    }
                }
            }
        }
        Target::Unrecorded(_) => {}
    }
    out.sort();
    out.dedup();
    out
}

/// The package's type definition an unsatisfied closure requirement names.
fn closure_def(ws: &Workspace, plan: &Plan, required: Ustr) -> Option<usize> {
    let module = ws
        .struct_by_name
        .get(&required)
        .map(|s| s.module)
        .or_else(|| ws.enums_by_name.get(&required).map(|e| e.module))
        .or_else(|| ws.newtypes_by_name.get(&required).map(|n| n.module))?;
    let file = ws.module(module).file;
    let name = declared_name(required.as_str());
    plan.defs
        .iter()
        .position(|d| d.kind.is_type() && plan.files[d.file].id == file && d.name.as_str() == name)
}

/// Every edit the plan makes to the package's files, by file index.
fn candidate_edits(plan: &Plan) -> std::result::Result<HashMap<usize, Vec<Edit>>, String> {
    let mut edits: HashMap<usize, Vec<Edit>> = HashMap::new();
    for &di in plan.exports.keys() {
        let d = &plan.defs[di];
        let text = &plan.files[d.file].text;
        if d.kind == DefKind::Macro {
            return Err(format!(
                "macro `{}` is used by a consumer, and a macro cannot be `pub` (it expands before the namespace tree exists)",
                d.name
            ));
        }
        let at = d.span.0;
        if !text[at..].starts_with(|c: char| c == '@' || c.is_ascii_alphabetic()) {
            return Err(format!("{}:{}: unexpected item start for `{}`", short_path(&plan.files[d.file].id), d.line, d.name));
        }
        edits.entry(d.file).or_default().push(Edit { range: (at, at), text: "pub ".to_string() });
    }
    for (fi, a, parenthesized) in &plan.ascriptions {
        let text = &plan.files[*fi].text;
        let between = &text[a.operand.1..a.ty.0];
        if between.trim() != "::" {
            return Err(format!(
                "{}: the ascription at byte {} has text other than `::` between its operand and type (`{}`); refusing an unverified edit",
                short_path(&plan.files[*fi].id),
                a.node.0,
                between
            ));
        }
        let e = edits.entry(*fi).or_default();
        if !parenthesized {
            e.push(Edit { range: (a.node.0, a.node.0), text: "(".to_string() });
        }
        e.push(Edit { range: (a.operand.1, a.ty.0), text: " : ".to_string() });
        if !parenthesized {
            e.push(Edit { range: (a.node.1, a.node.1), text: ")".to_string() });
        }
    }
    for r in &plan.repairs {
        edits.entry(r.file).or_default().push(Edit { range: (r.offset, r.offset), text: format!("{}::", r.qualifier) });
    }
    for list in edits.values_mut() {
        list.sort_by_key(|e| e.range);
        for w in list.windows(2) {
            if w[0].range.1 > w[1].range.0 {
                return Err(format!("overlapping edits at byte {}", w[1].range.0));
            }
        }
    }
    Ok(edits)
}

fn imports_text(plan: &Plan) -> Option<String> {
    if plan.imports.is_empty() {
        return None;
    }
    let mut out = String::from(
        "// Dependency names every file of this package sees (spec 8.7). Written by\n// `rock modules migrate`; review and narrow as the package is subdivided.\n",
    );
    for i in &plan.imports {
        out.push_str(&format!("pub(package) use {}::*;\n", i.alias));
    }
    Some(out)
}

/// The re-parsed candidate must hold exactly the rewritten ascriptions, each
/// with its original operand and type text.
fn verify_ascriptions(plan: &Plan, texts: &HashMap<usize, String>) -> std::result::Result<(), String> {
    for (fi, f) in plan.files.iter().enumerate() {
        let want: Vec<(&str, &str)> = plan
            .ascriptions
            .iter()
            .filter(|(i, ..)| *i == fi)
            .map(|(_, a, _)| (&f.text[a.operand.0..a.operand.1], &f.text[a.ty.0..a.ty.1]))
            .collect();
        let text = &texts[&fi];
        let ast = skull_parser::parse_module_with_dialect(text, f.id, Dialect::V1)
            .map_err(|e| format!("{}: the candidate does not parse as v1: {}", short_path(&f.id), e.message))?;
        if let Some(e) = ast.recovery_errors.first() {
            return Err(format!("{}: the candidate does not parse as v1: {}", short_path(&f.id), e.message));
        }
        let mut got: Vec<(usize, &str, &str)> = ast
            .exprs
            .iter()
            .filter_map(|(_, e)| match &e.kind {
                skull_parser::ExprKind::ParenAscription(op, ty) => {
                    let op = ast.exprs[*op].span.offset_range;
                    let ty = ast.types[*ty].span.offset_range;
                    Some((e.span.offset_range.0, &text[op.0..op.1], &text[ty.0..ty.1]))
                }
                _ => None,
            })
            .collect();
        got.sort();
        let got: Vec<(&str, &str)> = got.into_iter().map(|(_, a, b)| (a, b)).collect();
        if got != want {
            return Err(format!(
                "{}: the rewritten ascriptions do not re-parse as written ({} expected, {} found); refusing an unverified edit",
                short_path(&f.id),
                want.len(),
                got.len()
            ));
        }
    }
    Ok(())
}

/// Deep copy of a context's sources with the package's files replaced by the
/// candidate text in the v1 dialect and the facade added.
fn candidate_sources(
    c: &Ctx,
    plan: &Plan,
    file_index: &HashMap<Ustr, usize>,
    texts: &HashMap<usize, String>,
    imports: Option<&str>,
) -> SourceFiles {
    let mut tag = None;
    let mut files = Vec::with_capacity(c.sources.files.len() + 1);
    for f in &c.sources.files {
        let mut f = f.borrow().clone();
        let id = Ustr::from(f.file.to_id_path().as_str());
        if let Some(fi) = file_index.get(&id) {
            if f.package.as_ref().map(|t| t.id) == Some(c.p_id) {
                f.contents = texts[fi].clone();
                f.dialect = Dialect::V1;
                tag = f.package.clone();
            }
        }
        files.push(Arc::new(AtomicRefCell::new(f)));
    }
    if let Some(src) = imports {
        let path = plan.package_root.join(IMPORTS_FILE).to_string_lossy().into_owned();
        let mut f = rock_core::source_file_from_path_and_contents(path, IMPORTS_FILE.to_string(), src.to_string());
        f.package = tag;
        f.dialect = Dialect::V1;
        files.push(Arc::new(AtomicRefCell::new(f)));
    }
    SourceFiles { files }
}

fn candidate_file_pkg(c: &Ctx, plan: &Plan) -> HashMap<Ustr, u32> {
    let mut m = c.file_pkg.clone();
    m.insert(Ustr::from(plan.package_root.join(IMPORTS_FILE).to_string_lossy().as_ref()), c.p_id);
    m
}

fn map_sites(base: &Sites, edits: &HashMap<Ustr, Vec<Edit>>) -> Sites {
    base.iter()
        .map(|(k, v)| {
            let key = match edits.get(&k.file) {
                Some(_) if matches!(k.kind, RefKind::Type | RefKind::Pattern) => k.clone(),
                Some(e) => SiteKey { file: k.file, start: map_offset(e, k.start), end: map_offset(e, k.end), kind: k.kind },
                None => k.clone(),
            };
            (key, v.clone())
        })
        .collect()
}

/// A changed site the plan can repair: a named, non-method call in one of the
/// package's files whose baseline target is a single function of std or of an
/// imported v1 dependency. The repair qualifies the callee.
fn repair_for(
    plan: &Plan,
    file_index: &HashMap<Ustr, usize>,
    base: &Checked,
    c: &Ctx,
    key: &SiteKey,
    edits: &HashMap<Ustr, Vec<Edit>>,
) -> Option<Repair> {
    if key.kind != RefKind::Call {
        return None;
    }
    let fi = *file_index.get(&key.file)?;
    let file_edits = edits.get(&key.file).cloned().unwrap_or_default();
    let r = base.refs.iter().find(|r| {
        r.kind == RefKind::Call
            && base.ws.module(r.from).file == key.file
            && r.span.is_some_and(|s| {
                map_offset(&file_edits, s.offset_range.0) == key.start && map_offset(&file_edits, s.offset_range.1) == key.end
            })
    })?;
    let site = r.callee?;
    if site.method || site.name.starts_with("__op_") {
        return None;
    }
    let Target::Fn(mid, _) = r.target else { return None };
    let owner_file = base.ws.module(mid).file;
    let qualifier = if is_stdlib_module_file(owner_file.as_str()) {
        "std".to_string()
    } else {
        let owner = *c.file_pkg.get(&owner_file)?;
        let node = c.graph.package(PackageId(owner));
        plan.imports.iter().find(|i| i.package == node.manifest_name)?.alias.clone()
    };
    let text = &plan.files[fi].text;
    let (a, b) = site.name_span.offset_range;
    if text.get(a..b) != Some(site.name.as_str()) {
        return None;
    }
    Some(Repair {
        file: fi,
        offset: a,
        line: site.name_span.line_range.0 + 1,
        name: site.name.to_string(),
        qualifier: qualifier.clone(),
        reason: format!(
            "a v1 tier would hide the `{}` this call selected from `{}` (spec 8.3); qualifying keeps the target",
            site.name,
            if qualifier == "std" { "std".to_string() } else { qualifier }
        ),
    })
}

fn describe_change(key: &SiteKey, before: &[TargetId], after: &[TargetId]) -> String {
    let show = |v: &[TargetId]| {
        if v.is_empty() {
            return "nothing".to_string();
        }
        v.iter()
            .map(|t| match t {
                TargetId::Fn { file, name, ordinal } => format!("fn {name}#{ordinal} in {}", short_path(file)),
                TargetId::Def { file, name } => format!("{name} in {}", short_path(file)),
                TargetId::Family { files, name } => format!("{name} across {} file(s)", files.len()),
                TargetId::Unrecorded(n) => format!("unrecorded {n}"),
            })
            .collect::<Vec<_>>()
            .join(" + ")
    };
    format!(
        "{} bytes {}..{} ({}): {} -> {}",
        short_path(&key.file),
        key.start,
        key.end,
        key.kind.as_str(),
        show(before),
        show(after)
    )
}

/// Raw backend sources that name a definition whose emitted symbol a v1 owner
/// changes, in the package and in its consumers (spec 13, 15.2 "raw ABI
/// ambiguities"). An `inline(lua)` body's references are exact
/// (`modules_analysis::lua_references`): a free global it reads or writes,
/// or a string it can look up through `_G`. An `inline(js)` body is still a
/// text search, labelled as an estimate. `Err` when the census cannot run.
fn raw_abi_scan(plan: &mut Plan, ctxs: &[Ctx], file_index: &HashMap<Ustr, usize>) -> std::result::Result<(), String> {
    let moved: BTreeSet<&str> = plan
        .defs
        .iter()
        .filter(|d| !d.exempt_symbol && matches!(d.kind, DefKind::Fn | DefKind::Global | DefKind::Struct | DefKind::Enum | DefKind::Newtype))
        .map(|d| d.name.as_str())
        .collect();
    // A legacy symbol is the declared name or the name plus a mangled
    // signature suffix (`name__ret_T`).
    let moved_names = |words: &mut dyn Iterator<Item = &str>| -> BTreeSet<String> {
        words
            .map(|w| w.split("__").next().unwrap_or(w))
            .filter(|stem| moved.contains(stem))
            .map(str::to_string)
            .collect()
    };
    let mut sites: Vec<(String, InlineSource)> = Vec::new();
    for d in &plan.defs {
        if let Some(inline) = &d.inline {
            let site = format!("{}:{} `{}`", short_path(&plan.files[d.file].id), d.line, d.name);
            sites.push((site, inline.clone()));
        }
    }
    for c in &ctxs[1..] {
        for f in &c.sources.files {
            let f = f.borrow();
            let id = Ustr::from(f.file.to_id_path().as_str());
            if file_index.contains_key(&id) || c.file_pkg.get(&id).is_none() {
                continue;
            }
            let Ok(ast) = skull_parser::parse_module_with_dialect(&f.contents, id, f.dialect) else { continue };
            for item in &ast.items {
                if let skull_parser::ItemKind::Function(fd) = &item.kind {
                    if let Some(inline) = InlineSource::of(fd) {
                        let site = format!("{}:{} `{}` ({})", short_path(&id), item.span.line_range.0 + 1, fd.name.name, c.name());
                        sites.push((site, inline));
                    }
                }
            }
        }
    }
    let (lua, js): (Vec<_>, Vec<_>) = sites.into_iter().partition(|(_, s)| s.backend != skull_parser::FnBackend::Js);
    let bodies: Vec<modules_analysis::LuaBody> = lua
        .iter()
        .map(|(_, s)| modules_analysis::LuaBody { params: s.params.clone(), returns_value: s.returns_value, src: s.src })
        .collect();
    let refs = modules_analysis::lua_references(&bodies)?;
    let tail = "whose emitted symbol changes when the package becomes v1; mark the definition `@nomangle` or reach it through Rock before migrating";
    let listed = |names: &BTreeSet<String>| names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
    for ((site, _), r) in lua.iter().zip(refs) {
        if let Some(error) = r.error {
            plan.blockers.push(format!(
                "raw ABI review [unknown]: the inline body at {site} does not compile as Lua ({error}), so its references to renamed symbols are unknown"
            ));
            continue;
        }
        let globals = moved_names(&mut r.globals.iter().map(String::as_str));
        if !globals.is_empty() {
            plan.blockers.push(format!("raw ABI review [exact]: the inline body at {site} names {}, {tail}", listed(&globals)));
        }
        let via_g = moved_names(&mut r.via_g.iter().map(String::as_str));
        if !via_g.is_empty() {
            plan.blockers.push(format!(
                "raw ABI review [exact]: the inline body at {site} can name {} through `_G`, {tail}",
                listed(&via_g)
            ));
        }
    }
    for (site, s) in &js {
        let names = moved_names(&mut identifiers(s.src.as_str()));
        if !names.is_empty() {
            plan.blockers.push(format!("raw ABI review [text-search estimate]: the inline body at {site} names {}, {tail}", listed(&names)));
        }
    }
    Ok(())
}

fn identifiers(src: &str) -> impl Iterator<Item = &str> {
    src.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty() && !w.as_bytes()[0].is_ascii_digit())
}

/// `[language] modules = "v1"` added to a manifest, comment-preserving.
fn manifest_with_v1(text: &str) -> std::result::Result<String, String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let header = |l: &str| l.trim_start().starts_with('[');
    let is = |l: &str, name: &str| l.trim() == name;
    let out = if let Some(li) = lines.iter().position(|l| is(l, "[language]")) {
        let end = lines[li + 1..].iter().position(|l| header(l)).map(|p| li + 1 + p).unwrap_or(lines.len());
        let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        match (li + 1..end).find(|&i| lines[i].trim_start().starts_with("modules")) {
            Some(i) => out[i] = "modules = \"v1\"\n".to_string(),
            None => out.insert(li + 1, "modules = \"v1\"\n".to_string()),
        }
        out.concat()
    } else {
        let pkg = lines.iter().position(|l| is(l, "[package]")).ok_or("no [package] table")?;
        let next = lines[pkg + 1..].iter().position(|l| header(l)).map(|p| pkg + 1 + p);
        let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        match next {
            Some(i) => out.insert(i, "[language]\nmodules = \"v1\"\n\n".to_string()),
            None => {
                if !text.ends_with('\n') {
                    out.push("\n".to_string());
                }
                out.push("\n[language]\nmodules = \"v1\"\n".to_string());
            }
        }
        out.concat()
    };
    let parsed = StickConfig::new_from_str(&out).map_err(|e| format!("the edited manifest does not parse: {e:#}"))?;
    if parsed.dialect() != Dialect::V1 {
        return Err("the edited manifest does not select modules-v1".to_string());
    }
    Ok(out)
}

// --- apply ---------------------------------------------------------------------

fn apply(plan: &Plan) -> std::result::Result<Vec<Verification>, String> {
    // Stale check first: every file must still hold what was analyzed.
    for f in &plan.files {
        let now = std::fs::read_to_string(&f.path).map_err(|e| format!("{}: {e}", f.path.display()))?;
        if now != f.text {
            return Err(format!("{} changed since it was analyzed; nothing written", f.path.display()));
        }
    }
    let manifest_path = plan.package_root.join("stick.toml");
    let imports_path = plan.package_root.join(IMPORTS_FILE);
    if let Some((before, _)) = &plan.manifest {
        let now = std::fs::read_to_string(&manifest_path).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        if &now != before {
            return Err(format!("{} changed since it was analyzed; nothing written", manifest_path.display()));
        }
    }
    if plan.imports_text.is_some() && imports_path.exists() {
        return Err(format!("{} already exists; nothing written", imports_path.display()));
    }
    let mut writes: Vec<(PathBuf, String)> = Vec::new();
    for (fi, f) in plan.files.iter().enumerate() {
        if let Some(e) = plan.edits.get(&fi) {
            writes.push((f.path.clone(), apply_edits(&f.text, e)?));
        }
    }
    if let Some((_, after)) = &plan.manifest {
        writes.push((manifest_path, after.clone()));
    }
    if let Some(t) = &plan.imports_text {
        writes.push((imports_path, t.clone()));
    }
    for (path, text) in &writes {
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }

    // Re-verify from disk: the written package checks in every unit, with the
    // closure gate, and every site resolves as the baseline did.
    let mut out = Vec::new();
    let mut units = vec![(plan.package_root.clone(), PopMode::Test)];
    units.extend(plan.consumers.iter().map(|(_, root, mode)| (root.clone(), *mode)));
    for (dir, mode) in &units {
        let c = load_ctx(dir, &plan.package_root, *mode)?;
        let mut v = Verification { ctx: c.name().to_string(), compared: 0, changed: Vec::new(), errors: Vec::new() };
        match check_sources(&c.sources) {
            Ok(checked) => {
                v.errors.extend(checked.errors.iter().map(|e| render_one(&c.sources, e)));
                if let Err(e) = bone_semantic::semantic_passes::pass_validate_api_closure(&checked.ws) {
                    v.errors.push(render_one(&c.sources, &e));
                }
                v.compared = modules_analysis::references(&checked.ws).len();
            }
            Err((stage, errs)) => v.errors.push(format!("{stage}:\n{}", render(&c.sources, &errs))),
        }
        out.push(v);
    }
    Ok(out)
}

// --- rendering -------------------------------------------------------------------

fn render(sources: &SourceFiles, errs: &[CompilerError]) -> String {
    crate::skull_backend::render_project_errs(sources, errs)
}

fn render_one(sources: &SourceFiles, e: &CompilerError) -> String {
    render(sources, std::slice::from_ref(e))
}

fn short_path(id: &str) -> String {
    let p = Path::new(id);
    std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.canonicalize().ok())
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| id.to_string())
}

fn kinds_text(by_kind: &BTreeMap<RefKind, usize>) -> String {
    by_kind.iter().map(|(k, n)| format!("{} {n}", k.as_str().replace('_', " "))).collect::<Vec<_>>().join(", ")
}

fn render_pretty(outcome: &Outcome, mode: Mode, applied: Option<&std::result::Result<Vec<Verification>, String>>) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let plan = match outcome {
        Outcome::AlreadyV1(name) => {
            let _ = writeln!(o, "rock modules migrate: {name} is already modules-v1; nothing to migrate");
            return o;
        }
        Outcome::Failed { stage, message } => {
            let _ = writeln!(o, "rock modules migrate: analysis FAILED at {stage} [unknown]");
            for l in message.lines() {
                let _ = writeln!(o, "  {l}");
            }
            let _ = writeln!(o, "\nno plan: a failed analysis is not a clean result");
            return o;
        }
        Outcome::Planned(p) => p,
    };
    let _ = writeln!(o, "rock modules migrate: {} ({})", plan.package, short_path(&plan.package_root.to_string_lossy()));
    let _ = writeln!(o, "\nconfiguration [exact]");
    let _ = writeln!(o, "  package analyzed with its tests: {} file(s)", plan.files.len());
    if plan.consumers.is_empty() {
        let _ = writeln!(
            o,
            "  consumers: NONE named -- nothing is exported; name every consumer with --consumer <dir>, since current consumers are the only evidence of API intent"
        );
    }
    for (name, root, mode) in &plan.consumers {
        let pop = match mode {
            PopMode::Test => "with its tests (the `rock test` population)",
            PopMode::Production => "its `rock compile` population only (its test population does not check; see the notes)",
        };
        let _ = writeln!(o, "  consumer: {name} ({}), {pop}", short_path(&root.to_string_lossy()));
    }

    let _ = writeln!(o, "\nmanifest [exact]");
    match &plan.manifest {
        Some(_) => {
            let _ = writeln!(o, "  stick.toml: [language] modules = \"v1\"");
        }
        None => {
            let _ = writeln!(o, "  stick.toml: no edit (blocked)");
        }
    }
    let _ = writeln!(o, "\nmodule headers\n  none: every file stays in the package root (the spec 15.2 default)");

    let _ = writeln!(o, "\nimports [exact]");
    if plan.imports.is_empty() {
        let _ = writeln!(o, "  none: the package's bodies reference only themselves and std");
    } else {
        let _ = writeln!(o, "  {IMPORTS_FILE} (new):");
        for i in &plan.imports {
            let refs: usize = i.fragments.values().sum();
            let _ = writeln!(
                o,
                "    pub(package) use {}::*;   // {} fragment(s), {refs} reference(s) to {}",
                i.alias,
                i.fragments.len(),
                i.package
            );
        }
        let _ = writeln!(o, "  per fragment:");
        let mut per_file: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for i in &plan.imports {
            for (fi, n) in &i.fragments {
                per_file.entry(*fi).or_default().push(format!("{} ({n})", i.alias));
            }
        }
        for (fi, list) in per_file {
            let _ = writeln!(o, "    {}: {}", short_path(&plan.files[fi].id), list.join(", "));
        }
    }

    let exportable = plan.defs.iter().filter(|d| d.kind != DefKind::Macro).count();
    let _ = writeln!(o, "\nexports [exact]: {} of {} top-level definition(s) become `pub`", plan.exports.len(), exportable);
    for (&di, reasons) in &plan.exports {
        let d = &plan.defs[di];
        let mut why: Vec<String> = reasons
            .consumers
            .iter()
            .map(|(c, u)| format!("used by {c} ({}{})", kinds_text(&u.by_kind), u.example.as_ref().map(|e| format!("; first at {e}")).unwrap_or_default()))
            .collect();
        why.extend(reasons.closure.iter().map(|(e, r)| format!("{r} `{e}` (API closure)")));
        let _ = writeln!(
            o,
            "  pub {} {}  {}:{}  -- {}",
            d.kind.as_str(),
            d.name,
            short_path(&plan.files[d.file].id),
            d.line,
            why.join("; ")
        );
    }
    let private: Vec<String> = plan
        .defs
        .iter()
        .enumerate()
        .filter(|(i, d)| !plan.exports.contains_key(i) && d.kind != DefKind::Macro)
        .map(|(_, d)| d.name.to_string())
        .collect();
    let _ = writeln!(o, "  stays private (no named consumer uses it): {}", if private.is_empty() { "none".to_string() } else { private.join(", ") });

    let _ = writeln!(o, "\nascriptions [exact]: {} legacy `expr :: T` node(s)", plan.ascriptions.len());
    for (fi, a, _) in &plan.ascriptions {
        let f = &plan.files[*fi];
        let _ = writeln!(
            o,
            "  {} byte {}: `{}` -> `({} : {})`",
            short_path(&f.id),
            a.node.0,
            &f.text[a.node.0..a.node.1],
            &f.text[a.operand.0..a.operand.1],
            &f.text[a.ty.0..a.ty.1]
        );
    }

    let _ = writeln!(o, "\nrepairs [exact]: {} qualified call(s)", plan.repairs.len());
    for r in &plan.repairs {
        let _ = writeln!(o, "  {}:{} `{}` -> `{}::{}`: {}", short_path(&plan.files[r.file].id), r.line, r.name, r.qualifier, r.name, r.reason);
    }

    if !plan.notes.is_empty() {
        let _ = writeln!(o, "\nreview notes");
        for n in &plan.notes {
            let _ = writeln!(o, "  - {n}");
        }
    }

    let _ = writeln!(o, "\nverification (the candidate, checked in memory) [exact]");
    if plan.verification.is_empty() {
        let _ = writeln!(o, "  not run: the plan is blocked before a candidate exists");
    }
    for v in &plan.verification {
        let _ = writeln!(
            o,
            "  {}: {} error(s); {} resolved site(s) compared with the legacy baseline, {} changed",
            v.ctx,
            v.errors.len(),
            v.compared,
            v.changed.len()
        );
        for c in v.changed.iter().take(20) {
            let _ = writeln!(o, "    changed: {c}");
        }
        for e in v.errors.iter().take(5) {
            for l in e.lines() {
                let _ = writeln!(o, "    {l}");
            }
        }
    }

    if !plan.blockers.is_empty() {
        let _ = writeln!(o, "\nBLOCKED -- {} item(s); nothing is written:", plan.blockers.len());
        for b in &plan.blockers {
            let _ = writeln!(o, "  - {b}");
        }
        return o;
    }
    match (mode, applied) {
        (Mode::Check, _) => {
            let _ = writeln!(o, "\nmigration needed: run with --apply to write the edits above");
        }
        (Mode::Apply, Some(Err(e))) => {
            let _ = writeln!(o, "\napply FAILED: {e}");
        }
        (Mode::Apply, Some(Ok(vs))) => {
            let _ = writeln!(o, "\napplied; re-checked from disk (typecheck and API closure; emission is not part of this check -- run `rock test` on the package and build each consumer):");
            for v in vs {
                let _ = writeln!(o, "  {}: {} error(s), {} reference(s)", v.ctx, v.errors.len(), v.compared);
                for e in &v.errors {
                    for l in e.lines() {
                        let _ = writeln!(o, "    {l}");
                    }
                }
            }
        }
        (Mode::Apply, None) => {}
    }
    o
}

fn render_json(outcome: &Outcome, mode: Mode, applied: Option<&std::result::Result<Vec<Verification>, String>>) -> Value {
    let mode_s = match mode {
        Mode::Check => "check",
        Mode::Apply => "apply",
    };
    let plan = match outcome {
        Outcome::AlreadyV1(name) => return json!({ "package": name, "mode": mode_s, "verdict": "already_v1" }),
        Outcome::Failed { stage, message } => {
            return json!({ "mode": mode_s, "verdict": "analysis_failed", "status": "unknown", "stage": stage, "message": message })
        }
        Outcome::Planned(p) => p,
    };
    let exports: Vec<Value> = plan
        .exports
        .iter()
        .map(|(&di, r)| {
            let d = &plan.defs[di];
            json!({
                "name": d.name.as_str(),
                "kind": d.kind.as_str(),
                "file": short_path(&plan.files[d.file].id),
                "line": d.line,
                "consumers": r.consumers.iter().map(|(c, u)| json!({
                    "package": c,
                    "by_kind": u.by_kind.iter().map(|(k, n)| (k.as_str().to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
                    "first_site": u.example,
                })).collect::<Vec<_>>(),
                "closure": r.closure.iter().map(|(e, reason)| json!({ "exporter": e, "reason": reason })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let private: Vec<Value> = plan
        .defs
        .iter()
        .enumerate()
        .filter(|(i, d)| !plan.exports.contains_key(i) && d.kind != DefKind::Macro)
        .map(|(_, d)| json!({ "name": d.name.as_str(), "kind": d.kind.as_str(), "file": short_path(&plan.files[d.file].id), "line": d.line }))
        .collect();
    let verdict = if !plan.blockers.is_empty() {
        "blocked"
    } else {
        match (mode, applied) {
            (Mode::Check, _) => "migration_needed",
            (Mode::Apply, Some(Ok(v))) if v.iter().all(|v| v.errors.is_empty()) => "applied",
            (Mode::Apply, _) => "apply_failed",
        }
    };
    json!({
        "package": plan.package,
        "mode": mode_s,
        "verdict": verdict,
        "consumers": plan.consumers.iter().map(|(n, r, m)| json!({
            "package": n,
            "root": short_path(&r.to_string_lossy()),
            "population": match m { PopMode::Test => "test", PopMode::Production => "production" },
        })).collect::<Vec<_>>(),
        "test_only_uses": plan.test_uses.iter().map(|(&di, uses)| json!({
            "name": plan.defs[di].name.as_str(),
            "exported": plan.exports.contains_key(&di),
            "consumers": uses.iter().map(|(c, u)| json!({
                "package": c,
                "by_kind": u.by_kind.iter().map(|(k, n)| (k.as_str().to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
                "first_site": u.example,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "manifest": { "status": "exact", "edit": plan.manifest.as_ref().map(|_| "[language] modules = \"v1\"") },
        "module_headers": { "status": "exact", "edits": [] },
        "imports": {
            "status": "exact",
            "file": plan.imports_text.as_ref().map(|_| IMPORTS_FILE),
            "globs": plan.imports.iter().map(|i| json!({
                "alias": i.alias,
                "package": i.package,
                "fragments": i.fragments.iter().map(|(fi, n)| json!({ "file": short_path(&plan.files[*fi].id), "references": n })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        },
        "exports": { "status": "exact", "pub": exports, "private": private },
        "ascriptions": {
            "status": "exact",
            "count": plan.ascriptions.len(),
            "sites": plan.ascriptions.iter().map(|(fi, a, _)| json!({ "file": short_path(&plan.files[*fi].id), "offset": a.node.0 })).collect::<Vec<_>>(),
        },
        "repairs": plan.repairs.iter().map(|r| json!({
            "file": short_path(&plan.files[r.file].id), "line": r.line, "name": r.name, "qualifier": r.qualifier, "reason": r.reason,
        })).collect::<Vec<_>>(),
        "notes": plan.notes,
        "blockers": plan.blockers,
        "verification": plan.verification.iter().map(|v| json!({
            "context": v.ctx, "errors": v.errors, "compared_sites": v.compared, "changed_sites": v.changed,
        })).collect::<Vec<_>>(),
        "applied": applied.map(|a| match a {
            Ok(vs) => json!(vs.iter().map(|v| json!({ "context": v.ctx, "errors": v.errors, "references": v.compared })).collect::<Vec<_>>()),
            Err(e) => json!({ "error": e }),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_gains_a_language_table_after_package() {
        let src = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n# deps\n[dependencies]\n";
        let out = manifest_with_v1(src).unwrap();
        assert_eq!(out, "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n# deps\n[language]\nmodules = \"v1\"\n\n[dependencies]\n");
        let src = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[language]\nmodules = \"legacy\" # old\n";
        assert_eq!(manifest_with_v1(src).unwrap(), "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[language]\nmodules = \"v1\"\n");
    }

    #[test]
    fn offsets_map_through_insertions_and_replacements() {
        let edits = vec![
            Edit { range: (0, 0), text: "pub ".into() },
            Edit { range: (10, 10), text: "std::".into() },
            Edit { range: (20, 24), text: " : ".into() },
        ];
        assert_eq!(map_offset(&edits, 0), 0);
        assert_eq!(map_offset(&edits, 5), 9);
        assert_eq!(map_offset(&edits, 10), 14);
        assert_eq!(map_offset(&edits, 11), 20);
        assert_eq!(map_offset(&edits, 24), 32);
        assert_eq!(apply_edits("0123456789abcdefghij::  xyz", &edits).unwrap(), "pub 0123456789std::abcdefghij : xyz");
    }
}
