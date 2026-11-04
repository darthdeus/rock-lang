//! `rock fix --imports <package|file> [--check|--apply]` (spec 15.3; modules
//! plan section 7.4): add the `use` items a modules-v1 fragment's unresolved
//! bare names need.
//!
//! The input is what lowering reports as ``` `x` did not resolve to any
//! definition ``` in a v1 fragment of the package. Candidates are what
//! `bone_query::modules_scope::import_candidates` offers -- the completion
//! import edit's source -- so the command and the editor agree on what a
//! fragment can import, by what path and where the `use` goes
//! (`modules_scope::use_insertion`). One accessible candidate gets an
//! explicit import, or a module glob when the fragment already globs a
//! sibling module; several are listed with their origins; a definition
//! behind an undeclared dependency or a narrower visibility is reported.
//! Nothing adds a dependency, `allow_legacy`, or visibility.
//!
//! The semantic pass stops at the first interface error, and a body at its
//! first unresolved name, so names arrive a few at a time: the planner works
//! in rounds, re-checking the edited text in memory until no new name
//! surfaces. A name with nothing to import keeps hiding the names after it
//! in its body until the author resolves it. Safety is
//! migrate's check (`modules_analysis::sites`): every site the baseline
//! resolved must resolve to the same target in the candidate, and the
//! candidate may have no error the baseline lacked. An explicit import of a
//! name no tier binds cannot rebind another name, so when the baseline does
//! not check at all, only explicit imports are planned, and a glob plan is
//! also compared site by site with the same fixes as explicit imports, which
//! covers the bodies the baseline could not lower.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atomic_refcell::AtomicRefCell;
use bone_core::modules::{FragmentScope, PackageId};
use bone_query::modules_scope::{self, ImportCandidate};
use bone_semantic::inference::Workspace;
use bone_semantic::lower::resolve;
use rock_core::SourceFiles;
use rock_core::errors::CompilerError;
use rock_core::source_code::Dialect;
use serde_json::{Value, json};
use ustr::Ustr;

use crate::modules_analysis::{
    self, Ctx, Edit, PopMode, Sites, load_ctx, map_sites, short_path, sites,
};
use crate::modules_audit::Format;
use crate::modules_migrate::Mode;

const EXIT_NOT_CLEAN: i32 = 3;
const EXIT_ANALYSIS_FAILED: i32 = 4;
/// A round only continues when it planned a new import, and a body reports
/// only its first unresolved name (an interface error hides the whole
/// program), so the rounds follow the longest such chain; the cap only
/// guards the loop.
const MAX_ROUNDS: usize = 64;

pub fn run(target: &Path, mode: Mode, format: Format) -> Result<i32> {
    let outcome = plan(target);
    let applied = match (&outcome, mode) {
        (Outcome::Planned(p), Mode::Apply) if p.refused.is_none() && !p.fixes.is_empty() => {
            Some(apply(p))
        }
        _ => None,
    };
    let code = match (&outcome, &applied) {
        (Outcome::Failed { .. }, _) | (_, Some(Err(_))) => EXIT_ANALYSIS_FAILED,
        (Outcome::NotV1(_), _) => 0,
        (Outcome::Planned(p), _) if p.refused.is_some() || p.remaining() > 0 => EXIT_NOT_CLEAN,
        (Outcome::Planned(p), None) if !p.fixes.is_empty() => EXIT_NOT_CLEAN,
        (Outcome::Planned(_), _) => 0,
    };
    match format {
        Format::Pretty => print!("{}", render_pretty(&outcome, mode, applied.as_ref())),
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&render_json(&outcome, mode, applied.as_ref()))?
        ),
    }
    std::io::stdout().flush()?;
    Ok(code)
}

enum Outcome {
    NotV1(String),
    Failed { stage: String, message: String },
    Planned(Box<Plan>),
}

/// One v1 fragment of the package the fix may edit.
struct FixFile {
    id: Ustr,
    path: PathBuf,
    text: String,
    contribution: bone_semantic::semantic_passes::FragmentContribution,
}

