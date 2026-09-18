//! `rock modules audit <project> [--closed] [--format pretty|json]`
//! (modules spec 15.1, plan M5).
//!
//! Read-only. The package graph is the host's `PackageGraph`; the analyzed
//! population and the semantic pass are the ones `rock compile` runs
//! (`discover_sources` -> `production_only` -> `skull_assemble_project` ->
//! `check_modules`), except for a root that lives under a `tests/` path, which
//! `rock compile` would drop entirely and which is therefore analyzed with the
//! whole-package population `rock test` builds. Every section carries a status:
//! `exact` (compiler- or manifest-derived), `unknown` (analysis did not
//! complete) or `not_computed` (this version does not derive it). A section
//! that was not analyzed is never printed as zero.
//!
//! Exit status: 0 report complete; 3 `--closed` and a legacy package is
//! reachable; 4 the graph or the semantic pass failed (never a clean result).
//! 1 and 2 remain the CLI's generic error and clap's usage error.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bone_core::hir::Expr;
use bone_core::modules::is_stdlib_module_file;
use bone_core::prelude::ModuleId;
use rock_core::errors::CompilerError;
use rock_core::source_code::Dialect;
use rock_core::SourceFiles;
use rock_host::stick::graph::{PackageGraph, PackageId};
use rock_host::stick::StickConfig;
use serde_json::{json, Value};

pub enum Format {
    Pretty,
    Json,
}

const EXIT_NOT_CLOSED: i32 = 3;
const EXIT_ANALYSIS_FAILED: i32 = 4;
/// Reach paths listed per package; the count is always exact.
const PATHS_LISTED: usize = 8;
/// Example keys listed per unattributed bucket; the count is always exact.
const KEYS_LISTED: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Exact,
    Unknown,
    NotComputed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Exact => "exact",
            Status::Unknown => "unknown",
            Status::NotComputed => "not_computed",
        }
    }
}

/// The owner of a definition a body refers to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    Package(u32),
    /// The compiler-owned standard library (spec 5), identified by the
    /// loader's stdlib file ids.
    Std,
    /// The workspace's builtin module (no source file).
    Builtin,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    calls: usize,
    values: usize,
    /// Struct literals and newtype constructor calls.
    constructions: usize,
    enum_variants: usize,
}

impl Counts {
    fn total(&self) -> usize {
        self.calls + self.values + self.constructions + self.enum_variants
    }
}

/// References whose target key the analysis cannot attribute to exactly one
/// package: no definition at all (a raw `_G.X` Lua global), or a key defined in
/// several packages (a legacy overload family spanning packages, used as a
/// value). Counted, never guessed.
#[derive(Default)]
struct Unattributed {
    no_definition: BTreeMap<String, usize>,
    several_owners: BTreeMap<String, usize>,
    /// Named call expressions inference recorded no target for, by callee
    /// spelling. The completeness check on the `calls` column.
    unrecorded_calls: BTreeMap<String, usize>,
}

#[derive(Default)]
struct RefTable {
    /// (from package, to owner) -> counts. Same-package references are not
    /// recorded.
    edges: BTreeMap<(u32, Owner), Counts>,
    unattributed: BTreeMap<u32, Unattributed>,
}

struct Diag {
    file: String,
    line: usize,
    col: usize,
    message: String,
}

