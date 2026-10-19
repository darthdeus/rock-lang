//! `rock iter-completion <project>` -- tight-loop profiler for the per-keystroke
//! `bone.completion()` path. The expensive part is
//! `call_through_param_completions` (the fused-query convenience, e.g.
//! `rejtanog -> reject_target(TargetError::NotGround)`), which runs in
//! expression position on every keystroke and, before caching, rebuilds an
//! O(types * globals) reachability expansion each call.
//!
//! Setup: warm the workspace (cold load excluded), append a scratch fn whose
//! body holds a call-through-triggering prefix, then loop `completion()` at that
//! prefix and report the latency distribution. No nvim, no transport -- a clean
//! tight loop to run under `perf record` / `samply` while improving the path.
//!
//! Env knobs:
//!   ITER_COMPL_PREFIX  the synthetic prefix typed in the scratch body (default
//!                      "spawnmob"); must partially match real fn names so the
//!                      call-through noise guard admits them and `arg_values`
//!                      runs -- otherwise the slow path isn't exercised.
//!   ITER_COMPL_FILE    substring of the target file to host the scratch fn.

use anyhow::Result;
use bone_query::{last_completion_phase_timings, offset_to_position, BoneWorkspace};
use std::path::{Path, PathBuf};
use bone_core::prelude::Instant;

pub(crate) fn collect_rock_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs_err::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                // Skip build/dep dirs that aren't the project's own source.
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "target" | ".git" | "tmp" | "node_modules") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rock") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

