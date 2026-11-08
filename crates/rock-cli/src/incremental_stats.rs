//! `rock incremental-stats <project>` -- time the hot-reload compile path.
//!
//! `lsp-survey` times the language server's in-memory update; this command
//! times what `rock stone` does after a save: feed the edit into the warm
//! `IncrementalProject` and publish either `incremental.lua` (a body edit) or a
//! full `main.lua` (a signature edit). Candidates come from the same project
//! statistics as `lsp-survey`: the largest bodies take body edits, the most
//! called and most depended-on functions take signature edits. Each candidate
//! alternates the edited and the original text, so every sample is a real
//! change and the project ends in its original state.

use crate::lsp_survey::{EditKind, FnStat, collect_stats, make_edit, read_survey_source};
use crate::skull_backend::{IncrementalProject, SkullReload};
use anyhow::Result;
use bone_core::prelude::Instant;
use serde_json::json;
use std::path::Path;

struct EditResult {
    file: String,
    name: String,
    line: u32,
    kind: EditKind,
    reload: &'static str,
    error: Option<String>,
    reinferred: usize,
    bytes: usize,
    samples: Vec<f64>,
    cook: Vec<f64>,
    emit: Vec<f64>,
    write: Vec<f64>,
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn max(values: &[f64]) -> f64 {
    values.iter().copied().fold(0.0_f64, f64::max)
}

fn reload_label(reload: &SkullReload) -> &'static str {
    match reload {
        SkullReload::Full { .. } => "full",
        SkullReload::Incremental { .. } => "incremental",
        SkullReload::FullRequired { .. } => "full-required",
        SkullReload::ShapeChanged { .. } => "shape-changed",
        SkullReload::NoChange => "no-change",
    }
}

/// SIGNATURE edit for a compiling project: append a trailing defaulted
/// parameter. The arity change is an interface edit, and every call site still
/// type-checks, so the full rebuild it triggers can be timed. (`lsp-survey`
/// inserts a required parameter instead, which the language server tolerates
/// but a hot reload rejects.)
fn make_signature_edit(orig: &str, f: &FnStat) -> Option<String> {
    let open = orig[f.name_off..f.span_end].find('(')? + f.name_off;
    let mut depth = 0usize;
    let mut close = None;
    for (i, c) in orig[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let insert = if orig[open + 1..close].trim().is_empty() {
        "__lspx: int = 0"
    } else {
        ", __lspx: int = 0"
    };
    let mut out = String::with_capacity(orig.len() + insert.len());
    out.push_str(&orig[..close]);
    out.push_str(insert);
    out.push_str(&orig[close..]);
    Some(out)
}

fn edit_text(orig: &str, f: &FnStat, kind: EditKind) -> Option<String> {
    match kind {
        EditKind::Signature => make_signature_edit(orig, f),
        EditKind::Body | EditKind::Broken => make_edit(orig, f, kind),
    }
}

/// Body edits on the largest bodies, signature edits on the most called and
/// the most depended-on functions, each function at most once per kind.
fn candidates(fns: &[FnStat]) -> Vec<(FnStat, EditKind)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut add = |f: &FnStat, kind: EditKind| {
        if seen.insert((f.file.clone(), f.name.clone(), kind as u8)) {
            out.push((f.clone(), kind));
        }
    };
    let mut by_body: Vec<&FnStat> = fns.iter().collect();
    by_body.sort_by_key(|f| std::cmp::Reverse(f.body_bytes));
    for f in by_body.iter().take(3) {
        add(f, EditKind::Body);
    }
    let mut by_calls: Vec<&FnStat> = fns.iter().collect();
    by_calls.sort_by_key(|f| std::cmp::Reverse(f.call_fan_in));
    for f in by_calls.iter().take(2) {
        add(f, EditKind::Signature);
    }
    let mut by_mods: Vec<&FnStat> = fns.iter().filter(|f| f.mod_fan_in > 0).collect();
    by_mods.sort_by_key(|f| std::cmp::Reverse(f.mod_fan_in));
    for f in by_mods.iter().take(2) {
        add(f, EditKind::Signature);
    }
    out
}