/// Where an unresolved name was first reported, in baseline coordinates.
#[derive(Clone)]
struct Site {
    file: usize,
    line: usize,
    name: Ustr,
}

struct Fix {
    site: Site,
    /// The path after `use`, without the `;`.
    path: String,
    glob: bool,
    /// The explicit import of the definition, which `path` equals unless
    /// `glob`.
    explicit: String,
    /// The definition the import names.
    origin: String,
}

struct Listed {
    site: Site,
    entries: Vec<String>,
}

struct Plan {
    package: String,
    root: PathBuf,
    files: Vec<FixFile>,
    fixes: Vec<Fix>,
    ambiguous: Vec<Listed>,
    unavailable: Vec<Listed>,
    /// Unresolved names with no candidate anywhere in the unit.
    unresolved: Vec<Site>,
    /// How the candidate was verified.
    verification: String,
    refused: Option<Vec<String>>,
}

impl Plan {
    fn remaining(&self) -> usize {
        self.ambiguous.len() + self.unavailable.len() + self.unresolved.len()
    }

    /// Each file's inserted text, in baseline coordinates.
    fn edits(&self) -> BTreeMap<usize, Vec<Edit>> {
        edits_for(&self.files, &self.fixes)
    }
}

fn edits_for(files: &[FixFile], fixes: &[Fix]) -> BTreeMap<usize, Vec<Edit>> {
    let mut lines: BTreeMap<usize, BTreeSet<&str>> = BTreeMap::new();
    for f in fixes {
        lines.entry(f.site.file).or_default().insert(&f.path);
    }
    lines
        .into_iter()
        .map(|(fi, paths)| {
            let file = &files[fi];
            let mut edits: Vec<Edit> = paths
                .into_iter()
                .map(|path| {
                    let (at, text) =
                        modules_scope::use_insertion(&file.contribution, &file.text, path);
                    Edit::new((at, at), text)
                })
                .collect();
            edits.sort_by(|a, b| a.range.cmp(&b.range).then(a.text.cmp(&b.text)));
            (fi, edits)
        })
        .collect()
}

fn plan(target: &Path) -> Outcome {
    let failed = |stage: &str, message: String| Outcome::Failed {
        stage: stage.to_string(),
        message,
    };
    let target = match target.canonicalize() {
        Ok(p) => p,
        Err(e) => return failed("target", format!("`{}`: {e}", target.display())),
    };
    let (root, only) = if target.is_file() {
        let Some(root) = target
            .ancestors()
            .skip(1)
            .find(|d| d.join("stick.toml").is_file())
        else {
            return failed(
                "target",
                format!("no stick.toml above `{}`", target.display()),
            );
        };
        (root.to_path_buf(), Some(target.clone()))
    } else {
        (target.clone(), None)
    };
    let ctx = match load_ctx(&root, &root, PopMode::Test) {
        Ok(c) => c,
        Err(e) => return failed("package graph", e),
    };
    let package = ctx.name().to_string();
    let mut files = Vec::new();
    for f in &ctx.sources.files {
        let f = f.borrow();
        let id = Ustr::from(f.file.to_id_path().as_str());
        if f.dialect != Dialect::V1 || ctx.file_pkg.get(&id) != Some(&ctx.p_id) {
            continue;
        }
        let path = PathBuf::from(id.as_str());
        if only.as_ref().is_some_and(|o| o != &path) {
            continue;
        }
        let ast = match skull_parser::parse_module_with_dialect(&f.contents, id, Dialect::V1) {
            Ok(ast) => ast,
            Err(e) => return failed("parse", format!("{}: {}", short_path(&id), e.message)),
        };
        files.push(FixFile {
            id,
            path,
            text: f.contents.clone(),
            contribution: bone_semantic::semantic_passes::contribute(&ast),
        });
    }
    if files.is_empty() {
        return Outcome::NotV1(package);
    }
    files.sort_by_key(|f| f.id);

    let baseline = match Round::check(&ctx, &files, &BTreeMap::new()) {
        Ok(r) => r,
        Err((stage, message)) => return failed(stage, message),
    };
    let mut plan = match plan_rounds(&ctx, &files, &baseline, baseline.resolved.is_some()) {
        Ok(p) => p,
        Err((stage, message)) => return failed(stage, message),
    };
    // A glob that moves a site falls back to explicit imports.
    if plan.refused.is_some() && plan.fixes.iter().any(|f| f.glob) {
        plan = match plan_rounds(&ctx, &files, &baseline, false) {
            Ok(p) => p,
            Err((stage, message)) => return failed(stage, message),
        };
    }
    plan.package = package;
    plan.root = root;
    plan.files = files;
    Outcome::Planned(Box::new(plan))
}

