//! Bone-backed LSP CLI handlers (`--backend bone`), a thin one-shot shim over
//! the same facade the persistent server uses (`bone_query`).
//!
//! There is exactly one bone-consumption path: this builds a `BoneWorkspace`,
//! feeds it the single file, and calls the same query methods the server does,
//! so CLI and server can never drift. Output shapes mirror the Lua-backed CLI
//! handlers so downstream consumers don't care which backend produced them.
//!
//! Per `bone/CLAUDE.md` we never modify bone; this only consumes its public
//! pipeline (transitively, via the facade).

use anyhow::Result;
use lsp_types::{HoverContents, Position};
use bone_query::BoneWorkspace;
use serde_json::json;

/// `rock-lsp hover <file:line:col> --backend bone`.
pub fn hover(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;

    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    // CLI positions are 1-based; LSP/`Position` is 0-based.
    let pos = Position::new(line - 1, column - 1);

    let hover = bone.hover(&file_path, pos);
    let (contents, range) = match hover {
        Some(h) => {
            let text = match h.contents {
                HoverContents::Markup(m) => m.value,
                HoverContents::Scalar(s) => marked_string_text(s),
                HoverContents::Array(items) => items
                    .into_iter()
                    .map(marked_string_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            (text, h.range.map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null)))
        }
        None => {
            // Nothing resolves at the cursor. If the file doesn't type-check,
            // surface that as the hover body rather than a bare empty result.
            let diags = bone.diagnostics_for(&file_path);
            if diags.is_empty() {
                (String::new(), None)
            } else {
                let joined = diags
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                (format!("bone: file does not type-check\n\n{joined}"), None)
            }
        }
    };

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
        "hover": { "contents": contents, "range": range.unwrap_or(serde_json::Value::Null) },
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// A `BoneWorkspace` method that returns the diagnostics for one loaded file --
/// either all of them (`diagnostics_for`) or lints only (`lint_diagnostics`).
type DiagFn = fn(&BoneWorkspace, &str) -> Vec<lsp_types::Diagnostic>;

/// `rock-lsp diagnostics <path> --backend bone`. `path` may be a single file or
/// a directory (whole-project diagnostics -- see [`diagnostics_report`]).
pub fn diagnostics(path: &str) -> Result<()> {
    print_report(diagnostics_report(path)?)
}

/// `rock-lsp lint <path> --backend bone`. Lint findings only (`unused_var`,
/// `unused_fn`) -- no typecheck errors or holes. File or directory, like
/// `diagnostics`.
pub fn lint(path: &str) -> Result<()> {
    print_report(lint_report(path)?)
}

/// Print a diagnostics report, then fail when the directory walk could not read
/// some files: their diagnostics are missing, so the report is not a clean bill.
fn print_report(report: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&report)?);
    let unreadable: Vec<&str> = report["unreadable"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["file"].as_str())
        .collect();
    if !unreadable.is_empty() {
        anyhow::bail!("{} file(s) could not be read: {}", unreadable.len(), unreadable.join(", "));
    }
    Ok(())
}

/// All diagnostics JSON for a file or directory. Shared by the CLI handler and
/// tests. See [`report_with`] for the file/dir walk.
pub fn diagnostics_report(path: &str) -> Result<serde_json::Value> {
    report_with(path, BoneWorkspace::diagnostics_for, crate::CompilerPolicy::default())
}

/// Lint-only diagnostics JSON for a file or directory.
pub fn lint_report(path: &str) -> Result<serde_json::Value> {
    report_with(path, BoneWorkspace::lint_diagnostics, crate::CompilerPolicy::default())
}

pub fn diagnostics_with_policy(path: &str, policy: crate::CompilerPolicy) -> Result<()> {
    print_report(report_with(path, BoneWorkspace::diagnostics_for, policy)?)
}

pub fn lint_with_policy(path: &str, policy: crate::CompilerPolicy) -> Result<()> {
    print_report(report_with(path, BoneWorkspace::lint_diagnostics, policy)?)
}

