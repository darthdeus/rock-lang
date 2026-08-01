//! `rock mono-ty-census [root]` -- Stage 0c evidence (`docs/monomorphization-plan.md`
//! S 0c). DIAGNOSTIC: it changes no emission and nothing in a build calls it.
//!
//! Converts every inferred expr and binding type of the closed world to
//! [`MonoTy`](skull_mir::monoty::MonoTy) and reports what totality costs:
//! how many types convert, how many distinct logical types there are, and every
//! failure bucketed by reason with one source example each.
//!
//! **Closed world** is the same one `rock lint --union` uses -- every `bin`
//! project under the root, each analyzed with its own dependency set (stdlib +
//! stone + its own sources). A per-project view is not a closed world on its
//! own, which is why the union is the unit and a project that fails to
//! type-check is reported LOUDLY rather than skipped quietly: its types are
//! absent from the totals, and that changes what the totals mean.
//!
//! The substitution is deliberately EMPTY. Every declared generic read inside a
//! shared generic body therefore lands in the `unbound-generic` bucket, and that
//! count is the measurement: it is the size of the surface that has no concrete
//! instance today, i.e. what Stage 2 has to supply.
//!
//! Output is deterministic -- projects sorted, buckets sorted, no timings, and
//! paths rendered relative to the root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use ustr::Ustr;
use skull_mir::monoty::{MonoArena, MonoSubst, MonoTyReason};

use crate::Result;

/// One project's contribution.
#[derive(Default)]
struct ProjectStats {
    converted: usize,
    /// Distinct canonical renderings seen in THIS project.
    unique: usize,
    /// bucket -> (count, first example "file:line")
    failures: BTreeMap<&'static str, (usize, Vec<String>)>,
}