/// One in-memory check of the package with some imports added.
struct Round {
    sources: SourceFiles,
    /// The workspace and references when the semantic pass completed.
    resolved: Option<(Workspace, Vec<modules_analysis::Ref>)>,
    /// Every error, siblings flattened.
    errors: Vec<CompilerError>,
    /// Where the semantic pass stopped, when it did.
    stopped: Option<String>,
}

impl Round {
    fn check(
        ctx: &Ctx,
        files: &[FixFile],
        edits: &BTreeMap<usize, Vec<Edit>>,
    ) -> Result<Round, (&'static str, String)> {
        let mut texts: HashMap<Ustr, String> = HashMap::new();
        for (fi, e) in edits {
            let text =
                modules_analysis::apply_edits(&files[*fi].text, e).map_err(|m| ("edit", m))?;
            texts.insert(files[*fi].id, text);
        }
        let sources = SourceFiles {
            files: ctx
                .sources
                .files
                .iter()
                .map(|f| {
                    let mut f = f.borrow().clone();
                    if let Some(text) = texts.get(&Ustr::from(f.file.to_id_path().as_str())) {
                        f.contents = text.clone();
                    }
                    Arc::new(AtomicRefCell::new(f))
                })
                .collect(),
            package_roots: ctx.sources.package_roots.clone(),
        };
        match modules_analysis::check(&sources) {
            Ok(checked) => {
                let refs = modules_analysis::references(&checked.ws);
                let errors = flatten(checked.errors);
                Ok(Round {
                    sources,
                    resolved: Some((checked.ws, refs)),
                    errors,
                    stopped: None,
                })
            }
            Err(f) if f.stage == "parse" => Err((
                "parse",
                crate::skull_backend::render_project_errs(&sources, &f.errors),
            )),
            Err(f) => Ok(Round {
                sources,
                resolved: None,
                errors: flatten(f.errors),
                stopped: Some(f.stage.to_string()),
            }),
        }
    }

    /// The workspace to look candidates up in: the checked one, or the
    /// namespace tree alone when the semantic pass stopped.
    fn namespace(&self) -> Result<std::borrow::Cow<'_, Workspace>, (&'static str, String)> {
        if let Some((ws, _)) = &self.resolved {
            return Ok(std::borrow::Cow::Borrowed(ws));
        }
        let (modules, _) =
            crate::skull_backend::skull_assemble_project(&self.sources).map_err(|e| {
                (
                    "parse",
                    crate::skull_backend::render_project_errs(&self.sources, &e),
                )
            })?;
        let aliases = crate::bone_backend::aliases_for_modules(&modules)
            .map_err(|e| ("package graph", e.message))?;
        let mut ws = Workspace::default();
        bone_semantic::semantic_passes::build_namespaces(&mut ws, &modules, &aliases)
            .map_err(|e| ("namespace tree", e.message))?;
        Ok(std::borrow::Cow::Owned(ws))
    }
}

fn flatten(errors: Vec<CompilerError>) -> Vec<CompilerError> {
    let mut out = Vec::new();
    for mut e in errors {
        let siblings = std::mem::take(&mut e.siblings);
        out.push(e);
        out.extend(siblings);
    }
    out
}

/// The bare name an error reports as unresolved.
fn unresolved_name(e: &CompilerError) -> Option<Ustr> {
    let name = e
        .message
        .strip_prefix('`')?
        .strip_suffix("` did not resolve to any definition")?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| Ustr::from(name))
}

