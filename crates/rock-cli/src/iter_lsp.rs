//! `rock iter-lsp <project>` -- tight-loop profiler for the per-edit LSP query
//! path on the bone backend.
//!
//! The live editor (nvim `backend='bone'`) drives every keystroke through
//! `BoneWorkspace` (the facade in `bone-query` the interactive server calls):
//! `update` (didChange -> re-typecheck + cross-file diagnostics + hole
//! diagnostics), then a `semantic_tokens` full refresh and an `inlay_hints`
//! refresh, plus on-demand `hover`/`completion`/`definition` under the cursor
//! and a member (`recv.fie`) completion at the file's first member access --
//! the speculative-variant path plain completion never exercises.
//! `iter-incremental` profiles the bone `Session` + skull emit underneath; this
//! profiles the LSP query layer ON TOP of it -- the costs an editor actually
//! pays per keystroke.
//!
//! The workspace + whole compilation unit load ONCE (untimed warm); each pass
//! applies an alternating 1-char body edit (so every pass is a real change) and
//! times each operation, reporting the per-op distribution (mean/p50/p99).
//!
//! Targeting env vars (mirroring `iter-incremental`):
//! - `ITER_LSP_FN=<name>`  -- edit a real digit in that fn's body (alternated).
//! - `ITER_LSP_FILE=<sub>` -- restrict the auto-picked file to one matching `<sub>`.

use anyhow::Result;
use bone_query::{offset_to_position, BoneWorkspace};
use lsp_types::{Position, Range};
use std::path::{Path, PathBuf};
use bone_core::prelude::Instant;

use crate::skull_backend::{body_digit_flip, body_touch, ms};

/// Per-operation timing accumulator + distribution reporter.
struct Stat {
    label: &'static str,
    samples: Vec<f64>,
    /// Last-pass result count (tokens / hints / completions / diags), for context.
    last_count: usize,
}

impl Stat {
    fn new(label: &'static str) -> Self {
        Stat { label, samples: Vec::new(), last_count: 0 }
    }

