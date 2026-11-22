//! Time candidate body, signature and broken edits through the live LSP update and refresh path.

use anyhow::Result;
use bone_core::prelude::Instant;
use bone_query::{BoneWorkspace, offset_to_position};
use bone_semantic::prelude::{Idx, ModuleId};
use lsp_types::{Position, Range};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;

use crate::skull_backend::{body_numeric_edit, ms};

#[cfg(test)]
mod tests {
    #[test]
    fn signature_edit_keeps_the_receiver_first() {
        let orig = "pub fn T.get(pool, key: string) -> int { 1 }\n";
        let f = super::FnStat {
            file: String::new(),
            name: String::new(),
            line: 1,
            name_off: orig.find("get").unwrap(),
            span_end: orig.len(),
            body_range: orig.find('{').unwrap()..orig.rfind('}').unwrap() + 1,
            body_bytes: 0,
            params: 2,
            has_ret: true,
            call_fan_in: 0,
            mod_fan_in: 0,
        };
        assert_eq!(
            super::make_sig_edit(orig, &f).as_deref(),
            Some("pub fn T.get(pool, __lspx: int, key: string) -> int { 1 }\n")
        );
    }

    #[test]
    fn missing_survey_source_is_an_error() {
        let path = "/tmp/rock-lsp-survey-missing-source.rock";
        let err = super::read_survey_source(path).expect_err("missing source must not be skipped");
        assert!(err.to_string().contains(path));
    }

    #[test]
    fn survey_restore_errors_are_propagated() {
        let err = super::require_survey_restore("fixture.rock", Err::<(), _>("restore failed"))
            .expect_err("restore failure must not be discarded");
        assert!(err.to_string().contains("fixture.rock"));
        assert!(err.to_string().contains("restore failed"));
    }

    #[test]
    fn only_broken_candidates_may_be_classified_as_parse_errors() {
        let err = super::require_expected_survey_update(
            super::EditKind::Body,
            Err::<(), _>("unexpected update failure"),
        )
        .expect_err("a normal candidate failure must abort the survey");
        assert!(err.to_string().contains("unexpected update failure"));
        assert!(
            super::require_expected_survey_update(
                super::EditKind::Broken,
                Err::<(), _>("expected parse failure"),
            )
            .expect("broken candidates classify expected parse failures")
            .is_none()
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    Body,
    Signature,
    Broken,
}

impl EditKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            EditKind::Body => "body",
            EditKind::Signature => "signature",
            EditKind::Broken => "broken",
        }
    }
    fn expected_tier(self) -> &'static str {
        match self {
            EditKind::Body => "BodyOnly",
            EditKind::Signature => "Interface",
            EditKind::Broken => "FullUser",
        }
    }
}

#[derive(Clone)]
pub(crate) struct FnStat {
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) line: u32,
    pub(crate) name_off: usize,
    pub(crate) span_end: usize,
    pub(crate) body_range: std::ops::Range<usize>,
    pub(crate) body_bytes: usize,
    pub(crate) params: usize,
    pub(crate) has_ret: bool,
    pub(crate) call_fan_in: usize,
    pub(crate) mod_fan_in: usize,
}

pub(crate) struct ModStat {
    file: String,
    fns: usize,
    items: usize,
    body_bytes: usize,
    fan_in: usize,
}

struct CandResult {
    file: String,
    name: String,
    line: u32,
    kind: EditKind,
    tier: String,
    reinferred: usize,
    diags: usize,
    edit_mean: f64,
    edit_max: f64,
    update_mean: f64,
    semtok_mean: f64,
    inlay_mean: f64,
    hover_mean: f64,
    compl_mean: f64,
    snapshot_median: f64,
    allocation_median: f64,
    samples: Vec<f64>,
}

fn line_of(text: &str, off: usize) -> u32 {
    offset_to_position(text, off.min(text.len())).line + 1
}

pub(crate) fn read_survey_source(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("lsp-survey failed to read source `{path}`: {e}"))
}

fn require_survey_restore<T, E: std::fmt::Display>(
    path: &str,
    result: std::result::Result<T, E>,
) -> Result<()> {
    result
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("lsp-survey failed to restore `{path}`: {e}"))
}

fn require_expected_survey_update<T, E: std::fmt::Display>(
    kind: EditKind,
    result: std::result::Result<T, E>,
) -> Result<Option<T>> {
    match result {
        Ok(update) => Ok(Some(update)),
        Err(_) if kind == EditKind::Broken => Ok(None),
        Err(e) => anyhow::bail!("lsp-survey {} candidate update failed: {e}", kind.label()),
    }
}