/// A baseline offset for an offset in text that `edits` (insertions only)
/// produced.
fn unmap(edits: &[Edit], off: usize) -> usize {
    let mut shift = 0;
    for e in edits {
        if e.range.0 + shift < off {
            shift += e.text.len();
        }
    }
    off.saturating_sub(shift)
}

fn line_of(text: &str, off: usize) -> usize {
    text[..off.min(text.len())].matches('\n').count() + 1
}

fn plan_rounds(
    ctx: &Ctx,
    files: &[FixFile],
    baseline: &Round,
    allow_globs: bool,
) -> Result<Plan, (&'static str, String)> {
    let by_id: HashMap<Ustr, usize> = files.iter().enumerate().map(|(i, f)| (f.id, i)).collect();
    let mut plan = Plan {
        package: String::new(),
        root: PathBuf::new(),
        files: Vec::new(),
        fixes: Vec::new(),
        ambiguous: Vec::new(),
        unavailable: Vec::new(),
        unresolved: Vec::new(),
        verification: String::new(),
        refused: None,
    };
    let mut handled: BTreeSet<(usize, Ustr)> = BTreeSet::new();
    // The latest round checked with every import planned so far; the first
    // is the baseline itself.
    let mut owned: Option<Round> = None;
    let mut checked_fixes = 0;
    for _ in 0..MAX_ROUNDS {
        let edits = edits_for(files, &plan.fixes);
        if !plan.fixes.is_empty() {
            owned = Some(Round::check(ctx, files, &edits)?);
            checked_fixes = plan.fixes.len();
        }
        let round = owned.as_ref().unwrap_or(baseline);
        let mut found: Vec<Site> = Vec::new();
        for e in &round.errors {
            let (Some(name), Some(&fi)) = (unresolved_name(e), by_id.get(&e.location.file)) else {
                continue;
            };
            if !handled.insert((fi, name)) {
                continue;
            }
            let off = unmap(
                edits.get(&fi).map(Vec::as_slice).unwrap_or(&[]),
                e.location.offset,
            );
            found.push(Site {
                file: fi,
                line: line_of(&files[fi].text, off),
                name,
            });
        }
        if found.is_empty() {
            break;
        }
        let ws = round.namespace()?;
        let before = plan.fixes.len();
        for site in found {
            let Some(frag) = ws.fragment_scopes.get(&files[site.file].id) else {
                continue;
            };
            classify(&ws, frag, site, allow_globs, &mut plan);
        }
        if plan.fixes.len() == before {
            break;
        }
    }
    plan.fixes.sort_by(|a, b| {
        (a.site.file, a.site.line, a.site.name.as_str()).cmp(&(
            b.site.file,
            b.site.line,
            b.site.name.as_str(),
        ))
    });
    let edits = edits_for(files, &plan.fixes);
    if plan.fixes.is_empty() {
        plan.verification = "nothing to verify: no import is planned".to_string();
        return Ok(plan);
    }
    let last = match owned {
        Some(r) if checked_fixes == plan.fixes.len() => r,
        _ => Round::check(ctx, files, &edits)?,
    };
    verify(ctx, files, baseline, &last, &edits, &mut plan);
    Ok(plan)
}

/// Decide what one unresolved name gets.
fn classify(ws: &Workspace, frag: &FragmentScope, site: Site, allow_globs: bool, plan: &mut Plan) {
    let mut seen = BTreeSet::new();
    let candidates: Vec<ImportCandidate> =
        modules_scope::import_candidates(ws, frag, |n| n == site.name.as_str())
            .into_iter()
            .filter(|c| seen.insert((c.key, c.package)))
            .collect();
    match candidates.as_slice() {
        [one] => {
            let glob = allow_globs && globs_a_sibling(ws, frag, one);
            let path = if glob {
                let prefix = if one.from_root { "::" } else { "" };
                let module = one
                    .module_path
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                format!("{prefix}{module}::*")
            } else {
                one.path.clone()
            };
            plan.fixes.push(Fix {
                site,
                path,
                glob,
                explicit: one.path.clone(),
                origin: origin(ws, one),
            });
        }
        [] => {
            let reasons = unavailable(ws, frag, site.name);
            if reasons.is_empty() {
                plan.unresolved.push(site);
            } else {
                plan.unavailable.push(Listed {
                    site,
                    entries: reasons,
                });
            }
        }
        many => plan.ambiguous.push(Listed {
            site,
            entries: many.iter().map(|c| c.path.clone()).collect(),
        }),
    }
}