enum Analysis {
    Ok(RefTable),
    Failed {
        stage: &'static str,
        diagnostics: Vec<Diag>,
        rendered: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PopMode {
    Production,
    Test,
}

struct PopRow {
    package: u32,
    included: usize,
    excluded_test_only: usize,
}

struct Population {
    mode: PopMode,
    rows: Vec<PopRow>,
    std_modules: usize,
}

enum Revision {
    Known { commit: String, dirty_paths: Vec<String> },
    Unknown(String),
}

struct Report {
    project: String,
    closed_requested: bool,
    channel: String,
    revision: Revision,
    graph: std::result::Result<PackageGraph, String>,
    population: Option<Population>,
    analysis: Analysis,
}

/// Run the audit and print it. Returns the process exit code.
pub fn run(project: &Path, closed: bool, format: Format) -> Result<i32> {
    let Some((config_path, config)) = StickConfig::find_from_dir(project)? else {
        anyhow::bail!(
            "rock modules audit: no stick.toml at or above `{}`; the audit reads a package graph",
            project.display()
        );
    };
    let manifest_dir = config_path.parent().unwrap_or(&config_path).to_path_buf();
    let channel = rock_host::stick::resolved_channel_for(&config);
    let graph = PackageGraph::build_with_channel(&manifest_dir, &config, &channel)
        .map_err(|e| format!("{e:#}"));

    let mut report = Report {
        project: project.display().to_string(),
        closed_requested: closed,
        channel,
        revision: Revision::Unknown("package graph did not resolve".to_string()),
        graph,
        population: None,
        analysis: Analysis::Failed {
            stage: "package graph",
            diagnostics: Vec::new(),
            rendered: String::new(),
        },
    };

    match &report.graph {
        Err(e) => {
            report.analysis = failed_msg("package graph", e.clone());
        }
        Ok(graph) => {
            report.revision = revision(&manifest_dir, graph);
            let (population, analysis) = analyze(project, graph);
            report.population = population;
            report.analysis = analysis;
        }
    }

    let verdict = closed_verdict(&report);
    match format {
        Format::Pretty => print!("{}", render_pretty(&report, &verdict)),
        Format::Json => println!("{}", serde_json::to_string_pretty(&render_json(&report, &verdict))?),
    }
    std::io::stdout().flush()?;

    let analysis_failed = matches!(report.analysis, Analysis::Failed { .. });
    Ok(match verdict {
        Verdict::Fail(_) if closed => EXIT_NOT_CLOSED,
        _ if analysis_failed => EXIT_ANALYSIS_FAILED,
        _ => 0,
    })
}

fn analyze(project: &Path, graph: &PackageGraph) -> (Option<Population>, Analysis) {
    let spec = crate::unified_compile::SourceSpec::Directory(project.to_path_buf());
    let discovered = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => return (None, failed_msg("source discovery", format!("{e:#}"))),
    };

    // `rock compile` drops every file under a `tests/` path component; a root
    // that itself lives under one would be analyzed empty, so it takes the
    // population `rock test <dir>` builds instead.
    let root_path = graph.package(graph.root()).root.to_string_lossy().into_owned();
    let mode = if SourceFiles::is_test_only_path(&root_path) { PopMode::Test } else { PopMode::Production };
    let analyzed = match mode {
        PopMode::Production => discovered.production_only(),
        PopMode::Test => discovered.clone(),
    };

    let mut rows: BTreeMap<u32, PopRow> = graph
        .packages()
        .iter()
        .map(|n| (n.id.0, PopRow { package: n.id.0, included: 0, excluded_test_only: 0 }))
        .collect();
    let mut file_pkg: HashMap<ustr::Ustr, u32> = HashMap::new();
    for f in &analyzed.files {
        let f = f.borrow();
        if let Some(tag) = &f.package {
            file_pkg.insert(ustr::Ustr::from(f.file.to_id_path().as_str()), tag.id);
            if let Some(row) = rows.get_mut(&tag.id) {
                row.included += 1;
            }
        }
    }
    for f in &discovered.files {
        let f = f.borrow();
        if let Some(tag) = &f.package {
            if !file_pkg.contains_key(&ustr::Ustr::from(f.file.to_id_path().as_str())) {
                if let Some(row) = rows.get_mut(&tag.id) {
                    row.excluded_test_only += 1;
                }
            }
        }
    }

    let untagged: Vec<String> = analyzed
        .files
        .iter()
        .filter(|f| f.borrow().package.is_none())
        .map(|f| f.borrow().file.to_id_path())
        .collect();

    let population = Population {
        mode,
        rows: rows.into_values().collect(),
        std_modules: rock_host::stdlib_loader::stdlib_files().len(),
    };
    let (mut modules, _entry) = match crate::skull_backend::skull_assemble_project(&analyzed) {
        Ok(v) => v,
        Err(errs) => return (Some(population), failed("parse", &analyzed, &errs)),
    };

    if !untagged.is_empty() {
        let message = format!(
            "{} analyzed file(s) carry no package tag, so their references cannot be attributed: {}",
            untagged.len(),
            untagged.join(", ")
        );
        return (Some(population), failed_msg("package attribution", message));
    }

    let ws = match crate::bone_backend::check_modules(&mut modules) {
        Ok(ws) => ws,
        Err(errs) => return (Some(population), failed("semantic pass", &analyzed, &errs)),
    };
    match count_references(&ws, &file_pkg) {
        Ok(table) => (Some(population), Analysis::Ok(table)),
        Err(message) => (Some(population), failed_msg("package attribution", message)),
    }
}

/// A failure carried as one message, with no source location.
fn failed_msg(stage: &'static str, message: String) -> Analysis {
    Analysis::Failed {
        stage,
        diagnostics: vec![Diag { file: String::new(), line: 0, col: 0, message: message.clone() }],
        rendered: message,
    }
}

fn failed(stage: &'static str, sources: &SourceFiles, errs: &[CompilerError]) -> Analysis {
    let mut diagnostics = Vec::new();
    for e in errs {
        for d in std::iter::once(e).chain(e.siblings.iter()) {
            diagnostics.push(Diag {
                file: d.location.file.to_string(),
                line: d.location.line + 1,
                col: d.location.col + 1,
                message: d.message.clone(),
            });
        }
    }
    Analysis::Failed {
        stage,
        diagnostics,
        rendered: crate::skull_backend::render_project_errs(sources, errs),
    }
}

/// Count every cross-package reference the analyzed bodies resolve, keyed by
/// the package that owns the definition (the file that declares it).
///
/// Counted, per lowered body expression:
/// - calls: every call inference bound to a function
///   (`BodyInference::resolved_overload_calls`: named, UFCS, operator and
///   compound-assignment calls);
/// - values: `Expr::Global` -- a global, a function used as a value, or a type
///   used as a value -- and calls through a fn-valued global, attributed by
///   which modules define that key;
/// - constructions (struct literals, newtype constructor calls) and enum
///   variant constructions, by their nominal's key.
///
/// A named call with neither a recorded target nor a defining global/type is
/// listed as unrecorded rather than dropped.
fn count_references(
    ws: &bone_semantic::inference::Workspace,
    file_pkg: &HashMap<ustr::Ustr, u32>,
) -> std::result::Result<RefTable, String> {
    let owner_of = |mid: ModuleId| -> std::result::Result<Owner, String> {
        if mid == ModuleId::builtin() {
            return Ok(Owner::Builtin);
        }
        let file = ws.module(mid).file;
        if is_stdlib_module_file(file.as_str()) {
            return Ok(Owner::Std);
        }
        file_pkg
            .get(&file)
            .map(|&p| Owner::Package(p))
            .ok_or_else(|| format!("module `{file}` is neither stdlib nor an analyzed package file"))
    };

    let mut global_modules: HashMap<ustr::Ustr, Vec<ModuleId>> = HashMap::new();
    for m in &ws.modules {
        for key in m.global_by_name.keys() {
            global_modules.entry(*key).or_default().push(m.id);
        }
    }
    let type_modules = |key: ustr::Ustr| -> Vec<ModuleId> {
        let mut out = Vec::new();
        if let Some(s) = ws.struct_by_name.get(&key) {
            out.push(s.module);
        }
        if let Some(e) = ws.enums_by_name.get(&key) {
            out.push(e.module);
        }
        if let Some(n) = ws.newtypes_by_name.get(&key) {
            out.push(n.module);
        }
        out
    };
    let owners = |mids: Vec<ModuleId>| -> std::result::Result<BTreeSet<Owner>, String> {
        mids.into_iter().map(owner_of).collect()
    };

    let mut table = RefTable::default();
    for m in &ws.modules {
        if m.id == ModuleId::builtin() {
            continue;
        }
        let Owner::Package(from) = owner_of(m.id)? else { continue };
        let inference = ws.inference(m.id);
        for (body_owner, body) in &m.bodies {
            let bi = inference.and_then(|inf| inf.per_fn.get(body_owner));
            for (idx, expr) in body.expr_arena.iter() {
                let (key, mids, kind) = match expr {
                    Expr::Call { callee: bone_core::hir::Callee::Unresolved { name, .. }, .. } => {
                        if bi.is_some_and(|bi| bi.resolved_overload_calls.contains_key(&idx)) {
                            continue;
                        }
                        // Inference bound no function: a newtype constructor or
                        // a call through a fn-valued global, attributed like the
                        // definition it names.
                        let types = type_modules(*name);
                        let kind = if types.is_empty() { Kind::Value } else { Kind::Construction };
                        let mut mids = global_modules.get(name).cloned().unwrap_or_default();
                        mids.extend(types);
                        if mids.is_empty() {
                            let bucket = table.unattributed.entry(from).or_default();
                            *bucket.unrecorded_calls.entry(name.to_string()).or_default() += 1;
                            continue;
                        }
                        (*name, mids, kind)
                    }
                    Expr::Global(key) => {
                        let mut mids = global_modules.get(key).cloned().unwrap_or_default();
                        if let Some(fns) = ws.functions_by_name.get(key) {
                            mids.extend(fns.iter().map(|(mid, _)| *mid));
                        }
                        mids.extend(type_modules(*key));
                        (*key, mids, Kind::Value)
                    }
                    Expr::StructLiteral { name, .. } => (*name, type_modules(*name), Kind::Construction),
                    Expr::EnumVariant { enum_name, .. } => (*enum_name, type_modules(*enum_name), Kind::EnumVariant),
                    _ => continue,
                };
                let found = owners(mids)?;
                if found.len() != 1 {
                    let bucket = table.unattributed.entry(from).or_default();
                    let map = if found.is_empty() { &mut bucket.no_definition } else { &mut bucket.several_owners };
                    *map.entry(key.to_string()).or_default() += 1;
                    continue;
                }
                let to = *found.iter().next().expect("one owner");
                if to != Owner::Package(from) {
                    let counts = table.edges.entry((from, to)).or_default();
                    match kind {
                        Kind::Value => counts.values += 1,
                        Kind::Construction => counts.constructions += 1,
                        Kind::EnumVariant => counts.enum_variants += 1,
                    }
                }
            }
            let Some(bi) = bi else { continue };
            for (callee_mid, _) in bi.resolved_overload_calls.values() {
                let to = owner_of(*callee_mid)?;
                if to != Owner::Package(from) {
                    table.edges.entry((from, to)).or_default().calls += 1;
                }
            }
        }
    }
    Ok(table)
}

#[derive(Clone, Copy)]
enum Kind {
    Value,
    Construction,
    EnumVariant,
}

fn revision(manifest_dir: &Path, graph: &PackageGraph) -> Revision {
    let git = |args: &[&str]| -> std::result::Result<String, String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(manifest_dir)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let commit = match git(&["rev-parse", "HEAD"]) {
        Ok(c) => c.trim().to_string(),
        Err(e) => return Revision::Unknown(format!("not a git checkout: {e}")),
    };
    let mut paths: Vec<String> = graph
        .packages()
        .iter()
        .map(|n| n.root.to_string_lossy().into_owned())
        .collect();
    paths.push(rock_host::stdlib_loader::stdlib_dir().to_string_lossy().into_owned());
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    args.extend(paths.iter().map(String::as_str));
    match git(&args) {
        Ok(status) => Revision::Known {
            commit,
            dirty_paths: status.lines().map(|l| l.get(3..).unwrap_or(l).to_string()).collect(),
        },
        Err(e) => Revision::Unknown(format!("git status over the package roots failed: {e}")),
    }
}

// --- legacy boundaries and --closed ------------------------------------------

struct Reach {
    package: u32,
    /// Exact number of distinct package paths from the root.
    path_count: usize,
    /// Up to `PATHS_LISTED` of them, shortest first, as manifest names from
    /// the root.
    paths: Vec<Vec<String>>,
}

enum Verdict {
    Pass,
    /// Offending (legacy) packages, root first when it is one.
    Fail(Vec<u32>),
    Unknown(&'static str),
}

fn successors(graph: &PackageGraph, id: u32) -> Vec<u32> {
    let set: BTreeSet<u32> = graph
        .package(PackageId(id))
        .dependencies
        .iter()
        .map(|e| e.target.0)
        .collect();
    set.into_iter().collect()
}

/// Every package reachable from the root (the root included), with its reach
/// paths. The graph is acyclic (`PackageGraph` rejects cycles) and holds only
/// packages reachable from its root.
fn reach(graph: &PackageGraph) -> Vec<Reach> {
    let root = graph.root().0;
    let name = |id: u32| graph.package(PackageId(id)).manifest_name.clone();

    // Exact path counts: reversed load order visits every consumer before its
    // providers.
    let mut counts: HashMap<u32, usize> = HashMap::from([(root, 1)]);
    for id in graph.load_order().iter().rev() {
        let here = counts.get(&id.0).copied().unwrap_or(0);
        for next in successors(graph, id.0) {
            let slot = counts.entry(next).or_default();
            *slot = slot.saturating_add(here);
        }
    }

    // The listing: the paths a bounded DFS finds, shortest first. The bound
    // only shortens the listing; the counts above stay exact.
    const VISIT_BUDGET: usize = 100_000;
    let mut listed: HashMap<u32, Vec<Vec<u32>>> = HashMap::new();
    let mut visits = 0usize;
    let mut stack: Vec<Vec<u32>> = vec![vec![root]];
    while let Some(path) = stack.pop() {
        visits += 1;
        if visits > VISIT_BUDGET {
            break;
        }
        let at = *path.last().expect("non-empty path");
        for next in successors(graph, at).into_iter().rev() {
            let mut longer = path.clone();
            longer.push(next);
            stack.push(longer);
        }
        listed.entry(at).or_default().push(path);
    }

    let mut out: Vec<Reach> = counts
        .into_iter()
        .map(|(package, path_count)| {
            let mut paths: Vec<Vec<String>> = listed
                .remove(&package)
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.into_iter().map(name).collect())
                .collect();
            paths.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
            paths.truncate(PATHS_LISTED);
            Reach { package, path_count, paths }
        })
        .collect();
    out.sort_by_key(|r| (r.package != root, name(r.package)));
    out
}

fn closed_verdict(report: &Report) -> Verdict {
    let Ok(graph) = &report.graph else { return Verdict::Unknown("package graph failed") };
    let offending: Vec<u32> = reach(graph)
        .into_iter()
        .filter(|r| graph.package(PackageId(r.package)).dialect == Dialect::Legacy)
        .map(|r| r.package)
        .collect();
    if !offending.is_empty() {
        return Verdict::Fail(offending);
    }
    if matches!(report.analysis, Analysis::Failed { .. }) {
        return Verdict::Unknown("analysis failed");
    }
    Verdict::Pass
}

// --- not computed -----------------------------------------------------------

/// Spec 15.1 items this version does not derive, and what is missing.
const NOT_COMPUTED: &[(&str, &str)] = &[
    (
        "references in type positions, field accesses, patterns and decorator uses",
        "the counted kinds are calls, value references, constructions and enum variant constructions; nominals in signatures/annotations, `x.field` targets, match patterns and `@deco` uses need a walk over lowered types and inferred receiver types that this version does not do",
    ),
    (
        "compiler-synthesized protocol calls (for-in iter/next, implicit iterator coercion)",
        "recorded in BodyInference (`for_iter_protocol`, `implicit_iter_args`) but not counted as source references",
    ),
    (
        "affected declarations when projecting a selected package into a scoped root",
        "needs a projection planner over a migration candidate (`rock modules migrate`, not implemented)",
    ),
    (
        "required public-export closure and its reasons",
        "`pass_validate_api_closure` only rejects; there is no accessor returning the closure set with its reasons",
    ),
    (
        "import families needed per fragment",
        "resolution records the chosen key per site, not the import family that would have to be written",
    ),
    (
        "contextual-ascription nodes",
        "needs a parse-tree census of legacy `x :: T` ascriptions (planned for the migrate planner)",
    ),
    (
        "raw backend/ABI review sites",
        "no census of extern/inline(lua)/raw `_G` sites; raw `_G.X` reads appear only as unattributed value references",
    ),
    (
        "tests/sims relying on private internals",
        "needs per-site access classification of test/sim fragments against the owner's visibility",
    ),
    (
        "identity-string consumers",
        "no census of code that reads definition identity as a string",
    ),
    (
        "name collisions",
        "projection ambiguities are reported as diagnostics at use; no inventory of latent collisions",
    ),
    (
        "behavior-resolution changes against the legacy baseline",
        "needs a migration candidate to compare (`rock modules migrate`, not implemented)",
    ),
];

// --- rendering --------------------------------------------------------------

fn display_path(p: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.canonicalize().ok())
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(Path::to_path_buf))
        .map(|rel| if rel.as_os_str().is_empty() { PathBuf::from(".") } else { rel })
        .unwrap_or_else(|| p.to_path_buf())
        .display()
        .to_string()
}