/// Collect per-module + per-function stats for the PROJECT files (deps/stdlib
/// excluded) from a warm Session's workspace.
pub(crate) fn collect_stats(
    session: &bone_incremental::session::Session,
    proj_marker: &str,
) -> Result<(Vec<ModStat>, Vec<FnStat>)> {
    let ws = &session.ws;

    // One O(workspace) pass: tally call sites resolving to each (module, fn).
    let mut call_tally: HashMap<(ModuleId, Idx<bone_semantic::prelude::hir::Function>), usize> =
        HashMap::new();
    for (_mid, inf) in ws.inferences_iter() {
        for bi in inf.per_fn.values() {
            for callee in bi.resolved_overload_calls.values() {
                *call_tally.entry(*callee).or_default() += 1;
            }
        }
    }

    let mut mods = Vec::new();
    let mut fns = Vec::new();
    for module in ws.modules.iter() {
        let file = module.file.as_str().to_string();
        if session.warm_mids.contains(&module.id) || !file.contains(proj_marker) {
            continue;
        }
        let text = read_survey_source(&file)?;

        // Module fan-in: distinct modules depending on any symbol defined here.
        let mut dependent_mods = std::collections::HashSet::new();
        let defined = module
            .functions_by_name
            .keys()
            .chain(module.nominals.keys())
            .chain(module.global_by_name.keys());
        for name in defined {
            if let Some(set) = session.dependents.get(name) {
                for m in set {
                    if *m != module.id {
                        dependent_mods.insert(*m);
                    }
                }
            }
        }

        let mut body_bytes = 0usize;
        let mut fn_count = 0usize;
        for (fid, func) in module.functions.iter() {
            // Editable rock bodies only: skip externs and inline bodies.
            if func.is_extern || func.inline_body.is_some() {
                continue;
            }
            let (s, e) = func.span.offset_range();
            let Some(body) = module
                .bodies
                .get(&bone_semantic::hir::BodyOwner::Function(fid))
            else {
                continue;
            };
            let Some(root) = body.root else { continue };
            let (body_start, body_end) = body.expr_spans[root].offset_range();
            let bytes = e.saturating_sub(s);
            body_bytes += bytes;
            fn_count += 1;
            let name_off = func.name_span.offset_range().0;
            fns.push(FnStat {
                file: file.clone(),
                name: func.name.as_str().to_string(),
                line: line_of(&text, name_off),
                name_off,
                span_end: e,
                body_range: body_start..body_end,
                body_bytes: bytes,
                params: func.param_decls.len(),
                has_ret: func.return_ty.is_some(),
                call_fan_in: call_tally.get(&(module.id, fid)).copied().unwrap_or(0),
                mod_fan_in: session
                    .dependents
                    .get(&func.name)
                    .map(|s| s.iter().filter(|m| **m != module.id).count())
                    .unwrap_or(0),
            });
        }

        let items = module.functions.len()
            + module.structs.len()
            + module.enums.len()
            + module.newtypes.len()
            + module.globals.len();
        mods.push(ModStat {
            file,
            fns: fn_count,
            items,
            body_bytes,
            fan_in: dependent_mods.len(),
        });
    }
    Ok((mods, fns))
}

/// SIGNATURE edit: insert a dummy first param so the arity (interface) changes.
fn make_sig_edit(orig: &str, f: &FnStat) -> Option<String> {
    let paren = orig[f.name_off..f.span_end].find('(')? + f.name_off;
    let after = paren + 1;
    // `fn T.name(self, ..)`: the receiver has to stay first and untyped.
    let (at, insert) = if orig[..f.name_off].ends_with('.') {
        (end_of_first_param(orig, after)?, ", __lspx: int")
    } else if f.params == 0 {
        (after, "__lspx: int")
    } else {
        (after, "__lspx: int, ")
    };
    let mut out = String::with_capacity(orig.len() + insert.len());
    out.push_str(&orig[..at]);
    out.push_str(insert);
    out.push_str(&orig[at..]);
    Some(out)
}