/// How a report names a candidate's definition.
fn origin(ws: &Workspace, c: &ImportCandidate) -> String {
    if ws.defs.contains_key(&c.key) {
        return c.key.to_string();
    }
    let package = ws
        .ns
        .package(c.package)
        .map_or_else(|| c.package.to_string(), |p| p.self_name.to_string());
    format!("{package}::{} (legacy)", c.key)
}

/// Whether `frag` already globs a sibling of the module holding `c`: the
/// same package, the same parent, not the package root (plan 7.4).
fn globs_a_sibling(ws: &Workspace, frag: &FragmentScope, c: &ImportCandidate) -> bool {
    let Some(meta) = ws.defs.get(&c.key) else {
        return false;
    };
    let Some((_, parent)) = meta.module.split_last() else {
        return false;
    };
    frag.globs.iter().any(|glob| {
        matches!(resolve::resolve_path_to_module(ws, frag, glob), Ok((pkg, path))
            if pkg == meta.package && path.len() == meta.module.len() && path.starts_with(parent) && path != meta.module)
    })
}

/// Why each definition named `name` that `frag` cannot import is out of
/// reach: a narrower visibility, a package that is not a declared
/// dependency, or a legacy package with no `allow_legacy` view.
fn unavailable(ws: &Workspace, frag: &FragmentScope, name: Ustr) -> Vec<String> {
    let aliases: HashMap<PackageId, Ustr> = ws
        .ns
        .package(frag.package)
        .map(|p| p.aliases.iter().map(|(a, t)| (*t, *a)).collect())
        .unwrap_or_default();
    let own = ws
        .ns
        .package(frag.package)
        .map_or_else(String::new, |p| p.self_name.to_string());
    let mut out = Vec::new();
    for (key, meta) in &ws.defs {
        if meta.name != name {
            continue;
        }
        if meta.package != frag.package && !aliases.contains_key(&meta.package) {
            let pkg = ws
                .ns
                .package(meta.package)
                .map_or_else(|| meta.package.to_string(), |p| p.self_name.to_string());
            out.push(format!(
                "`{key}` is in `{pkg}`, which is not a declared dependency of `{own}`"
            ));
            continue;
        }
        let path = bone_core::modules::UsePathRef {
            from_root: false,
            segments: key.as_str().split("::").map(Ustr::from).collect(),
        };
        let reason = match resolve::resolve_path(ws, frag, &path) {
            Err(message) => message,
            Ok(_) => "is accessible but not offered as an import".to_string(),
        };
        out.push(format!("`{key}`: {reason}"));
    }
    for file in ws.ns.legacy_owned.files_defining(name) {
        if bone_core::modules::is_stdlib_module_file(file.as_str()) {
            continue;
        }
        let viewed = aliases.keys().any(|p| {
            ws.ns
                .package(*p)
                .is_some_and(|pkg| pkg.legacy_files.contains(&file))
        });
        if !viewed {
            out.push(format!(
                "a legacy definition in {}: its package is not an `allow_legacy` dependency of `{own}`",
                short_path(&file)
            ));
        }
    }
    out.sort();
    out
}