fn dialect_str(d: Dialect) -> &'static str {
    match d {
        Dialect::V1 => "v1",
        Dialect::Legacy => "legacy",
    }
}

fn owner_name(graph: &PackageGraph, owner: Owner) -> String {
    match owner {
        Owner::Package(id) => graph.package(PackageId(id)).manifest_name.clone(),
        Owner::Std => "std".to_string(),
        Owner::Builtin => "compiler builtin".to_string(),
    }
}

/// Package ids `from` reaches through its dependencies (itself excluded).
fn reachable_from(graph: &PackageGraph, from: u32) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut stack = successors(graph, from);
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            stack.extend(successors(graph, id));
        }
    }
    seen
}

/// A reference edge to a package that `from` does not declare directly:
/// a back-reference when the target depends (transitively) on `from`,
/// otherwise an undeclared peer (which includes a transitive-only provider).
fn undeclared(graph: &PackageGraph, table: &RefTable) -> Vec<(u32, u32, &'static str, usize)> {
    let mut out = Vec::new();
    for (&(from, to), counts) in &table.edges {
        let Owner::Package(to) = to else { continue };
        let direct = successors(graph, from);
        if direct.contains(&to) {
            continue;
        }
        let class = if reachable_from(graph, to).contains(&from) { "back_reference" } else { "undeclared_peer" };
        out.push((from, to, class, counts.total()));
    }
    out
}