/// The byte offset ending the parameter that starts at `start`: its top-level
/// `,` or the list's closing `)`.
fn end_of_first_param(orig: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in orig[start..].char_indices() {
        match c {
            '(' | '[' | '<' | '{' => depth += 1,
            ',' | ')' if depth == 0 => return Some(start + i),
            ')' | ']' | '>' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Insert an unterminated quote to exercise mid-edit parsing.
fn make_broken_edit(orig: &str, f: &FnStat) -> Option<String> {
    let at = f.body_range.start + 1;
    let mut out = String::with_capacity(orig.len() + 1);
    out.push_str(&orig[..at]);
    out.push('"');
    out.push_str(&orig[at..]);
    Some(out)
}

pub(crate) fn make_edit(orig: &str, f: &FnStat, kind: EditKind) -> Result<Option<String>> {
    Ok(match kind {
        EditKind::Body => return body_numeric_edit(orig, f.body_range.clone()),
        EditKind::Signature => make_sig_edit(orig, f),
        EditKind::Broken => make_broken_edit(orig, f),
    })
}

pub(crate) fn lsp_survey(
    path: &str,
    iters: usize,
    warmup: usize,
    json_out: bool,
    allocation_hints: bool,
) -> Result<()> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        anyhow::bail!("lsp-survey expects a project directory, got `{path}`");
    }
    let proj_marker = format!(
        "{}/",
        dir.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    );

    // Discover the whole compilation unit (project + stick.toml deps) and warm a
    // Session (for stats + tier ground-truth) + a BoneWorkspace (for LSP timing).
    let sources = rock_host::compilation_units::load_sources_for_project(dir)?;
    let mut session = crate::bone_backend::bone_session(&sources)
        .map_err(|e| anyhow::anyhow!("bone session warm failed: {}", e.message))?;

    let (mut mods, mut fns) = collect_stats(&session, &proj_marker)?;
    if fns.is_empty() {
        anyhow::bail!("no editable project functions found under `{path}`");
    }

    // Warm the LSP facade off the largest project file's unit.
    mods.sort_by_key(|a| std::cmp::Reverse(a.body_bytes));
    fns.sort_by_key(|a| std::cmp::Reverse(a.body_bytes));
    let warm_file = fns[0].file.clone();
    let warm_canon = fs_err::canonicalize(&warm_file)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(warm_file);
    let mut bone = BoneWorkspace::new();
    let t = Instant::now();
    bone.update(&warm_canon, read_survey_source(&warm_canon)?);
    let cold_ms = ms(t);
    let t = Instant::now();
    let snapshot = bone.allocation_snapshot(&warm_canon, [warm_canon.as_str()], allocation_hints);
    let cold_snapshot_ms = ms(t);
    let t = Instant::now();
    let cold_allocations = snapshot
        .map(|snapshot| snapshot.diagnostics())
        .unwrap_or_default();
    let cold_allocation_ms = ms(t);
    let cold_allocation_diagnostics: usize = cold_allocations.values().map(Vec::len).sum();

    // Generate candidates: top targets by each metric x edit kind, deduped.
    let mut seen = std::collections::HashSet::new();
    let mut candidates: Vec<(FnStat, EditKind)> = Vec::new();
    let add = |f: &FnStat,
               kind: EditKind,
               out: &mut Vec<(FnStat, EditKind)>,
               seen: &mut std::collections::HashSet<(String, String, u8)>| {
        let key = (f.file.clone(), f.name.clone(), kind as u8);
        if seen.insert(key) {
            out.push((f.clone(), kind));
        }
    };

    let mut by_body = fns.clone();
    by_body.sort_by_key(|a| std::cmp::Reverse(a.body_bytes));
    let mut by_calls = fns.clone();
    by_calls.sort_by_key(|a| std::cmp::Reverse(a.call_fan_in));
    let mut by_modfan = fns.clone();
    by_modfan.sort_by_key(|a| std::cmp::Reverse(a.mod_fan_in));

    for f in by_body.iter().take(5) {
        add(f, EditKind::Body, &mut candidates, &mut seen);
        add(f, EditKind::Broken, &mut candidates, &mut seen);
    }
    for f in by_calls.iter().take(5) {
        add(f, EditKind::Body, &mut candidates, &mut seen);
        add(f, EditKind::Signature, &mut candidates, &mut seen);
    }
    for f in by_modfan.iter().filter(|f| f.mod_fan_in > 0).take(5) {
        add(f, EditKind::Signature, &mut candidates, &mut seen);
    }

    let iters = iters.max(15);
    let full_range = Range {
        start: Position::new(0, 0),
        end: Position::new(u32::MAX, 0),
    };
    let mut results: Vec<CandResult> = Vec::new();

    for (f, kind) in &candidates {
        let canon = fs_err::canonicalize(&f.file)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| f.file.clone());
        let orig = read_survey_source(&canon)?;
        let Some(edited) = make_edit(&orig, f, *kind)? else {
            eprintln!(
                "skipping {} edit of {}: no editable literal or declaration",
                kind.label(),
                f.name
            );
            continue;
        };

        // Ground-truth tier + reinferred count from the Session (then revert).
        let (tier, reinferred) =
            match require_expected_survey_update(*kind, session.update_file(&canon, &edited))? {
                Some(u) => (format!("{:?}", u.tier), u.reinferred.len()),
                None => ("ParseErr".to_string(), 0),
            };
        require_survey_restore(&canon, session.update_file(&canon, &orig))?;

        // Time the live LSP path on the BoneWorkspace, alternating edited/orig.
        bone.update(&canon, orig.clone());
        let cursor = offset_to_position(&orig, f.name_off.min(orig.len().saturating_sub(1)));
        let (mut upd, mut sem, mut inl, mut hov, mut cmp) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut samples = Vec::with_capacity(iters);
        let mut snapshot_samples = Vec::with_capacity(iters);
        let mut allocation_samples = Vec::with_capacity(iters);
        let mut last_diags = 0usize;
        for i in 0..(warmup + iters) {
            let text = if i % 2 == 0 { &edited } else { &orig };
            let owned = text.clone();
            let t = Instant::now();
            let d = bone.update(&canon, owned);
            let u_ms = ms(t);
            let t = Instant::now();
            let snapshot = bone.allocation_snapshot(&canon, [canon.as_str()], allocation_hints);
            let snapshot_ms = ms(t);
            let t = Instant::now();
            let allocations = snapshot
                .map(|snapshot| snapshot.diagnostics())
                .unwrap_or_default();
            let allocation_ms = ms(t);
            let dc: usize = d.iter().map(|(_, v)| v.len()).sum::<usize>()
                + allocations.values().map(Vec::len).sum::<usize>();

            let t = Instant::now();
            let _ = bone.semantic_tokens(&canon);
            let s_ms = ms(t);

            let t = Instant::now();
            let _ = bone.inlay_hints(&canon, full_range);
            let i_ms = ms(t);

            let t = Instant::now();
            let _ = bone.hover(&canon, cursor);
            let h_ms = ms(t);

            let t = Instant::now();
            let _ = bone.completion(&canon, cursor);
            let c_ms = ms(t);

            if i >= warmup {
                upd.push(u_ms);
                sem.push(s_ms);
                inl.push(i_ms);
                hov.push(h_ms);
                cmp.push(c_ms);
                samples.push(u_ms + s_ms + i_ms);
                snapshot_samples.push(snapshot_ms);
                allocation_samples.push(allocation_ms);
                last_diags = dc;
            }
        }
        bone.update(&canon, orig.clone()); // restore baseline

        let mean = |v: &[f64]| {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        let max = samples.iter().cloned().fold(0.0_f64, f64::max);
        snapshot_samples.sort_by(f64::total_cmp);
        allocation_samples.sort_by(f64::total_cmp);
        let snapshot_median = snapshot_samples[snapshot_samples.len() / 2];
        let allocation_median = allocation_samples[allocation_samples.len() / 2];
        results.push(CandResult {
            file: f.file.clone(),
            name: f.name.clone(),
            line: f.line,
            kind: *kind,
            tier,
            reinferred,
            diags: last_diags,
            edit_mean: mean(&samples),
            edit_max: max,
            update_mean: mean(&upd),
            semtok_mean: mean(&sem),
            inlay_mean: mean(&inl),
            hover_mean: mean(&hov),
            compl_mean: mean(&cmp),
            snapshot_median,
            allocation_median,
            samples,
        });
    }

    results.sort_by(|a, b| b.edit_max.partial_cmp(&a.edit_max).unwrap());

    if json_out {
        let mods_json: Vec<_> = mods
            .iter()
            .map(|m| {
                json!({
                    "file": m.file, "fns": m.fns, "items": m.items,
                    "body_bytes": m.body_bytes, "fan_in": m.fan_in,
                })
            })
            .collect();
        let fns_json: Vec<_> = fns
            .iter()
            .map(|f| {
                json!({
                    "file": f.file, "name": f.name, "line": f.line,
                    "body_bytes": f.body_bytes, "params": f.params, "has_ret": f.has_ret,
                    "call_fan_in": f.call_fan_in, "mod_fan_in": f.mod_fan_in,
                })
            })
            .collect();
        let res_json: Vec<_> = results
            .iter()
            .map(|r| {
                json!({
                    "file": r.file, "name": r.name, "line": r.line,
                    "kind": r.kind.label(), "tier": r.tier, "reinferred": r.reinferred,
                    "diags": r.diags, "edit_mean_ms": r.edit_mean, "edit_max_ms": r.edit_max,
                    "update_mean_ms": r.update_mean, "semtok_mean_ms": r.semtok_mean,
                    "inlay_mean_ms": r.inlay_mean, "hover_mean_ms": r.hover_mean,
                    "compl_mean_ms": r.compl_mean, "samples_ms": r.samples,
                    "allocation_median_ms": r.allocation_median, "snapshot_median_ms": r.snapshot_median,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": path, "cold_ms": cold_ms, "cold_snapshot_ms": cold_snapshot_ms,
                "cold_allocation_ms": cold_allocation_ms,
                "allocation_hints": allocation_hints, "cold_allocation_diagnostics": cold_allocation_diagnostics,
                "modules": mods_json,
                "functions": fns_json,
                "candidates": res_json,
            }))?
        );
        return Ok(());
    }

    // -- pretty report --
    eprintln!("=== Rock LSP-Survey (bone backend): {path} ===");
    eprintln!(
        "{} project modules, {} editable fns | {iters} iters/candidate ({warmup} warmup)\n",
        mods.len(),
        fns.len()
    );

    eprintln!(
        "  cold update {cold_ms:.2} ms, snapshot {cold_snapshot_ms:.2} ms, allocation diagnostics {cold_allocation_ms:.2} ms ({cold_allocation_diagnostics})"
    );
    eprintln!("  biggest modules (by body bytes):");
    for m in mods.iter().take(8) {
        eprintln!(
            "    {:<28} {:>6} B  {:>3} fns  {:>3} items  fan-in {:>3}",
            crate::bone_backend::short_path(&m.file),
            m.body_bytes,
            m.fns,
            m.items,
            m.fan_in
        );
    }
    eprintln!("\n  biggest functions (by body bytes):");
    for f in by_body.iter().take(8) {
        eprintln!(
            "    {:<22} {:<24} {:>6} B",
            format!("{}:{}", crate::bone_backend::short_path(&f.file), f.line),
            f.name,
            f.body_bytes
        );
    }
    eprintln!("\n  most-called functions (call-site fan-in):");
    for f in by_calls.iter().filter(|f| f.call_fan_in > 0).take(8) {
        eprintln!(
            "    {:<22} {:<24} {:>4} calls  {:>3} dep-mods",
            format!("{}:{}", crate::bone_backend::short_path(&f.file), f.line),
            f.name,
            f.call_fan_in,
            f.mod_fan_in
        );
    }

    eprintln!("\n  WORST EDITS (ranked by max per-edit auto-refresh latency):");
    eprintln!(
        "    {:<26} {:<22} {:<10} {:<9} {:>7} {:>7}  [upd/sem/inl/hov/cmp ms, reinf, diags]",
        "where", "fn", "kind", "tier", "mean", "max"
    );
    for r in results.iter().take(20) {
        let tier_tag = if r.tier.contains(r.kind.expected_tier()) || r.tier == "ParseErr" {
            r.tier.clone()
        } else {
            format!("{}!", r.tier) // actual tier differs from the edit's intent
        };
        eprintln!(
            "    {:<26} {:<22} {:<10} {:<9} {:>7.3} {:>7.3}  [{:.2}/{:.2}/{:.2}/{:.2}/{:.2} r{} d{}]",
            format!("{}:{}", crate::bone_backend::short_path(&r.file), r.line),
            r.name,
            r.kind.label(),
            tier_tag,
            r.edit_mean,
            r.edit_max,
            r.update_mean,
            r.semtok_mean,
            r.inlay_mean,
            r.hover_mean,
            r.compl_mean,
            r.reinferred,
            r.diags,
        );
    }
    if let Some(worst) = results.first() {
        eprintln!(
            "\n  slowest: {} edit in {} `{}` -- {:.3} ms mean ({:.3} ms max), tier {}",
            worst.kind.label(),
            crate::bone_backend::short_path(&worst.file),
            worst.name,
            worst.edit_mean,
            worst.edit_max,
            worst.tier,
        );
    }
    Ok(())
}