/// Compare the candidate with the baseline (plan 7.4): every baseline site
/// resolves as before, and no error is new.
fn verify(
    ctx: &Ctx,
    files: &[FixFile],
    baseline: &Round,
    last: &Round,
    edits: &BTreeMap<usize, Vec<Edit>>,
    plan: &mut Plan,
) {
    let fixed: BTreeSet<(Ustr, Ustr)> = plan
        .fixes
        .iter()
        .map(|f| (files[f.site.file].id, f.site.name))
        .collect();
    let mut problems = Vec::new();
    // Errors by (file, message); the fixed names' reports are gone.
    let key = |e: &CompilerError| (e.location.file, e.message.clone());
    let mut before: BTreeMap<(Ustr, String), usize> = BTreeMap::new();
    for e in &baseline.errors {
        *before.entry(key(e)).or_default() += 1;
    }
    for e in &last.errors {
        if unresolved_name(e).is_some_and(|n| fixed.contains(&(e.location.file, n))) {
            problems.push(format!(
                "`{}` still does not resolve in {}",
                unresolved_name(e).unwrap(),
                short_path(&e.location.file)
            ));
            continue;
        }
        match before.get_mut(&key(e)) {
            Some(n) if *n > 0 => *n -= 1,
            _ if unresolved_name(e).is_some() => {}
            _ => problems.push(format!(
                "new error in {}: {}",
                short_path(&e.location.file),
                e.message
            )),
        }
    }
    match (&baseline.resolved, &last.resolved) {
        (Some((bws, brefs)), Some((cws, crefs))) => {
            let file_pkg: HashMap<Ustr, u32> = files.iter().map(|f| (f.id, ctx.p_id)).collect();
            let by_id: HashMap<Ustr, Vec<Edit>> = edits
                .iter()
                .map(|(fi, e)| (files[*fi].id, e.clone()))
                .collect();
            let base: Sites = map_sites(&sites(bws, brefs, &file_pkg, &HashMap::new()), &by_id);
            let cand: Sites = sites(cws, crefs, &file_pkg, &HashMap::new());
            let mut changed = 0;
            for (k, targets) in &base {
                if cand.get(k) != Some(targets) {
                    changed += 1;
                    problems.push(modules_analysis::describe_change(
                        k,
                        targets,
                        cand.get(k).map(Vec::as_slice).unwrap_or(&[]),
                    ));
                }
            }
            plan.verification = format!(
                "{} resolved site(s) of the fixed files compared with the baseline, {changed} changed; {} error(s) remain",
                base.len(),
                last.errors.len()
            );
            problems.extend(glob_moves(ctx, files, edits, &file_pkg, (cws, crefs), plan));
        }
        (None, _) => {
            plan.verification = format!(
                "the baseline stops in its {} with an unresolved name, so no resolved site can move; explicit imports only; {} error(s) remain",
                baseline.stopped.as_deref().unwrap_or("check"),
                last.errors.len()
            );
        }
        (Some(_), None) => problems.push(format!(
            "the candidate stops in its {}, which the baseline passed",
            last.stopped.as_deref().unwrap_or("check")
        )),
    }
    if !problems.is_empty() {
        plan.refused = Some(problems);
    }
}

/// A body that names an unresolved name has no baseline sites, yet a glob
/// can rebind another call there by winning its exact tie by tier (spec
/// 8.3). The same fixes as explicit imports bind only the names they fix,
/// each of which no tier bound, so that candidate is the reference for every
/// site of a glob plan.
fn glob_moves(
    ctx: &Ctx,
    files: &[FixFile],
    edits: &BTreeMap<usize, Vec<Edit>>,
    file_pkg: &HashMap<Ustr, u32>,
    (cws, crefs): (&Workspace, &[modules_analysis::Ref]),
    plan: &Plan,
) -> Vec<String> {
    if !plan.fixes.iter().any(|f| f.glob) {
        return Vec::new();
    }
    let explicit_fixes: Vec<Fix> = plan
        .fixes
        .iter()
        .map(|f| Fix {
            site: f.site.clone(),
            path: f.explicit.clone(),
            glob: false,
            explicit: f.explicit.clone(),
            origin: f.origin.clone(),
        })
        .collect();
    let explicit_edits = edits_for(files, &explicit_fixes);
    let reference = match Round::check(ctx, files, &explicit_edits) {
        Ok(r) => r,
        Err((stage, m)) => {
            return vec![format!(
                "the explicit-import check failed in its {stage}: {m}"
            )];
        }
    };
    let Some((rws, rrefs)) = &reference.resolved else {
        return vec![format!(
            "the explicit-import check stops in its {}, so the globs cannot be verified",
            reference.stopped.as_deref().unwrap_or("check")
        )];
    };
    let to_base = |sites: Sites, edits: &BTreeMap<usize, Vec<Edit>>| -> Sites {
        let by_id: HashMap<Ustr, &[Edit]> = edits
            .iter()
            .map(|(fi, e)| (files[*fi].id, e.as_slice()))
            .collect();
        sites
            .into_iter()
            .map(|(mut k, v)| {
                if let (Some(e), false) = (by_id.get(&k.file), k.kind.per_file()) {
                    k.start = unmap(e, k.start);
                    k.end = unmap(e, k.end);
                }
                (k, v)
            })
            .collect()
    };
    let with_globs = to_base(sites(cws, crefs, file_pkg, &HashMap::new()), edits);
    let explicit = to_base(
        sites(rws, rrefs, file_pkg, &HashMap::new()),
        &explicit_edits,
    );
    explicit
        .iter()
        .filter(|(k, targets)| with_globs.get(*k) != Some(*targets))
        .map(|(k, targets)| {
            modules_analysis::describe_change(
                k,
                targets,
                with_globs.get(k).map(Vec::as_slice).unwrap_or(&[]),
            )
        })
        .collect()
}