pub fn incremental_stats(path: &str, iters: usize, warmup: usize, json_out: bool) -> Result<()> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        anyhow::bail!("incremental-stats expects a project directory, got `{path}`");
    }
    let dir_str = fs_err::canonicalize(dir)?.to_string_lossy().into_owned();
    let proj_marker = format!(
        "{}/",
        Path::new(&dir_str)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );

    // Project statistics from a throwaway Session: the IncrementalProject
    // keeps its own Session private, and the stats need the dependents map.
    let spec = crate::unified_compile::SourceSpec::Directory(dir.to_path_buf());
    let sources = crate::unified_compile::discover_sources(&spec)?;
    let session = crate::bone_backend::bone_session(crate::bone_backend::session_files(&sources))
        .map_err(|e| anyhow::anyhow!("bone session warm failed: {}", e.message))?;
    let (_mods, fns) = collect_stats(&session, &proj_marker)?;
    drop(session);
    if fns.is_empty() {
        anyhow::bail!("no editable project functions found under `{path}`");
    }

    let out_dir = tempfile::tempdir()?;
    let main_path = out_dir.path().join("main.lua");
    let incremental_path = out_dir.path().join("incremental.lua");
    let t = Instant::now();
    let mut project = IncrementalProject::init(&dir_str, &main_path, &incremental_path)?;
    let init_ms = t.elapsed().as_secs_f64() * 1000.0;
    let main_bytes = std::fs::metadata(&main_path)?.len();

    let iters = iters.max(1);
    let mut results = Vec::new();
    for (f, kind) in candidates(&fns) {
        let orig = read_survey_source(&f.file)?;
        let Some(edited) = edit_text(&orig, &f, kind) else {
            continue;
        };
        let source = Path::new(&f.file);
        let mut result = EditResult {
            file: f.file.clone(),
            name: f.name.clone(),
            line: f.line,
            kind,
            reload: "no-change",
            error: None,
            reinferred: 0,
            bytes: 0,
            samples: Vec::new(),
            cook: Vec::new(),
            emit: Vec::new(),
            write: Vec::new(),
        };
        // The loop may end on the edited text or on a rolled-back error; the
        // restoring edit below returns to the baseline for the next candidate.
        for i in 0..(warmup + iters) {
            let text = if i % 2 == 0 { &edited } else { &orig };
            let t = Instant::now();
            let reload = project.apply_edit(source, text, &main_path, &incremental_path);
            let wall_ms = t.elapsed().as_secs_f64() * 1000.0;
            // A rejected edit rolls the project back to the previous text; the
            // candidate is reported as an error rather than ending the survey,
            // so one unlucky target cannot hide every other measurement.
            let reload = match reload {
                Ok(reload) => reload,
                Err(error) => {
                    result.reload = "error";
                    result.error = Some(error.to_string());
                    result.samples.clear();
                    break;
                }
            };
            if i < warmup {
                continue;
            }
            result.reload = reload_label(&reload);
            result.samples.push(wall_ms);
            match reload {
                SkullReload::Full {
                    bytes,
                    cook_ms,
                    emit_ms,
                    write_ms,
                } => {
                    result.bytes = bytes;
                    result.cook.push(cook_ms);
                    result.emit.push(emit_ms);
                    result.write.push(write_ms);
                }
                SkullReload::Incremental {
                    bytes,
                    reinferred,
                    cook_ms,
                    emit_ms,
                    write_ms,
                } => {
                    result.bytes = bytes;
                    result.reinferred = reinferred;
                    result.cook.push(cook_ms);
                    result.emit.push(emit_ms);
                    result.write.push(write_ms);
                }
                SkullReload::FullRequired { .. }
                | SkullReload::ShapeChanged { .. }
                | SkullReload::NoChange => {}
            }
        }
        if project.source_text(source).as_deref() != Some(orig.as_str()) {
            project.apply_edit(source, &orig, &main_path, &incremental_path)?;
        }
        results.push(result);
    }
    results.sort_by(|a, b| max(&b.samples).total_cmp(&max(&a.samples)));

    if json_out {
        let edits: Vec<_> = results
            .iter()
            .map(|r| {
                json!({
                    "file": r.file, "name": r.name, "line": r.line,
                    "kind": r.kind.label(), "reload": r.reload, "error": r.error,
                    "reinferred": r.reinferred, "overlay_bytes": r.bytes,
                    "mean_ms": mean(&r.samples), "max_ms": max(&r.samples),
                    "cook_mean_ms": mean(&r.cook), "emit_mean_ms": mean(&r.emit),
                    "write_mean_ms": mean(&r.write), "samples_ms": r.samples,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": path,
                "iters": iters,
                "warmup": warmup,
                "init_ms": init_ms,
                "main_bytes": main_bytes,
                "edits": edits,
            }))?
        );
        return Ok(());
    }

    eprintln!("=== Rock incremental-stats (skull hot reload): {path} ===");
    eprintln!(
        "initial build {init_ms:.1} ms, main.lua {main_bytes} B | {iters} iters/edit ({warmup} warmup)\n"
    );
    eprintln!(
        "    {:<26} {:<22} {:<10} {:<12} {:>8} {:>8}  [cook/emit/write ms, reinf, bytes]",
        "where", "fn", "kind", "reload", "mean", "max"
    );
    for r in &results {
        if let Some(error) = &r.error {
            eprintln!(
                "    {:<26} {:<22} {:<10} error: {}",
                format!("{}:{}", crate::bone_backend::short_path(&r.file), r.line),
                r.name,
                r.kind.label(),
                error.lines().next().unwrap_or("")
            );
            continue;
        }
        eprintln!(
            "    {:<26} {:<22} {:<10} {:<12} {:>8.3} {:>8.3}  [{:.2}/{:.2}/{:.2} r{} {}B]",
            format!("{}:{}", crate::bone_backend::short_path(&r.file), r.line),
            r.name,
            r.kind.label(),
            r.reload,
            mean(&r.samples),
            max(&r.samples),
            mean(&r.cook),
            mean(&r.emit),
            mean(&r.write),
            r.reinferred,
            r.bytes,
        );
    }
    Ok(())
}