pub fn iter_completion(path: &str, iters: usize, warmup: usize, budget_ms: Option<f64>) -> Result<()> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        anyhow::bail!("iter-completion expects a project directory, got `{path}`");
    }
    let candidates = collect_rock_files(dir);
    if candidates.is_empty() {
        anyhow::bail!("no .rock files under `{path}`");
    }

    // Host file for the scratch fn: prefer ITER_COMPL_FILE, else the first
    // source file with a top-level `fn ` (so the buffer is a plausible module).
    let want = std::env::var("ITER_COMPL_FILE").ok();
    let target_path = candidates
        .iter()
        .filter(|p| {
            want.as_ref()
                .map(|w| p.to_string_lossy().contains(w.as_str()))
                .unwrap_or(true)
        })
        .find(|p| fs_err::read_to_string(p).map(|t| t.contains("fn ")).unwrap_or(false))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no .rock file with a `fn ` under `{path}`"))?;

    let target = fs_err::canonicalize(&target_path)
        .unwrap_or(target_path.clone())
        .to_string_lossy()
        .to_string();
    let target_name = target_path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.clone());

    // Build the buffer: original source + a scratch fn whose body is the probe
    // WORD. We then complete at EVERY cursor position along that word (prefix
    // lengths 1..=len), simulating typing it out. Short prefixes partial-match
    // many fns across many param types -- the expensive call_through case. The
    // word should be a real-ish identifier so its prefixes match project fn
    // names; override with ITER_COMPL_WORD.
    //
    // ITER_COMPL_CALLEE wraps the word as `<callee>(<word>` (an unterminated
    // call), putting the cursor in ARG position so the reachability pipeline
    // (phase B) also fires. The callee must be a real fn (its first-param type
    // drives the expected-type / reach query); else expected_types is empty and
    // reach stays inert, same as bare position.
    let word = std::env::var("ITER_COMPL_WORD").unwrap_or_else(|_| "spawn_mob".to_string());
    let callee = std::env::var("ITER_COMPL_CALLEE").ok().filter(|s| !s.is_empty());
    let orig = fs_err::read_to_string(&target_path)?;
    let body = match &callee {
        Some(c) => format!("{c}({word}"),
        None => word.clone(),
    };
    let scratch = format!("\nfn __iter_compl_probe() {{\n    {body}\n}}\n");
    let text = format!("{orig}{scratch}");
    let probe = format!("\n    {body}\n}}\n");
    let probe_start = text
        .rfind(&probe)
        .ok_or_else(|| anyhow::anyhow!("internal: scratch probe not found in buffer"))?;
    // Word starts after the indent, plus `<callee>(` when in arg-probe mode.
    let word_start = probe_start + "\n    ".len() + callee.as_ref().map_or(0, |c| c.len() + 1);
    // One cursor position per prefix length 1..=word.len (ASCII -> byte == char),
    // so we measure the FULL typing arc. Short prefixes (2-5 chars) are the
    // sweet spot and where the candidate set explodes.
    let positions: Vec<(usize, lsp_types::Position)> = (1..=word.len())
        .map(|l| (l, offset_to_position(&text, word_start + l)))
        .collect();
    if positions.is_empty() {
        anyhow::bail!("ITER_COMPL_WORD must be at least 1 char");
    }

    // Warm: cold load (project + stick.toml deps) + seed the buffer. Excluded
    // from the timing -- this is what the server does on first open.
    let mut bone = BoneWorkspace::with_identity_storage_exemptions(crate::registry::registered().identity_storage_exemptions);
    bone.load_project(&target);
    bone.update(&target, text.clone());

    let iters = iters.max(1);
    let warmup = warmup.max(1);
    for _ in 0..warmup {
        for (_, pos) in &positions {
            let _ = bone.completion(&target, *pos);
        }
    }

    // Per-prefix-length, per-phase samples so we can see WHICH keystroke is slow
    // AND which pipeline (general candidates / reachability / call-through) owns
    // the time. Phase timings come from the thread-local the backend records.
    struct LenSamples {
        len: usize,
        total: Vec<f64>,
        gaps: Vec<f64>,
        rank: Vec<f64>,
        reach: Vec<f64>,
        ct: Vec<f64>,
        n_general: usize,
        n_out: usize,
        ct_fns: usize,
        ct_arg_iters: usize,
        ct_survivors: usize,
    }
    let mut by_len: Vec<LenSamples> = positions
        .iter()
        .map(|(l, _)| LenSamples {
            len: *l,
            total: Vec::new(),
            gaps: Vec::new(),
            rank: Vec::new(),
            reach: Vec::new(),
            ct: Vec::new(),
            n_general: 0,
            n_out: 0,
            ct_fns: 0,
            ct_arg_iters: 0,
            ct_survivors: 0,
        })
        .collect();
    let mut all: Vec<f64> = Vec::new();
    for _ in 0..iters {
        for (i, (_, pos)) in positions.iter().enumerate() {
            let t = Instant::now();
            let _ = bone.completion(&target, *pos);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let ph = last_completion_phase_timings();
            let s = &mut by_len[i];
            s.total.push(ms);
            s.gaps.push(ph.candidates_ms);
            s.rank.push(ph.rank_ms);
            s.reach.push(ph.reach_ms);
            s.ct.push(ph.call_through_ms);
            s.n_general = ph.n_general;
            s.n_out = ph.n_out;
            s.ct_fns = ph.ct_fns;
            s.ct_arg_iters = ph.ct_arg_iters;
            s.ct_survivors = ph.ct_survivors;
            all.push(ms);
        }
    }

    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = all.len();
    let pct = |p: f64| all[((n as f64 * p) as usize).min(n - 1)];
    let mean = all.iter().sum::<f64>() / n as f64;

    eprintln!("=== Rock Iter-Completion (bone backend): {path} ===");
    eprintln!(
        "typing `{word}` in {target_name} | {iters} iters x {} prefix-lengths ({warmup} warmup)",
        positions.len()
    );
    eprintln!(
        "  completion()  mean {:.2}  p50 {:.2}  p95 {:.2}  min {:.2}  max {:.2}  [ms]",
        mean,
        pct(0.50),
        pct(0.95),
        all[0],
        all[n - 1],
    );
    eprintln!("  per-prefix-length median by phase (which keystroke + which pipeline):");
    eprintln!(
        "    {:>5}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}   {:>6} {:>5}   {:>5} {:>9} {:>5}",
        "chars", "total", "cands", "rank", "reach", "call-thru", "ncand", "nout", "ctfns", "argiters", "surv"
    );
    let med = |v: &[f64]| -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    };
    for s in &by_len {
        eprintln!(
            "    {:>5}  {:>8.2}  {:>8.2}  {:>8.2}  {:>8.2}  {:>9.2}   {:>6} {:>5}   {:>5} {:>9} {:>5}   `{}`",
            s.len,
            med(&s.total),
            med(&s.gaps),
            med(&s.rank),
            med(&s.reach),
            med(&s.ct),
            s.n_general,
            s.n_out,
            s.ct_fns,
            s.ct_arg_iters,
            s.ct_survivors,
            &word[..s.len],
        );
    }
    eprintln!("  (gaps=general gather; rank=rank+materialize; reach=type-path; call-thru=fused-query)");
    eprintln!("  (ctfns=fns past noise guard; argiters=(fn,arg) pairs tested; surv=pairs matched+scored)");
    // The perf gate (the compiler contract M0): fail loudly when the
    // WORST per-keystroke median walks past the budget. Medians, not maxes --
    // a single scheduler hiccup must not flake the gate.
    if let Some(budget) = budget_ms {
        let worst = by_len
            .iter()
            .map(|s| (med(&s.total), s.len))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
            .unwrap_or((0.0, 0));
        if worst.0 > budget {
            anyhow::bail!(
                "completion perf gate FAILED: worst keystroke median {:.2}ms (prefix `{}`) exceeds budget {budget:.2}ms",
                worst.0,
                &word[..worst.1],
            );
        }
        eprintln!(
            "  perf gate OK: worst keystroke median {:.2}ms (prefix `{}`) within budget {budget:.2}ms",
            worst.0,
            &word[..worst.1],
        );
    }
    Ok(())
}