/// Write the planned imports, refusing a file that changed since it was read,
/// and re-check the package from disk: `written` names the files, `errors`
/// what the check still reports.
struct Applied {
    written: Vec<String>,
    errors: Vec<String>,
}

fn apply(plan: &Plan) -> std::result::Result<Applied, String> {
    let mut writes = Vec::new();
    for (fi, edits) in plan.edits() {
        let f = &plan.files[fi];
        let on_disk =
            std::fs::read_to_string(&f.path).map_err(|e| format!("{}: {e}", f.path.display()))?;
        if on_disk != f.text {
            return Err(format!(
                "{} changed since it was read; nothing written",
                f.path.display()
            ));
        }
        writes.push((
            f.path.clone(),
            modules_analysis::apply_edits(&f.text, &edits)?,
        ));
    }
    let mut written = Vec::new();
    for (path, text) in writes {
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        written.push(short_path(&path.to_string_lossy()));
    }
    let ctx = load_ctx(&plan.root, &plan.root, PopMode::Test)?;
    let round = Round::check(&ctx, &[], &BTreeMap::new())
        .map_err(|(stage, m)| format!("re-check ({stage}): {m}"))?;
    let errors = round
        .errors
        .iter()
        .map(|e| crate::skull_backend::render_project_errs(&round.sources, std::slice::from_ref(e)))
        .collect();
    Ok(Applied { written, errors })
}

// --- output ----------------------------------------------------------------------

fn site_json(files: &[FixFile], s: &Site) -> Value {
    json!({ "file": short_path(&files[s.file].id), "line": s.line, "name": s.name.as_str() })
}

fn render_json(
    outcome: &Outcome,
    mode: Mode,
    applied: Option<&std::result::Result<Applied, String>>,
) -> Value {
    let mode_s = if mode == Mode::Apply {
        "apply"
    } else {
        "check"
    };
    let p = match outcome {
        Outcome::NotV1(package) => {
            return json!({ "package": package, "mode": mode_s, "verdict": "not_v1" });
        }
        Outcome::Failed { stage, message } => {
            return json!({ "mode": mode_s, "verdict": "analysis_failed", "status": "unknown", "stage": stage, "message": message });
        }
        Outcome::Planned(p) => p,
    };
    let verdict = verdict(p, applied);
    let listed = |v: &[Listed], key: &str| -> Vec<Value> {
        v.iter()
            .map(|l| {
                let mut o = site_json(&p.files, &l.site);
                o[key] = json!(l.entries);
                o
            })
            .collect()
    };
    let mut out = json!({
        "package": p.package,
        "mode": mode_s,
        "verdict": verdict,
        "fixes": p.fixes.iter().map(|f| {
            let mut o = site_json(&p.files, &f.site);
            o["use"] = json!(f.path);
            o["glob"] = json!(f.glob);
            o["origin"] = json!(f.origin);
            o
        }).collect::<Vec<_>>(),
        "ambiguous": listed(&p.ambiguous, "candidates"),
        "unavailable": listed(&p.unavailable, "reasons"),
        "unresolved": p.unresolved.iter().map(|s| site_json(&p.files, s)).collect::<Vec<_>>(),
        "verification": p.verification,
        "refused": p.refused,
    });
    match applied {
        Some(Ok(a)) => {
            out["written"] = json!(a.written);
            out["rechecked_errors"] = json!(a.errors);
        }
        Some(Err(e)) => out["apply_error"] = json!(e),
        None => {}
    }
    out
}