fn render_pretty(report: &Report, verdict: &Verdict) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let tag = |s: Status| format!("[{}]", s.as_str());
    let analysis_failed = matches!(report.analysis, Analysis::Failed { .. });
    let unknown = "unknown (analysis failed)";

    let _ = writeln!(o, "rock modules audit: {}", report.project);

    // Configuration.
    let _ = writeln!(o, "\nconfiguration {}", tag(Status::Exact));
    match &report.revision {
        Revision::Known { commit, dirty_paths } if dirty_paths.is_empty() => {
            let _ = writeln!(o, "  source revision: {commit} (package roots and stdlib clean)");
        }
        Revision::Known { commit, dirty_paths } => {
            let _ = writeln!(
                o,
                "  source revision: {commit} + {} uncommitted path(s) under the package roots/stdlib: {}",
                dirty_paths.len(),
                dirty_paths.join(", ")
            );
        }
        Revision::Unknown(why) => {
            let _ = writeln!(o, "  source revision: unknown ({why})");
        }
    }
    let _ = writeln!(o, "  channel: {}", report.channel);
    let _ = writeln!(
        o,
        "  analysis: bone semantic pass (the front half of `rock compile`); no emission, no lint pass"
    );
    match (&report.population, &report.graph) {
        (Some(pop), Ok(graph)) => {
            let _ = writeln!(
                o,
                "  population: {}",
                match pop.mode {
                    PopMode::Production =>
                        "production -- the `rock compile` set; files under a `tests/` path are excluded",
                    PopMode::Test =>
                        "test -- the root lives under a `tests/` path, which `rock compile` drops entirely, so it is analyzed with the whole-package set `rock test` builds",
                }
            );
            for row in &pop.rows {
                let _ = writeln!(
                    o,
                    "    {:<16} {} file(s) analyzed, {} test-only excluded",
                    graph.package(PackageId(row.package)).manifest_name,
                    row.included,
                    row.excluded_test_only
                );
            }
            let _ = writeln!(o, "    {:<16} {} compiler-owned module(s)", "std", pop.std_modules);
        }
        _ => {
            let _ = writeln!(o, "  population: {unknown}");
        }
    }
    match &report.analysis {
        Analysis::Ok(_) => {
            let _ = writeln!(o, "  analysis result: ok");
        }
        Analysis::Failed { stage, diagnostics, rendered } => {
            let _ = writeln!(o, "  analysis result: FAILED at {stage} ({} diagnostic(s))", diagnostics.len());
            for line in rendered.lines() {
                let _ = writeln!(o, "    {line}");
            }
        }
    }

    // Package graph.
    match &report.graph {
        Err(e) => {
            let _ = writeln!(o, "\npackage graph {}", tag(Status::Unknown));
            let _ = writeln!(o, "  {unknown}: {e}");
        }
        Ok(graph) => {
            let aliases = bone_query::package_aliases::from_graph(graph);
            let _ = writeln!(o, "\npackage graph {}", tag(Status::Exact));
            for node in graph.packages() {
                let root_mark = if node.id == graph.root() { ", selected root" } else { "" };
                let _ = writeln!(
                    o,
                    "  #{} {} ({}{root_mark}) {}",
                    node.id.0,
                    node.manifest_name,
                    dialect_str(node.dialect),
                    display_path(&node.root)
                );
                let resolver = aliases.packages.iter().find(|p| p.id == node.id.0);
                let _ = writeln!(
                    o,
                    "      self name: {}",
                    resolver.map(|p| p.self_name.to_string()).unwrap_or_default()
                );
                if node.dependencies.is_empty() {
                    let _ = writeln!(o, "      dependencies: none");
                }
                for edge in &node.dependencies {
                    let target = graph.package(edge.target);
                    let _ = writeln!(
                        o,
                        "      dependency `{}` -> {} (#{}, {}){}",
                        edge.alias,
                        target.manifest_name,
                        edge.target.0,
                        dialect_str(target.dialect),
                        if edge.allow_legacy { " allow_legacy" } else { "" }
                    );
                }
            }
            let _ = writeln!(o, "  unresolved edges: none; cycles: none (the graph build rejects both)");
        }
    }

    // Legacy boundaries.
    match &report.graph {
        Err(_) => {
            let _ = writeln!(o, "\nreachable legacy boundaries {}\n  {unknown}", tag(Status::Unknown));
        }
        Ok(graph) => {
            let root = graph.package(graph.root());
            let _ = writeln!(o, "\nreachable legacy boundaries from {} {}", root.manifest_name, tag(Status::Exact));
            let _ = writeln!(o, "  std: compiler-owned standard library, the allowed boundary (spec 5); never counted");
            for r in reach(graph) {
                let node = graph.package(PackageId(r.package));
                let role = match (node.dialect, r.package == graph.root().0) {
                    (Dialect::Legacy, true) => "LEGACY (selected root)",
                    (Dialect::Legacy, false) => "LEGACY",
                    (Dialect::V1, true) => "v1 (selected root)",
                    (Dialect::V1, false) => "v1",
                };
                let _ = writeln!(o, "  {} {role}, {} path(s)", node.manifest_name, r.path_count);
                for p in &r.paths {
                    let _ = writeln!(o, "      {}", p.join(" -> "));
                }
                if r.path_count > r.paths.len() {
                    let _ = writeln!(o, "      ... {} more path(s) not listed", r.path_count - r.paths.len());
                }
            }
        }
    }

    // Closure verdict.
    let label = if report.closed_requested { "--closed" } else { "closure (--closed not requested)" };
    match verdict {
        Verdict::Pass => {
            let _ = writeln!(o, "\n{label}: pass -- every reachable package is v1; std is the only boundary");
        }
        Verdict::Fail(ids) => {
            let graph = report.graph.as_ref().expect("a failing verdict has a graph");
            let _ = writeln!(o, "\n{label}: FAIL -- {} legacy package(s) reachable:", ids.len());
            let reaches = reach(graph);
            for id in ids {
                let r = reaches.iter().find(|r| r.package == *id).expect("offending package is reachable");
                let why = if *id == graph.root().0 { "the selected root is legacy" } else { "reachable legacy package" };
                let _ = writeln!(
                    o,
                    "  {} ({why}): {}",
                    graph.package(PackageId(*id)).manifest_name,
                    r.paths.first().map(|p| p.join(" -> ")).unwrap_or_default()
                );
            }
        }
        Verdict::Unknown(why) => {
            let _ = writeln!(o, "\n{label}: unknown ({why})");
        }
    }

    // Cross-package references.
    match (&report.analysis, &report.graph) {
        (Analysis::Ok(table), Ok(graph)) => {
            let _ = writeln!(o, "\ncross-package references by original owner {}", tag(Status::Exact));
            let _ = writeln!(
                o,
                "  counted: calls, value references, constructions (struct literals, newtype constructors), enum variants in lowered bodies"
            );
            if table.edges.is_empty() {
                let _ = writeln!(o, "  none");
            }
            for (&(from, to), c) in &table.edges {
                let _ = writeln!(
                    o,
                    "  {} -> {}: {} (calls {}, values {}, constructions {}, enum variants {})",
                    graph.package(PackageId(from)).manifest_name,
                    owner_name(graph, to),
                    c.total(),
                    c.calls,
                    c.values,
                    c.constructions,
                    c.enum_variants
                );
            }
            for (&from, u) in &table.unattributed {
                let from = &graph.package(PackageId(from)).manifest_name;
                for (label, map) in [
                    ("value reference(s) with no definition (raw global)", &u.no_definition),
                    ("value reference(s) whose key is defined in several packages", &u.several_owners),
                    ("named call(s) with no recorded target", &u.unrecorded_calls),
                ] {
                    if map.is_empty() {
                        continue;
                    }
                    let total: usize = map.values().sum();
                    let keys: Vec<String> = map.iter().take(KEYS_LISTED).map(|(k, n)| format!("{k} x{n}")).collect();
                    let more = if map.len() > KEYS_LISTED { format!(", ... {} more key(s)", map.len() - KEYS_LISTED) } else { String::new() };
                    let _ = writeln!(o, "  {from}: {total} unattributed {label}: {}{more}", keys.join(", "));
                }
            }

            let und = undeclared(graph, table);
            let _ = writeln!(o, "\nreferences to undeclared peers or back to consumers {}", tag(Status::Exact));
            let _ = writeln!(o, "  over the counted reference kinds above");
            if und.is_empty() {
                let _ = writeln!(o, "  none");
            }
            for (from, to, class, n) in und {
                let _ = writeln!(
                    o,
                    "  {} -> {}: {n} ({})",
                    graph.package(PackageId(from)).manifest_name,
                    graph.package(PackageId(to)).manifest_name,
                    class.replace('_', " ")
                );
            }
        }
        _ => {
            let _ = writeln!(o, "\ncross-package references by original owner {}\n  {unknown}", tag(Status::Unknown));
            let _ = writeln!(
                o,
                "\nreferences to undeclared peers or back to consumers {}\n  {unknown}",
                tag(Status::Unknown)
            );
        }
    }

    let _ = writeln!(o, "\nnot computed by this version {}", tag(Status::NotComputed));
    for (item, missing) in NOT_COMPUTED {
        let _ = writeln!(o, "  - {item}: {missing}");
    }
    if analysis_failed {
        let _ = writeln!(o, "\nanalysis failed: derived sections are unknown, not zero");
    }
    o
}