/// Diagnostics JSON for a file or a directory, using `diag_fn` to select which
/// diagnostics to report. A directory walks every `.rock` under it, loads each
/// one's project once (whole-compilation-unit, so cross-file symbols and the
/// whole-project `unused_fn` lint resolve), and reports the files that have any,
/// plus an `unreadable` list naming each file it could not read.
fn report_with(path: &str, diag_fn: DiagFn, policy: crate::CompilerPolicy) -> Result<serde_json::Value> {
    if fs_err::metadata(path)?.is_dir() {
        return dir_report(path, diag_fn, policy);
    }
    let file_path = canonical(path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);
    Ok(json!({
        "file": file_path,
        "backend": "bone",
        "diagnostics": diags_to_json(diag_fn(&bone, &file_path)),
    }))
}

fn dir_report(dir: &str, diag_fn: DiagFn, policy: crate::CompilerPolicy) -> Result<serde_json::Value> {
    let mut files = rock_host::project::find_rock_files_at_path(dir)?;
    files.sort();

    let mut bone = policy.workspace();
    let mut unreadable = Vec::new();
    for f in &files {
        // Idempotent per project root: once a file's compilation unit is loaded,
        // its siblings already have buffer text, so skip the reload. A file with
        // no resolvable unit (standalone, no stick.toml) loads on its own.
        if bone.file_text(f).is_some() {
            continue;
        }
        bone.load_project(f);
        if bone.file_text(f).is_none() {
            match fs_err::read_to_string(f) {
                Ok(src) => {
                    bone.update(f, src);
                }
                Err(e) => unreadable.push(json!({ "file": f, "error": e.to_string() })),
            }
        }
    }

    let mut file_reports = Vec::new();
    let mut total = 0usize;
    for f in &files {
        let diags = diag_fn(&bone, f);
        if diags.is_empty() {
            continue;
        }
        total += diags.len();
        file_reports.push(json!({ "file": f, "diagnostics": diags_to_json(diags) }));
    }

    Ok(json!({
        "dir": canonical(dir),
        "backend": "bone",
        "file_count": files.len(),
        "diagnostic_count": total,
        "files": file_reports,
        "unreadable": unreadable,
    }))
}

fn diags_to_json(diags: Vec<lsp_types::Diagnostic>) -> Vec<serde_json::Value> {
    diags
        .iter()
        .map(|d| serde_json::to_value(d).unwrap_or(serde_json::Value::Null))
        .collect()
}

/// `rock-lsp definition <file:line:col> --backend bone`.
pub fn definition(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let loc = bone.definition(&file_path, Position::new(line - 1, column - 1));
    emit_definitions(&file_path, line, column, loc)
}

/// `rock-lsp type-definition <file:line:col> --backend bone`.
pub fn type_definition(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let loc = bone.type_definition(&file_path, Position::new(line - 1, column - 1));
    emit_definitions(&file_path, line, column, loc)
}