fn verdict(p: &Plan, applied: Option<&std::result::Result<Applied, String>>) -> &'static str {
    match applied {
        Some(Ok(_)) => "applied",
        Some(Err(_)) => "apply_failed",
        None if p.refused.is_some() => "refused",
        None if !p.fixes.is_empty() => "fixes_available",
        None if p.remaining() > 0 => "unresolved",
        None => "clean",
    }
}

fn render_pretty(
    outcome: &Outcome,
    mode: Mode,
    applied: Option<&std::result::Result<Applied, String>>,
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let p = match outcome {
        Outcome::NotV1(package) => {
            return format!(
                "rock fix --imports: {package} has no modules-v1 fragment; nothing to do\n"
            );
        }
        Outcome::Failed { stage, message } => {
            return format!("rock fix --imports: analysis failed ({stage})\n{message}\n");
        }
        Outcome::Planned(p) => p,
    };
    let at = |site: &Site| {
        format!(
            "{}:{} `{}`",
            short_path(&p.files[site.file].id),
            site.line,
            site.name
        )
    };
    let _ = writeln!(s, "rock fix --imports: {}\n", p.package);
    let _ = writeln!(s, "imports [exact]: {} name(s)", p.fixes.len());
    for f in &p.fixes {
        let _ = writeln!(s, "  {} -> use {};  ({})", at(&f.site), f.path, f.origin);
    }
    if !p.ambiguous.is_empty() {
        let _ = writeln!(
            s,
            "\nambiguous [exact]: several accessible candidates; qualify the use or import one (no edit)"
        );
        for l in &p.ambiguous {
            let _ = writeln!(s, "  {}: {}", at(&l.site), l.entries.join(", "));
        }
    }
    if !p.unavailable.is_empty() {
        let _ = writeln!(
            s,
            "\nunavailable [exact]: never added (a dependency, `allow_legacy` or visibility is the author's)"
        );
        for l in &p.unavailable {
            let _ = writeln!(s, "  {}:", at(&l.site));
            for r in &l.entries {
                let _ = writeln!(s, "    {r}");
            }
        }
    }
    if !p.unresolved.is_empty() {
        let _ = writeln!(
            s,
            "\nno candidate [exact]: nothing in the unit defines these names"
        );
        for site in &p.unresolved {
            let _ = writeln!(s, "  {}", at(site));
        }
    }
    let _ = writeln!(
        s,
        "\nverification (the candidate, checked in memory) [exact]\n  {}",
        p.verification
    );
    if let Some(problems) = &p.refused {
        let _ = writeln!(
            s,
            "\nrefused: the imports would change what the package resolves; nothing written"
        );
        for problem in problems {
            let _ = writeln!(s, "  {problem}");
        }
    }
    match applied {
        Some(Ok(a)) => {
            let _ = writeln!(
                s,
                "\napplied: {}; re-checked from disk: {} error(s)",
                a.written.join(", "),
                a.errors.len()
            );
            for e in &a.errors {
                let _ = writeln!(s, "{e}");
            }
        }
        Some(Err(e)) => {
            let _ = writeln!(s, "\napply failed: {e}");
        }
        None if mode == Mode::Check && !p.fixes.is_empty() && p.refused.is_none() => {
            let _ = writeln!(s, "\nimports needed: run with --apply to write them");
        }
        None => {}
    }
    s
}