pub fn do_monoty_census(root: Option<&str>) -> Result<()> {
    let root = Path::new(root.unwrap_or(".")).canonicalize().map_err(|e| {
        anyhow::anyhow!("mono-ty-census: cannot resolve `{}`: {e}", root.unwrap_or("."))
    })?;
    let projects = crate::lint_union::bin_projects_under(&root);
    if projects.is_empty() {
        anyhow::bail!("mono-ty-census: no `bin` project found under `{}`", root.display());
    }

    let mut per_project: Vec<(String, ProjectStats)> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    // The union arena: one interning table across every project, so "unique
    // MonoTys" is a whole-world count rather than a sum of per-project counts.
    let mut union = MonoArena::new();
    let mut union_ids = std::collections::HashSet::new();
    let mut union_converted = 0usize;
    // Occurrences whose logical type a SpecKey slot cannot be keyed on.
    let mut union_gap_occurrences = 0usize;
    let mut union_failures: BTreeMap<&'static str, (usize, Vec<String>)> = BTreeMap::new();

    for dir in &projects {
        let name = rel(&root, dir);
        match census_project(dir, &mut union, &mut union_ids) {
            Ok((stats, converted, failures, gap)) => {
                union_converted += converted;
                union_gap_occurrences += gap;
                for (bucket, (n, ex)) in failures {
                    let e = union_failures.entry(bucket).or_default();
                    e.0 += n;
                    for x in ex {
                        if e.1.len() < MAX_EXAMPLES && !e.1.contains(&x) {
                            e.1.push(x);
                        }
                    }
                }
                per_project.push((name, stats));
            }
            Err(e) => skipped.push((name, e.to_string())),
        }
    }

    per_project.sort_by(|a, b| a.0.cmp(&b.0));
    skipped.sort();

    println!("# MonoTy conversion census");
    println!();
    println!("projects: {} analyzed, {} skipped", per_project.len(), skipped.len());
    println!();
    println!("| project | types | unique | failures |");
    println!("|---|---:|---:|---:|");
    for (name, s) in &per_project {
        let fails: usize = s.failures.values().map(|(n, _)| n).sum();
        println!("| {name} | {} | {} | {fails} |", s.converted, s.unique);
    }
    let union_fail_total: usize = union_failures.values().map(|(n, _)| n).sum();
    println!(
        "| **union** | **{union_converted}** | **{}** | **{union_fail_total}** |",
        union_ids.len()
    );
    println!();

    // The Stage-1 cost/benefit line: how much of the corpus is a type today's
    // partial tree cannot name, and therefore silently folds onto a shared key.
    let gap_types: Vec<&String> =
        union_ids.iter().filter(|r| render_is_gap(r)).collect();
    println!("## what totality costs");
    println!();
    println!(
        "- occurrences whose type contains a constructor specialization declines \
         (`any`, unit, `never`, `fn`): **{union_gap_occurrences}** of {union_converted} \
         ({:.2}%)",
        100.0 * union_gap_occurrences as f64 / union_converted.max(1) as f64
    );
    println!(
        "- DISTINCT logical types that contain one: **{}** of {} ({:.1}%)",
        gap_types.len(),
        union_ids.len(),
        100.0 * gap_types.len() as f64 / union_ids.len().max(1) as f64
    );
    println!();

    println!("## failures by reason (union)");
    println!();
    if union_failures.is_empty() {
        println!("none -- conversion is total over the closed world.");
    } else {
        println!("| bucket | count | example |");
        println!("|---|---:|---|");
        for (bucket, (n, ex)) in &union_failures {
            let ex = if ex.is_empty() {
                "--".to_string()
            } else {
                ex.iter().map(|e| format!("`{e}`")).collect::<Vec<_>>().join(", ")
            };
            println!("| `{bucket}` | {n} | {ex} |");
        }
    }
    println!();

    // Loud, never a footnote: a project that did not type-check contributes no
    // types, so every total above is missing whatever it alone reaches.
    if !skipped.is_empty() {
        println!("## NOT analyzed");
        println!();
        for (name, why) in &skipped {
            println!("- `{name}`: {}", why.lines().next().unwrap_or("?"));
        }
        println!();
    }
    Ok(())
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// Does this logical type contain a constructor a `SpecKey` cannot be keyed on?
///
/// `reify_type` admits int/float/bool/string, tuple, nullable and application,
/// and answers `None` for `any`, unit, never and function types -- and that
/// `None` becomes a `SpecKey` slot rather than a rejection, so every type below
/// is one that currently collapses distinct instantiations onto one identity.
/// This count is the direct measurement of what totality buys.
fn outside_reified_type(ty: &skull_mir::monoty::MonoTy) -> bool {
    use skull_mir::monoty::MonoTy::*;
    match ty {
        Any | Unit | Never | Function { .. } => true,
        Int | Float | Bool | String => false,
        Nullable(inner) => outside_reified_type(inner),
        Tuple(parts) => parts.iter().any(outside_reified_type),
        App { params, .. } => params.iter().any(outside_reified_type),
    }
}

type ProjectResult =
    (ProjectStats, usize, BTreeMap<&'static str, (usize, Vec<String>)>, usize);

fn census_project(
    dir: &PathBuf,
    union: &mut MonoArena,
    union_ids: &mut std::collections::HashSet<String>,
) -> Result<ProjectResult> {
    let spec = crate::common::path_to_source_spec(&dir.display().to_string());
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let (mut modules, _entry) = crate::skull_backend::skull_assemble_project(&sources)
        .map_err(|errs| anyhow::anyhow!("{} parse/assemble errors", errs.len()))?;
    let ws = crate::bone_backend::check_modules(&mut modules)
        .map_err(|errs| anyhow::anyhow!("{} type errors", errs.len()))?;

    let mut stats = ProjectStats::default();
    let mut seen_here = std::collections::HashSet::new();
    let mut converted = 0usize;
    let mut gap = 0usize;
    let mut failures: BTreeMap<&'static str, (usize, Vec<String>)> = BTreeMap::new();

    // Module order is `ws.modules`, which assembly fixes deterministically.
    for mid in module_ids(&ws) {
        let Some(ir) = ws.inference(mid) else { continue };
        let module = ws.module(mid);
        let subst = MonoSubst::default();
        for (owner, inf) in ir.per_fn.iter() {
            let body = module.bodies.get(owner);
            for (e, ty) in inf.expr_types.iter() {
                match union.convert(&ws, inf, *ty, &subst) {
                    Ok(id) => {
                        converted += 1;
                        if outside_reified_type(union.get(id)) {
                            gap += 1;
                        }
                        let r = union.render(id);
                        seen_here.insert(r.clone());
                        union_ids.insert(r);
                    }
                    Err(reason) => {
                        let ex = body
                            .and_then(|b| b.expr_spans.get(e))
                            .map(|sp| format!("{}:{}", short(sp.file), sp.line_range.0 + 1));
                        note(&mut failures, reason, ex);
                    }
                }
            }
            for (b, ty) in inf.binding_types.iter() {
                match union.convert(&ws, inf, *ty, &subst) {
                    Ok(id) => {
                        converted += 1;
                        if outside_reified_type(union.get(id)) {
                            gap += 1;
                        }
                        let r = union.render(id);
                        seen_here.insert(r.clone());
                        union_ids.insert(r);
                    }
                    Err(reason) => {
                        let ex = body
                            .and_then(|bd| bd.binding_spans.get(b))
                            .map(|sp| format!("{}:{}", short(sp.file), sp.line_range.0 + 1));
                        note(&mut failures, reason, ex);
                    }
                }
            }
        }
    }

    stats.converted = converted;
    stats.unique = seen_here.len();
    stats.failures = failures.clone();
    Ok((stats, converted, failures, gap))
}

/// Enough examples to see a PATTERN in a bucket; one can be an accident.
const MAX_EXAMPLES: usize = 3;

fn note(
    into: &mut BTreeMap<&'static str, (usize, Vec<String>)>,
    reason: MonoTyReason,
    example: Option<String>,
) {
    let e = into.entry(reason.bucket()).or_default();
    e.0 += 1;
    if let Some(x) = example {
        if e.1.len() < MAX_EXAMPLES && !e.1.contains(&x) {
            e.1.push(x);
        }
    }
}

/// A file name without the machine-specific prefix, so output is comparable
/// across checkouts.
fn short(file: Ustr) -> String {
    let s = file.as_str();
    match s.rfind('/') {
        Some(i) => {
            let tail = &s[..i];
            match tail.rfind('/') {
                Some(j) => s[j + 1..].to_string(),
                None => s[i + 1..].to_string(),
            }
        }
        None => s.to_string(),
    }
}

fn module_ids(ws: &Workspace) -> Vec<ModuleId> {
    (0..ws.modules.len() as u32).map(ModuleId::new).collect()
}

/// The gap test over a canonical rendering, for the distinct-type count (the
/// union set is keyed by rendering, not by id). Substring-safe because the
/// renderer emits these as whole tokens.
fn render_is_gap(r: &str) -> bool {
    r.contains("fn(")
        || token(r, "any")
        || token(r, "unit")
        || token(r, "never")
}

fn token(hay: &str, needle: &str) -> bool {
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle).map(|i| i + from) {
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let j = i + needle.len();
        let after_ok = j >= bytes.len() || !bytes[j].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = i + 1;
    }
    false
}