/// Print the `{definitions: [...]}` JSON shared by the `definition` and
/// `type-definition` handlers (both resolve a single optional target).
fn emit_definitions(
    file_path: &str,
    line: u32,
    column: u32,
    loc: Option<lsp_types::Location>,
) -> Result<()> {
    let definitions = loc
        .into_iter()
        .map(|loc| -> Result<serde_json::Value> {
            Ok(json!({
                "file": loc.uri.to_file_path()?,
                "range": {
                    "start": { "line": loc.range.start.line, "character": loc.range.start.character },
                    "end":   { "line": loc.range.end.line,   "character": loc.range.end.character },
                },
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
        "definitions": definitions,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp completion <file:line:col> --backend bone`.
pub fn completion(file_path: &str, line: u32, column: u32, explain: bool, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let pos = Position::new(line - 1, column - 1);
    let items = bone.completion(&file_path, pos);

    if explain {
        // Ranking breakdown for humans: server order with each item's RankKey
        // rungs decoded (rank_key.rs) -- a mis-rank reads as named fields, not
        // digit columns. Items without a decodable key print the raw string.
        for (i, it) in items.iter().enumerate() {
            let st = it.sort_text.as_deref().unwrap_or("");
            let ex = bone_query::explain_sort_text(st)
                .unwrap_or_else(|| format!("raw sort_text: {st}"));
            println!("{:3}. {:<40} {}", i + 1, it.label, ex);
        }
        return Ok(());
    }

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
        "completions": serde_json::to_value(&items)?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock lsp completion-matrix <file:line:col> --prefixes a,b,c
/// [--policies clean,subseq,subseq3] [--top N]`.
///
/// Tuning harness: at ONE cursor, splice each test prefix in place of the
/// identifier run ending there, run completion under each admission policy, and
/// print a side-by-side matrix (rank x policy, cells `label [bBAND]`). Lets you
/// eyeball what each policy admits/orders before picking a default. The real
/// project is loaded so cross-file symbols resolve like the live server.
pub fn completion_matrix(
    file_path: &str,
    line: u32,
    column: u32,
    prefixes: &[String],
    policies: &[bone_query::MatchPolicy],
    top: usize, policy: crate::CompilerPolicy) -> Result<()> {
    let mut probe = MatrixProbe::load(file_path, line, column, policies.to_vec(), top, policy)?;
    println!("{}\n", probe.header());
    for prefix in prefixes {
        println!("{}", probe.render(prefix));
    }
    Ok(())
}

/// `rock lsp completion <file:line:col> --repl [--policies ..] [--top N]`.
///
/// The completion tweaker: the project loads ONCE, then you type at the cursor
/// and the rank x policy matrix reprints. On a TTY this is per-keystroke (raw
/// mode); piped stdin falls back to a line-at-a-time loop (so scripts/tests
/// still work). Esc / Ctrl-C / Ctrl-D quit.
pub fn completion_repl(
    file_path: &str,
    line: u32,
    column: u32,
    policies: &[bone_query::MatchPolicy],
    top: usize, policy: crate::CompilerPolicy) -> Result<()> {
    use std::io::IsTerminal;
    let probe = MatrixProbe::load(file_path, line, column, policies.to_vec(), top, policy)?;
    if std::io::stdin().is_terminal() {
        completion_repl_raw(probe)
    } else {
        completion_repl_lines(probe)
    }
}

/// Piped/non-TTY fallback: one prefix per input line.
fn completion_repl_lines(mut probe: MatrixProbe) -> Result<()> {
    use std::io::{BufRead, Write};
    println!("{}", probe.header());
    println!("type a prefix, Enter to rank; blank line or `q` to quit.\n");
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    loop {
        print!("> ");
        std::io::stdout().flush().ok();
        let mut buf = String::new();
        if lock.read_line(&mut buf)? == 0 {
            break; // EOF
        }
        let prefix = buf.trim_end_matches(['\n', '\r']);
        if prefix.is_empty() || prefix == "q" {
            break;
        }
        println!("{}", probe.render(prefix));
    }
    Ok(())
}

/// Restores cooked mode + cursor on drop -- so a panic mid-loop can't leave the
/// user's shell in raw mode with a hidden cursor.
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Per-keystroke TTY loop: the current prefix's matrix is live at the top;
/// Enter commits it to a unique history stacked below (newest first, no dupes).
fn completion_repl_raw(mut probe: MatrixProbe) -> Result<()> {
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{size, Clear, ClearType};
    use crossterm::{cursor::MoveTo, queue};
    use std::io::Write;

    crossterm::terminal::enable_raw_mode()?;
    let _guard = RawGuard;
    queue!(std::io::stdout(), crossterm::cursor::Hide)?;
    let mut prefix = String::new();
    // Committed queries, newest first, each with its precomputed matrix block.
    // Results are fixed for the session, so we render a query once at commit.
    let mut history: Vec<(String, Vec<String>)> = Vec::new();

    // Build the full frame as logical lines, then paint flicker-free.
    let frame = |probe: &mut MatrixProbe, prefix: &str, history: &[(String, Vec<String>)]| {
        let mut lines = vec![
            probe.header(),
            "live -- type to edit; Enter saves to history; Esc / Ctrl-C / Ctrl-D quit".to_string(),
            String::new(),
            format!("query> {prefix}"),
        ];
        if !prefix.is_empty() {
            lines.extend(probe.render_lines(prefix));
        }
        if !history.is_empty() {
            lines.push(String::new());
            lines.push("--- history (unique, newest first) ---".to_string());
            for (_, block) in history {
                lines.push(String::new());
                lines.extend(block.iter().cloned());
            }
        }
        lines
    };

    let paint = |lines: &[String]| -> Result<()> {
        // Unknown or zero height (e.g. a pty with no winsize) must NOT clamp to
        // zero rows -- treat it as unbounded so the frame still renders.
        let rows = match size() {
            Ok((_, h)) if h > 0 => h as usize,
            _ => usize::MAX,
        };
        let mut out = std::io::stdout();
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, 0))?;
        // Clear each line to EOL as we overwrite it (no full-screen wipe = no
        // flash), then clear anything left below the new frame.
        for line in lines.iter().take(rows) {
            buf.extend_from_slice(line.as_bytes());
            queue!(buf, Clear(ClearType::UntilNewLine))?;
            buf.extend_from_slice(b"\r\n");
        }
        queue!(buf, Clear(ClearType::FromCursorDown))?;
        out.write_all(&buf)?;
        out.flush()?;
        Ok(())
    };

    paint(&frame(&mut probe, &prefix, &history))?;
    loop {
        let Event::Key(k) = read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue; // ignore key-release (Windows) / repeat duplication
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => break,
            KeyCode::Char('c' | 'd') if ctrl => break,
            KeyCode::Enter => {
                if !prefix.is_empty() {
                    // Unique + move-to-front: a re-query bumps its old entry
                    // instead of stacking a duplicate.
                    history.retain(|(p, _)| p != &prefix);
                    let block = probe.render_lines(&prefix);
                    history.insert(0, (std::mem::take(&mut prefix), block));
                }
            }
            KeyCode::Backspace => {
                prefix.pop();
            }
            KeyCode::Char(c) if !c.is_control() => prefix.push(c),
            _ => continue, // arrows etc. -- no state change
        }
        paint(&frame(&mut probe, &prefix, &history))?;
    }
    Ok(())
}

/// One cursor prepared for repeated "what if I typed <prefix>" probes: the file
/// text with the trailing identifier removed and the project loaded, so each
/// `render` is just a splice + per-policy completion (no reload).
struct MatrixProbe {
    bone: BoneWorkspace,
    file_path: String,
    /// Source with the identifier run at the cursor already removed.
    head: String,
    tail: String,
    orig_ident: String,
    line: u32,
    column: u32,
    policies: Vec<bone_query::MatchPolicy>,
    top: usize,
}

impl MatrixProbe {
    fn load(
        file_path: &str,
        line: u32,
        column: u32,
        policies: Vec<bone_query::MatchPolicy>,
        top: usize,
        policy: crate::CompilerPolicy,
    ) -> Result<MatrixProbe> {
        let file_path = canonical(file_path);
        let src = fs_err::read_to_string(&file_path)?;
        // Walk back over the identifier run ending at the cursor -- that run is
        // what each probed prefix replaces.
        let cur = line_col_to_offset(&src, line, column);
        let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut start = cur;
        while start > 0 && is_ident(src.as_bytes()[start - 1]) {
            start -= 1;
        }
        let mut bone = policy.workspace();
        bone.load_project(&file_path);
        Ok(MatrixProbe {
            bone,
            head: src[..start].to_string(),
            tail: src[cur..].to_string(),
            orig_ident: src[start..cur].to_string(),
            file_path,
            line,
            column,
            policies,
            top,
        })
    }

    fn header(&self) -> String {
        format!(
            "cursor {}:{}  identifier at cursor: {:?}  policies: [{}]  top {}",
            self.line,
            self.column,
            self.orig_ident,
            self.policies.iter().map(|p| p.name()).collect::<Vec<_>>().join(", "),
            self.top,
        )
    }

    /// Splice `prefix` at the cursor, run completion under each policy, and
    /// format the rank x policy matrix (band-tagged cells) as one string.
    fn render(&mut self, prefix: &str) -> String {
        self.render_lines(prefix).join("\n")
    }

    /// The matrix as logical lines (no trailing newlines) -- the raw TTY painter
    /// needs lines to clear each to EOL for flicker-free redraw.
    fn render_lines(&mut self, prefix: &str) -> Vec<String> {
        let spliced = format!("{}{}{}", self.head, prefix, self.tail);
        let new_off = self.head.len() + prefix.len();
        let pos = bone_query::offset_to_position(&spliced, new_off);
        self.bone.update(&self.file_path, spliced);

        let cols: Vec<Vec<String>> = self
            .policies
            .iter()
            .map(|&p| {
                bone_query::set_match_policy(p);
                self.bone
                    .completion(&self.file_path, pos)
                    .into_iter()
                    .take(self.top)
                    .map(|it| {
                        let band = it
                            .sort_text
                            .as_deref()
                            .filter(|st| st.len() >= 18 && st.as_bytes()[..18].iter().all(u8::is_ascii_digit))
                            .map_or('?', |st| st.as_bytes()[2] as char);
                        format!("{} [b{band}]", it.label)
                    })
                    .collect()
            })
            .collect();
        bone_query::set_match_policy(bone_query::MatchPolicy::default_policy());

        let width = 46usize;
        let mut lines = vec![format!(
            "=== prefix {:?}  (cursor -> {}:{})",
            prefix,
            pos.line + 1,
            pos.character + 1
        )];
        let hdr: Vec<String> = self
            .policies
            .iter()
            .map(|p| format!("{:<width$}", p.name(), width = width))
            .collect();
        lines.push(format!("     {}", hdr.join("")));
        let rows = cols.iter().map(Vec::len).max().unwrap_or(0);
        for r in 0..rows {
            let cells: Vec<String> = cols
                .iter()
                .map(|c| {
                    let s = c.get(r).map_or("", String::as_str);
                    let s: String = s.chars().take(width - 2).collect();
                    format!("{s:<width$}", width = width)
                })
                .collect();
            lines.push(format!("{:>3}. {}", r + 1, cells.join("")));
        }
        lines
    }
}

/// Absolute byte offset of a 1-based (line, column) in `src`. Columns are byte
/// columns; identifiers are ASCII so this matches the LSP char columns here.
fn line_col_to_offset(src: &str, line: u32, column: u32) -> usize {
    let mut off = 0usize;
    for (i, l) in src.split_inclusive('\n').enumerate() {
        if i as u32 + 1 == line {
            return (off + (column as usize).saturating_sub(1)).min(src.len());
        }
        off += l.len();
    }
    off.min(src.len())
}

/// `rock-lsp references <file:line:col> --backend bone`.
pub fn references(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let mut bone = policy.workspace();
    // Load the whole project so cross-file references are found.
    bone.load_project(&file_path);

    let pos = Position::new(line - 1, column - 1);
    let locs = bone.references(&file_path, pos);

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
        "references": serde_json::to_value(&locs)?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp prepare-rename <file:line:col> --backend bone`.
pub fn prepare_rename(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let pos = Position::new(line - 1, column - 1);
    let range = bone.prepare_rename(&file_path, pos).map(|r| {
        json!({
            "start": { "line": r.start.line, "character": r.start.character },
            "end":   { "line": r.end.line,   "character": r.end.character },
        })
    });

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "range": range.unwrap_or(serde_json::Value::Null),
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp rename <file:line:col> <new_name> --backend bone`.
pub fn rename(file_path: &str, line: u32, column: u32, new_name: &str, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let mut bone = policy.workspace();
    // Load the whole project so rename rewrites cross-file references too.
    bone.load_project(&file_path);

    let pos = Position::new(line - 1, column - 1);
    let mut changes = serde_json::Map::new();
    if let Some(edit) = bone.rename(&file_path, pos, new_name) {
        if let Some(ch) = edit.changes {
            for (uri, edits) in ch {
                // Shape matches the harness applier: { range, new_text }.
                let arr: Vec<serde_json::Value> = edits
                    .iter()
                    .map(|e| {
                        json!({
                            "range": {
                                "start": { "line": e.range.start.line, "character": e.range.start.character },
                                "end":   { "line": e.range.end.line,   "character": e.range.end.character },
                            },
                            "new_text": e.new_text,
                        })
                    })
                    .collect();
                changes.insert(uri.to_string(), json!(arr));
            }
        }
    }

    let output = json!({ "backend": "bone", "changes": changes });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp signature-help <file:line:col> --backend bone`.
pub fn signature_help(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let pos = Position::new(line - 1, column - 1);
    match bone.signature_help(&file_path, pos) {
        // Match the lua handler: serialize the SignatureHelp directly.
        Some(sh) => println!("{}", serde_json::to_string_pretty(&sh)?),
        None => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "signatures": [],
                "activeSignature": serde_json::Value::Null,
                "activeParameter": serde_json::Value::Null,
            }))?
        ),
    }
    Ok(())
}

/// `rock-lsp document-symbols <file> --backend bone`.
pub fn document_symbols(file_path: &str, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let symbols = bone.document_symbols(&file_path);
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "symbols": serde_json::to_value(&symbols)?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp hole-info <file:line:col> --backend bone`. Type holes only for now
/// (bone rejects expression holes at lowering); prints nothing and exits 1 when
/// the cursor isn't on a hole, matching the lua handler.
pub fn hole_info(file_path: &str, line: u32, column: u32, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let pos = Position::new(line - 1, column - 1);
    match bone.hole_info(&file_path, pos) {
        Some(report) => {
            print!("{report}");
            Ok(())
        }
        None => {
            eprintln!("No hole at {file_path}:{line}:{column}");
            std::process::exit(1);
        }
    }
}

/// `rock-lsp workspace-symbols <file> <query> --backend bone`.
pub fn workspace_symbols(file_path: &str, query: &str, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let mut bone = policy.workspace();
    // Load the whole project so symbols from every user file are searched.
    bone.load_project(&file_path);

    let symbols = bone.workspace_symbols(query);
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "query": query,
        "symbols": serde_json::to_value(&symbols)?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp semantic-tokens <file> --backend bone`.
pub fn semantic_tokens(file_path: &str, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let tokens = bone.semantic_tokens(&file_path);
    let data: Vec<u32> = tokens
        .data
        .iter()
        .flat_map(|t| {
            [
                t.delta_line,
                t.delta_start,
                t.length,
                t.token_type,
                t.token_modifiers_bitset,
            ]
        })
        .collect();
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "count": tokens.data.len(),
        "data": data,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp inlay-hints <file> --backend bone`. Hints over the whole file.
pub fn inlay_hints(file_path: &str, policy: crate::CompilerPolicy) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = policy.workspace();
    bone.update(&file_path, src);

    let full = lsp_types::Range {
        start: Position::new(0, 0),
        end: Position::new(u32::MAX, 0),
    };
    let hints = bone.inlay_hints(&file_path, full);
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "hints": serde_json::to_value(&hints)?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn marked_string_text(s: lsp_types::MarkedString) -> String {
    match s {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::LanguageString(ls) => ls.value,
    }
}

fn canonical(file_path: &str) -> String {
    // Strip Windows' `\\?\` verbatim prefix so this matches the module keys
    // bone-query builds; the bare `fs_err::canonicalize` form won't string-match
    // them and breaks goto/hover/etc. resolution on Windows.
    bone_query::canonicalize_lenient(file_path)
}