fn render_json(report: &Report, verdict: &Verdict) -> Value {
    let unknown_reason = "analysis failed";

    let revision = match &report.revision {
        Revision::Known { commit, dirty_paths } => json!({
            "status": "exact",
            "commit": commit,
            "dirty_paths": dirty_paths,
        }),
        Revision::Unknown(why) => json!({ "status": "unknown", "reason": why }),
    };
    let population = match (&report.population, &report.graph) {
        (Some(pop), Ok(graph)) => json!({
            "status": "exact",
            "mode": match pop.mode { PopMode::Production => "production", PopMode::Test => "test" },
            "packages": pop.rows.iter().map(|r| json!({
                "package": graph.package(PackageId(r.package)).manifest_name,
                "included_files": r.included,
                "excluded_test_only_files": r.excluded_test_only,
            })).collect::<Vec<_>>(),
            "std_modules": pop.std_modules,
        }),
        _ => json!({ "status": "unknown", "reason": unknown_reason }),
    };
    let analysis = match &report.analysis {
        Analysis::Ok(_) => json!({ "status": "ok", "stage": Value::Null, "diagnostics": [] }),
        Analysis::Failed { stage, diagnostics, .. } => json!({
            "status": "failed",
            "stage": stage,
            "diagnostics": diagnostics.iter().map(|d| json!({
                "file": d.file, "line": d.line, "col": d.col, "message": d.message,
            })).collect::<Vec<_>>(),
        }),
    };

    let (package_graph, legacy_boundaries) = match &report.graph {
        Err(e) => (
            json!({ "status": "unknown", "reason": e }),
            json!({ "status": "unknown", "reason": "package graph failed" }),
        ),
        Ok(graph) => {
            let aliases = bone_query::package_aliases::from_graph(graph);
            let packages: Vec<Value> = graph
                .packages()
                .iter()
                .map(|node| {
                    let resolver = aliases.packages.iter().find(|p| p.id == node.id.0);
                    json!({
                        "id": node.id.0,
                        "package": node.manifest_name,
                        "self_name": resolver.map(|p| p.self_name.to_string()),
                        "dialect": dialect_str(node.dialect),
                        "root": display_path(&node.root),
                        "selected_root": node.id == graph.root(),
                        "dependencies": node.dependencies.iter().map(|e| json!({
                            "declared_key": e.declared_key,
                            "alias": e.alias,
                            "package": graph.package(e.target).manifest_name,
                            "id": e.target.0,
                            "allow_legacy": e.allow_legacy,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let reachable: Vec<Value> = reach(graph)
                .into_iter()
                .map(|r| {
                    let node = graph.package(PackageId(r.package));
                    json!({
                        "package": node.manifest_name,
                        "dialect": dialect_str(node.dialect),
                        "legacy": node.dialect == Dialect::Legacy,
                        "selected_root": r.package == graph.root().0,
                        "path_count": r.path_count,
                        "paths": r.paths,
                    })
                })
                .collect();
            (
                json!({
                    "status": "exact",
                    "packages": packages,
                    "unresolved_edges": [],
                    "cycles": [],
                }),
                json!({
                    "status": "exact",
                    "root": graph.package(graph.root()).manifest_name,
                    "std_boundary": "compiler-owned standard library (spec 5); never counted as legacy",
                    "reachable": reachable,
                }),
            )
        }
    };

    let closed = match verdict {
        Verdict::Pass => json!({ "requested": report.closed_requested, "verdict": "pass", "root_is_legacy": false, "offending": [] }),
        Verdict::Unknown(why) => json!({ "requested": report.closed_requested, "verdict": "unknown", "reason": why, "offending": [] }),
        Verdict::Fail(ids) => {
            let graph = report.graph.as_ref().expect("a failing verdict has a graph");
            let reaches = reach(graph);
            json!({
                "requested": report.closed_requested,
                "verdict": "fail",
                "root_is_legacy": ids.contains(&graph.root().0),
                "offending": ids.iter().map(|id| {
                    let r = reaches.iter().find(|r| r.package == *id).expect("offending package is reachable");
                    json!({
                        "package": graph.package(PackageId(*id)).manifest_name,
                        "selected_root": *id == graph.root().0,
                        "path_count": r.path_count,
                        "paths": r.paths,
                    })
                }).collect::<Vec<_>>(),
            })
        }
    };

    let (refs, undeclared_json) = match (&report.analysis, &report.graph) {
        (Analysis::Ok(table), Ok(graph)) => {
            let edges: Vec<Value> = table
                .edges
                .iter()
                .map(|(&(from, to), c)| json!({
                    "from": graph.package(PackageId(from)).manifest_name,
                    "to": owner_name(graph, to),
                    "calls": c.calls,
                    "values": c.values,
                    "constructions": c.constructions,
                    "enum_variants": c.enum_variants,
                    "total": c.total(),
                }))
                .collect();
            let unattributed: Vec<Value> = table
                .unattributed
                .iter()
                .map(|(&from, u)| json!({
                    "from": graph.package(PackageId(from)).manifest_name,
                    "no_definition": u.no_definition,
                    "several_owners": u.several_owners,
                    "unrecorded_calls": u.unrecorded_calls,
                }))
                .collect();
            let und: Vec<Value> = undeclared(graph, table)
                .into_iter()
                .map(|(from, to, class, n)| json!({
                    "from": graph.package(PackageId(from)).manifest_name,
                    "to": graph.package(PackageId(to)).manifest_name,
                    "class": class,
                    "references": n,
                }))
                .collect();
            (
                json!({
                    "status": "exact",
                    "counted_kinds": ["calls", "values", "constructions", "enum_variants"],
                    "edges": edges,
                    "unattributed": unattributed,
                }),
                json!({ "status": "exact", "over": "the counted_kinds of cross_package_references", "edges": und }),
            )
        }
        _ => (
            json!({ "status": "unknown", "reason": unknown_reason }),
            json!({ "status": "unknown", "reason": unknown_reason }),
        ),
    };

    json!({
        "project": report.project,
        "configuration": {
            "status": "exact",
            "channel": report.channel,
            "revision": revision,
            "analysis": analysis,
            "analysis_kind": "bone semantic pass (front half of rock compile); no emission, no lint pass",
            "population": population,
        },
        "package_graph": package_graph,
        "legacy_boundaries": legacy_boundaries,
        "closed": closed,
        "cross_package_references": refs,
        "undeclared_references": undeclared_json,
        "not_computed": NOT_COMPUTED.iter().map(|(item, missing)| json!({
            "item": item, "status": "not_computed", "missing": missing,
        })).collect::<Vec<_>>(),
    })
}
