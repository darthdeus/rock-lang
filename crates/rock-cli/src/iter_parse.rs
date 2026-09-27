//! Front-end profiling loops: `rock iter-tokenize` and `rock iter-parse`.
//!
//! Both discover sources ONCE (stick.toml, deps, globbing) and then re-run a
//! single front-end stage N times so a sampling profiler attributes nearly
//! every sample to that one stage:
//!
//! * `iter-tokenize <project> --iters N` -- the loop body is just
//!   `Tokenizer::tokenize` per file; the parser is never constructed.
//! * `iter-parse <project> --iters N` -- tokenizes every file ONCE up front and
//!   caches the token streams, so the timed loop is *only* the skull parser.
//!
//! Run either under a sampler with a large N (1000-5000) so the process runs
//! long enough for statistically meaningful data:
//!
//! ```text
//! samply record -- rock iter-tokenize mag --iters 3000
//! samply record -- rock iter-parse    mag --iters 2000
//! ```
//!
//! The end-of-run PROFILE line reports a `loc/sec` figure that reuses
//! `count_source_stats` + `CompileStats::format_throughput`, so it's directly
//! comparable to `rock compile`'s throughput (and to the other loop). Both are
//! backend-independent: the front-end is shared by every backend, so they
//! ignore `--backend`.

use crate::Result;
use rock_core::{CompilerError, CompileStats, ModuleId, SourceFiles};
use rock_tokenizer::{count_source_stats, Token, Tokenizer};
use ustr::Ustr;
use std::time::{Duration, Instant};

fn discover(path: &str) -> Result<SourceFiles> {
    let spec = crate::common::path_to_source_spec(path);
    crate::unified_compile::discover_sources(&spec)
}

/// Source set for the parser A/B: a `stick.toml` project when one exists, else a
/// bare directory of `*.rock` files walked directly (each parsed independently —
/// parsing does no cross-file resolution, so the flat set is exactly the model
/// `skull-cook-diff` / `rock test --backend bone` use for the `tests/` corpus).
/// Without this, the A/B was confined to games (the only `stick.toml` projects),
/// leaving `tests/` + stdlib unverified.
fn discover_or_walk(path: &str) -> Result<SourceFiles> {
    use rock_core::source_file_from_path_and_contents;
    let p = std::path::Path::new(path);
    if p.is_file() {
        return discover(path);
    }
    if !p.is_dir() {
        anyhow::bail!("path does not exist: {path}");
    }
    if crate::StickConfig::find_from_dir(p)?.is_some() {
        return discover(path);
    }
    let mut paths = Vec::new();
    for entry in crate::WalkDir::new(p) {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("rock") {
            paths.push(entry.path().to_path_buf());
        }
    }
    paths.sort();
    let mut sources = SourceFiles::empty();
    for fp in &paths {
        let contents = fs_err::read_to_string(fp)?;
        let id = fp.to_string_lossy().into_owned();
        sources.add_or_update_file(source_file_from_path_and_contents(id.clone(), id, contents));
    }
    if sources.files.is_empty() {
        anyhow::bail!("no .rock files at {path}");
    }
    Ok(sources)
}

/// `rock iter-tokenize` -- tokenize every file N times, no parsing.
pub fn iter_tokenize(path: &str, iters: usize) -> Result<()> {
    let sources = discover(path)?;
    let iters = iters.max(1);

    // Warm-up pass outside the timed loop: surfaces any tokenizer error up
    // front, primes caches, and collects the per-file LoC stats for `loc/sec`.
    let mut stats = CompileStats::new();
    if let Err(e) = tokenize_all(&sources, Some(&mut stats)) {
        eprintln!("{}", e.render(&sources));
        crate::ablation_report::exit_with_census(1);
    }

    let t = Instant::now();
    for _ in 0..iters {
        if let Err(e) = tokenize_all(&sources, None) {
            eprintln!("{}", e.render(&sources));
            crate::ablation_report::exit_with_census(1);
        }
    }
    report("iter-tokenize", path, iters, &stats, t.elapsed());
    Ok(())
}

/// One tokenize pass over every source file. When `stats` is `Some`, merges each
/// file's `CompileStats` (LoC classification piggybacks on the tokens -- no
/// extra walk).
fn tokenize_all(
    sources: &SourceFiles,
    mut stats: Option<&mut CompileStats>,
) -> std::result::Result<(), CompilerError> {
    for file in &sources.files {
        let f = file.borrow();
        let mut tokenizer = Tokenizer::from_source_file(&f.contents, file);
        let tokens = tokenizer.tokenize()?;
        if let Some(stats) = stats.as_deref_mut() {
            stats.merge(&count_source_stats(&f.contents, &tokens));
        }
    }
    Ok(())
}

/// `rock iter-parse <project> --iters N` -- tokenize every file once, then time
/// the skull-parser parse loop over the cached token streams (tokenization is
/// outside the timed region). Backend-independent: the front end is shared.
pub fn iter_parse_skull(path: &str, iters: usize) -> Result<()> {
    let iters = iters.max(1);
    let (prepared, stats) = prepare_skull(path)?;

    // ---- timed parse loop (skull) ----
    let t = Instant::now();
    let mut parsed = 0usize;
    for _ in 0..iters {
        for (tokens, _, _, contents) in &prepared {
            if skull_parser::Parser::new(tokens.clone(), contents, 0).parse_module().is_ok() {
                parsed += 1;
            }
        }
    }
    let _ = parsed;
    report("iter-parse", path, iters, &stats, t.elapsed());
    Ok(())
}

/// A file's tokens plus the identity the two parsers need (skull keys only on
/// tokens; rock needs file id + module id; the span A/B needs the source text).
type SkullPrepared = Vec<(Vec<Token>, Ustr, ModuleId, String)>;

/// Tokenize every file under `path` ONCE (shared by both parsers, so the A/B
/// isolates the parser, not the lexer) and gather LoC stats. Tokenizer errors
/// are fatal — the corpus is expected to lex clean.
fn prepare_skull(path: &str) -> Result<(SkullPrepared, CompileStats)> {
    let sources = discover_or_walk(path)?;
    let mut stats = CompileStats::new();
    let mut prepared: SkullPrepared = Vec::with_capacity(sources.files.len());
    for file in &sources.files {
        let f = file.borrow();
        let mut tokenizer = Tokenizer::from_source_file(&f.contents, file);
        let tokens = match tokenizer.tokenize() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}", e.render(&sources));
                crate::ablation_report::exit_with_census(1);
            }
        };
        stats.merge(&count_source_stats(&f.contents, &tokens));
        let file_ustr: Ustr = Ustr::from(f.file.to_id_path().as_str());
        let module_id = f.module_id;
        let contents = f.contents.clone();
        drop(f);
        prepared.push((tokens, file_ustr, module_id, contents));
    }
    Ok((prepared, stats))
}

fn report(label: &str, path: &str, iters: usize, stats: &CompileStats, elapsed: Duration) {
    let per_iter = elapsed / iters as u32;
    let per_ms = per_iter.as_secs_f64() * 1000.0;
    log::info!(target: "PROFILE",
        "{} {}: {} iters, {} files, {} lines, {}, {:.4}ms/iter ({:.3}ms total)",
        label, path, iters, stats.files, stats.total_lines,
        stats.format_throughput(per_iter), per_ms, elapsed.as_secs_f64() * 1000.0);
}