    fn report(&self) {
        if self.samples.is_empty() {
            eprintln!("  {:<22} (no samples)", self.label);
            return;
        }
        let n = self.samples.len() as f64;
        let mean = self.samples.iter().sum::<f64>() / n;
        let var = self.samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
        let stddev = var.sqrt();
        let cv = if mean > 0.0 { stddev / mean * 100.0 } else { 0.0 };
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| sorted[((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize];
        eprintln!(
            "  {:<22} mean {:>7.3}  p50 {:>7.3}  p99 {:>7.3}  min {:>7.3}  max {:>7.3}  (cv {:>4.1}%)  [{} last]",
            self.label,
            mean,
            pct(50.0),
            pct(99.0),
            sorted[0],
            sorted[sorted.len() - 1],
            cv,
            self.last_count,
        );
    }
}

/// Offset of the first NON-KEYWORD identifier inside the first top-level `fn`
/// body -- a position where hover/definition/references resolve a real symbol
/// (a bare keyword like `let`/`if` resolves to nothing, so references would
/// return early and under-report the O(workspace) cost).
fn body_cursor(text: &str) -> Option<usize> {
    const KEYWORDS: &[&str] = &[
        "let", "mut", "if", "else", "for", "while", "return", "match", "in", "co", "yield",
        "true", "false", "null", "break", "continue", "and", "or", "not", "as", "fn", "const",
    ];
    let fn_pos = if text.starts_with("fn ") { 0 } else { text.find("\nfn ")? + 1 };
    let brace = text[fn_pos..].find('{')? + fn_pos;
    let body = &text[brace + 1..];
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < body.len() {
        // Advance to the next identifier start.
        let Some(rel) = body[i..].find(|c: char| c.is_alphabetic() || c == '_') else {
            return None;
        };
        let start = i + rel;
        let end = start + body[start..].find(|c: char| !is_ident(c)).unwrap_or(body.len() - start);
        let word = &body[start..end];
        if !KEYWORDS.contains(&word) {
            return Some(brace + 1 + start);
        }
        i = end;
    }
    None
}

/// Offset one char into the field of the first `recv.fie` member access inside
/// a fn body -- drives the MEMBER completion path (`receiver_path` triggers on
/// the preceding `.`), which reparses and is invisible to the plain-completion
/// timing. One char in so the path also pays its prefix filter.
fn member_cursor(text: &str) -> Option<usize> {
    let fn_pos = if text.starts_with("fn ") { 0 } else { text.find("\nfn ")? + 1 };
    let b = text.as_bytes();
    let mut i = fn_pos;
    while i + 2 < b.len() {
        let recv_ok = i > 0 && (b[i - 1].is_ascii_alphabetic() || b[i - 1] == b'_');
        if b[i] == b'.' && recv_ok && b[i + 1].is_ascii_alphabetic() {
            // Skip a dot inside a `//` comment (scan back to the line start).
            let line_start = text[..i].rfind('\n').map_or(0, |p| p + 1);
            if !text[line_start..i].contains("//") {
                return Some(i + 2);
            }
        }
        i += 1;
    }
    None
}

/// Collect `.rock` files under `dir` (recursively), skipping hidden dirs and
/// `target`. Used to pick an editable PROJECT file (deps load via the unit).
fn collect_rock_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.') && name != "target"
        })
        .flatten()
    {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("rock") {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    out
}

pub fn iter_lsp(path: &str, iters: usize, warmup: usize) -> Result<()> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        anyhow::bail!("iter-lsp expects a project directory, got `{path}`");
    }

    let candidates = collect_rock_files(dir);
    if candidates.is_empty() {
        anyhow::bail!("no .rock files under `{path}`");
    }

    // Choose the target file + two alternating text states.
    let want_file = std::env::var("ITER_LSP_FILE").ok();
    let (target_path, state_a, state_b) = if let Ok(fname) = std::env::var("ITER_LSP_FN") {
        // Edit a real digit in `fn <fname>`'s body, alternating flipped <-> original.
        let mut chosen = None;
        for p in &candidates {
            let Ok(t) = fs_err::read_to_string(p) else { continue };
            if t.contains(&format!("fn {fname}")) {
                if let Some(flipped) = body_digit_flip(&t, &fname) {
                    chosen = Some((p.clone(), flipped, t));
                    break;
                }
            }
        }
        chosen.ok_or_else(|| {
            anyhow::anyhow!("no file with a digit-flippable `fn {fname}` body under `{path}`")
        })?
    } else {
        // Auto-pick the first file with a touchable top-level fn body. Prefer a
        // non-test file (more representative of editing real game code).
        let pick = candidates
            .iter()
            .filter(|p| {
                want_file
                    .as_ref()
                    .map(|w| p.to_string_lossy().contains(w.as_str()))
                    .unwrap_or(true)
            })
            .find(|p| {
                fs_err::read_to_string(p)
                    .ok()
                    .and_then(|t| body_touch(&t))
                    .is_some()
            })
            .ok_or_else(|| anyhow::anyhow!("no file with a touchable fn body under `{path}`"))?;
        let orig = fs_err::read_to_string(pick)?;
        let a = body_touch(&orig).unwrap();
        let b = body_touch(&a).unwrap();
        (pick.clone(), a, b)
    };

    let target = fs_err::canonicalize(&target_path)
        .unwrap_or(target_path.clone())
        .to_string_lossy()
        .to_string();
    let target_name = target_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    // Warm: build the workspace and batch-index the whole compilation unit
    // (project + stick.toml deps), exactly as the server does on first open.
    // This is the cold load -- excluded from the per-edit timing below.
    let mut bone = BoneWorkspace::with_identity_storage_exemptions(crate::registry::registered().identity_storage_exemptions);
    bone.load_project(&target);
    // Seed the edited file's buffer so the first timed pass is a steady-state edit.
    let _ = bone.update(&target, state_a.clone());

    let iters = iters.max(1);
    let full_range = Range {
        start: Position::new(0, 0),
        end: Position::new(u32::MAX, 0),
    };

    let mut s_update = Stat::new("update (didChange)");
    let mut s_semtok = Stat::new("semantic_tokens");
    let mut s_inlay = Stat::new("inlay_hints");
    let mut s_hover = Stat::new("hover");
    let mut s_compl = Stat::new("completion");
    let mut s_member = Stat::new("completion (member .)");
    let mut s_def = Stat::new("definition");
    // Additional editor traffic not in the auto-refresh trio: document_highlight
    // fires on every cursor move (O(workspace) via references); doc/workspace
    // symbols + signature_help round out what an editor actually requests.
    let mut s_dhl = Stat::new("document_highlight");
    let mut s_refs = Stat::new("references");
    let mut s_docsym = Stat::new("document_symbols");
    let mut s_wssym = Stat::new("workspace_symbols");
    let mut s_sig = Stat::new("signature_help");
    // Realistic Ctrl-T query: a short substring (workspace_symbols matches by
    // `.contains`). "" would be the all-symbols worst case; 2 chars is typical.
    let ws_query: String = target_name.chars().filter(|c| c.is_alphabetic()).take(2).collect();
    let mut per_edit: Vec<f64> = Vec::with_capacity(iters);

    for i in 0..(warmup + iters) {
        let text = if i % 2 == 0 { &state_a } else { &state_b };
        let cursor = body_cursor(text).map(|off| offset_to_position(text, off));
        let member_pos = member_cursor(text).map(|off| offset_to_position(text, off));

        let owned = text.clone();
        let t = Instant::now();
        let diags = bone.update(&target, owned);
        let update_ms = ms(t);
        let diag_count: usize = diags.iter().map(|(_, d)| d.len()).sum();

        let t = Instant::now();
        let tokens = bone.semantic_tokens(&target);
        let semtok_ms = ms(t);

        let t = Instant::now();
        let hints = bone.inlay_hints(&target, full_range);
        let inlay_ms = ms(t);

        // document_symbols + workspace_symbols don't need a cursor.
        let t = Instant::now();
        let docsym = bone.document_symbols(&target);
        let docsym_ms = ms(t);

        let t = Instant::now();
        let wssym = bone.workspace_symbols(&ws_query);
        let wssym_ms = ms(t);

        let (mut hover_ms, mut compl_ms, mut def_ms) = (None, None, None);
        let (mut dhl_ms, mut refs_ms, mut sig_ms) = (None, None, None);
        let (mut compl_count, mut dhl_count, mut refs_count) = (0, 0, 0);
        if let Some(pos) = cursor {
            let t = Instant::now();
            let _ = bone.hover(&target, pos);
            hover_ms = Some(ms(t));

            let t = Instant::now();
            let compl = bone.completion(&target, pos);
            compl_ms = Some(ms(t));
            compl_count = compl.len();

            let t = Instant::now();
            let _ = bone.definition(&target, pos);
            def_ms = Some(ms(t));

            // Fires on every cursor move; O(workspace) via references.
            let t = Instant::now();
            let dhl = bone.document_highlight(&target, pos);
            dhl_ms = Some(ms(t));
            dhl_count = dhl.len();

            let t = Instant::now();
            let refs = bone.references(&target, pos);
            refs_ms = Some(ms(t));
            refs_count = refs.len();

            let t = Instant::now();
            let _ = bone.signature_help(&target, pos);
            sig_ms = Some(ms(t));
        }

        // Member (`recv.fie`) completion goes through the reparse-based path,
        // which the edit-site completion above never exercises.
        let (mut member_ms, mut member_count) = (None, 0);
        if let Some(mpos) = member_pos {
            let t = Instant::now();
            let items = bone.completion(&target, mpos);
            member_ms = Some(ms(t));
            member_count = items.len();
        }

        if i >= warmup {
            s_update.samples.push(update_ms);
            s_update.last_count = diag_count;
            s_semtok.samples.push(semtok_ms);
            s_semtok.last_count = tokens.data.len();
            s_inlay.samples.push(inlay_ms);
            s_inlay.last_count = hints.len();
            if let Some(v) = hover_ms {
                s_hover.samples.push(v);
            }
            if let Some(v) = compl_ms {
                s_compl.samples.push(v);
                s_compl.last_count = compl_count;
            }
            if let Some(v) = member_ms {
                s_member.samples.push(v);
                s_member.last_count = member_count;
            }
            if let Some(v) = def_ms {
                s_def.samples.push(v);
            }
            s_docsym.samples.push(docsym_ms);
            s_docsym.last_count = docsym.len();
            s_wssym.samples.push(wssym_ms);
            s_wssym.last_count = wssym.len();
            if let Some(v) = dhl_ms {
                s_dhl.samples.push(v);
                s_dhl.last_count = dhl_count;
            }
            if let Some(v) = refs_ms {
                s_refs.samples.push(v);
                s_refs.last_count = refs_count;
            }
            if let Some(v) = sig_ms {
                s_sig.samples.push(v);
            }
            per_edit.push(update_ms + semtok_ms + inlay_ms);
        }
    }

    let edit_mean = per_edit.iter().sum::<f64>() / per_edit.len() as f64;
    let cursor_note = if body_cursor(&state_a).is_some() {
        "edit-site cursor"
    } else {
        "no cursor resolved -- hover/completion/definition skipped"
    };

    eprintln!("=== Rock Iter-LSP (bone backend): {path} ===");
    eprintln!(
        "editing `{target_name}` | {iters} iters ({warmup} warmup) | {cursor_note}\n"
    );
    eprintln!("  per-op latency [ms]:");
    s_update.report();
    s_semtok.report();
    s_inlay.report();
    eprintln!("    (inlay_hints over the WHOLE file -- an upper bound; editors request only the viewport range)");
    s_hover.report();
    s_compl.report();
    s_member.report();
    s_def.report();
    s_dhl.report();
    s_refs.report();
    s_docsym.report();
    eprintln!("    (workspace_symbols query {ws_query:?})");
    s_wssym.report();
    s_sig.report();
    eprintln!(
        "\n  per-edit auto refresh (update + semantic_tokens + inlay_hints): mean {:.3} ms",
        edit_mean
    );
    eprintln!("  (the work the editor does on EVERY keystroke; others are on cursor-move / on-demand)");
    Ok(())
}
