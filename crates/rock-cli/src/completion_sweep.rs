//! `rock completion-sweep <project> [--stride N]` -- deterministic completion
//! output over a derived corpus of cursor positions, for BYTE-IDENTITY gating
//! of completion refactors (the compiler contract, M0). Workflow: dump
//! to a file before a change, dump again after, `diff`. Any diff is either a
//! bug or a corpus-pinned intentional change.
//!
//! Positions are derived deterministically from each source file:
//!   - the END of every identifier run (completing that identifier as the
//!     typed prefix -- covers member access, call args, assignments, type
//!     slots, whatever the identifier sits in), and
//!   - right AFTER every `.` that follows an identifier char (empty-prefix
//!     member completion).
//! `--stride N` keeps every Nth position (default 10) so the corpus stays
//! diffable-sized; stride 1 is the exhaustive gate.
//!
//! Output (stdout; one block per position):
//!   === <relpath>:<line>:<col> prefix="<p>"
//!   <label>\t<kind>\t<sortText>\t<insertText>\t<fmt>\t<detail>
//! Fields with embedded tabs/newlines are escaped; absent fields print `-`.
//! Paths are project-relative so dumps diff across checkouts.

use anyhow::Result;
use bone_query::{BoneWorkspace, offset_to_position};
use std::io::Write;
use std::path::Path;

/// Byte offsets to probe in `text`: identifier-run ends + after-dot member
/// positions, ascending, deduped, then strided.
fn probe_offsets(text: &str, stride: usize) -> Vec<usize> {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if ident(b[i]) {
            let start = i;
            while i < b.len() && ident(b[i]) {
                i += 1;
            }
            // Skip pure-number runs; completing `42|` is not a case we rank.
            if !b[start].is_ascii_digit() {
                out.push(i);
            }
        } else {
            if b[i] == b'.' && i > 0 && ident(b[i - 1]) {
                out.push(i + 1);
            }
            i += 1;
        }
    }
    out.dedup();
    let stride = stride.max(1);
    out.into_iter().step_by(stride).collect()
}

/// The typed prefix at `offset` (the identifier run ending there), for the
/// human-readable block header.
fn prefix_at(text: &str, offset: usize) -> &str {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut s = offset.min(b.len());
    while s > 0 && ident(b[s - 1]) {
        s -= 1;
    }
    &text[s..offset.min(b.len())]
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn field(s: Option<&str>) -> String {
    s.map(esc).unwrap_or_else(|| "-".to_string())
}

pub fn completion_sweep(path: &str, stride: usize, check: bool) -> Result<()> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        anyhow::bail!("completion-sweep expects a project directory, got `{path}`");
    }
    let files = crate::iter_completion::collect_rock_files(dir);
    if files.is_empty() {
        anyhow::bail!("no .rock files under `{path}`");
    }
    let root = fs_err::canonicalize(dir)?;

    let mut bone = BoneWorkspace::new();
    let first = fs_err::canonicalize(&files[0])?
        .to_string_lossy()
        .to_string();
    bone.load_project(&first);

    // Buffer stdout: the dump is tens of MB of small writes at stride 1.
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let mut n_positions = 0usize;
    let mut n_items = 0usize;
    let mut n_violations = 0usize;

    for f in &files {
        let abs = fs_err::canonicalize(f)?;
        let rel = abs
            .strip_prefix(&root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .to_string();
        let abs_str = abs.to_string_lossy().to_string();
        let text = fs_err::read_to_string(&abs)?;
        bone.update(&abs_str, text.clone());
        for off in probe_offsets(&text, stride) {
            let pos = offset_to_position(&text, off);
            let items = bone.completion(&abs_str, pos);
            n_positions += 1;
            n_items += items.len();
            // `--check`: enforce the position-blind response invariants
            // (the compiler contract M4) at every swept position --
            // the real-project rung of the invariant suite.
            if check {
                let violations = bone_query::invariants::completion_response_violations(
                    &items,
                    prefix_at(&text, off),
                );
                for v in &violations {
                    eprintln!(
                        "INVARIANT {rel}:{}:{} prefix=\"{}\": {v}",
                        pos.line + 1,
                        pos.character + 1,
                        esc(prefix_at(&text, off)),
                    );
                }
                n_violations += violations.len();
            }
            writeln!(
                out,
                "=== {rel}:{}:{} prefix=\"{}\"",
                pos.line + 1,
                pos.character + 1,
                esc(prefix_at(&text, off)),
            )?;
            for it in items {
                let fmt = match it.insert_text_format {
                    Some(lsp_types::InsertTextFormat::SNIPPET) => "S",
                    _ => "-",
                };
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    esc(&it.label),
                    it.kind
                        .map(|k| format!("{k:?}"))
                        .unwrap_or_else(|| "-".to_string()),
                    field(it.sort_text.as_deref()),
                    field(it.insert_text.as_deref()),
                    fmt,
                    field(it.detail.as_deref()),
                )?;
            }
        }
    }
    out.flush()?;
    eprintln!(
        "completion-sweep: {} files, {n_positions} positions (stride {stride}), {n_items} items",
        files.len()
    );
    if check {
        if n_violations > 0 {
            anyhow::bail!("completion-sweep --check: {n_violations} invariant violations");
        }
        eprintln!("completion-sweep --check: invariants hold at all {n_positions} positions");
    }
    Ok(())
}
