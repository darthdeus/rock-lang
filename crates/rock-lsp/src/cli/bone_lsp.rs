//! One-shot CLI queries over the same workspace used by the LSP server.

use anyhow::Result;
use bone_query::{BoneWorkspace, FileUriExt};
use lsp_types::{HoverContents, MarkedString, Position};
use serde_json::json;

fn marked_string(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(block) => {
            format!("```{}\n{}\n```", block.language, block.value)
        }
    }
}

/// `rock lsp hover <file:line:col>`.
pub fn hover(file_path: &str, line: u32, column: u32) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    // CLI positions are 1-based; LSP/`Position` is 0-based.
    let pos = Position::new(line - 1, column - 1);

    let hover = bone.hover(&file_path, pos);
    let (contents, range) = match hover {
        Some(h) => {
            let text = match h.contents {
                HoverContents::Markup(markup) => markup.value,
                HoverContents::Scalar(marked) => marked_string(marked),
                HoverContents::Array(marked) => marked
                    .into_iter()
                    .map(marked_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            (
                text,
                h.range
                    .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null)),
            )
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
    print_json(&output)?;
    Ok(())
}

/// A `BoneWorkspace` method that returns the diagnostics for one loaded file --
/// either all of them (`diagnostics_for`) or lints only (`lint_diagnostics`).
type DiagFn = fn(&BoneWorkspace, &str) -> Vec<lsp_types::Diagnostic>;

/// `rock lsp diagnostics <path>`. `path` may be a single file or
/// a directory (whole-project diagnostics -- see [`diagnostics_report`]).
pub fn diagnostics(path: &str) -> Result<()> {
    print_report(diagnostics_report(path)?)
}

/// `rock lsp lint <path>`. Lint findings only (`unused_var`,
/// `unused_fn`) -- no typecheck errors or holes. File or directory, like
/// `diagnostics`.
pub fn lint(path: &str) -> Result<()> {
    print_report(lint_report(path)?)
}

/// Print a diagnostics report, then fail when the directory walk could not read
/// some files: their diagnostics are missing, so the report is not a clean bill.
fn print_report(report: serde_json::Value) -> Result<()> {
    print_json(&report)?;
    let unreadable: Vec<&str> = report["unreadable"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["file"].as_str())
        .collect();
    if !unreadable.is_empty() {
        anyhow::bail!(
            "{} file(s) could not be read: {}",
            unreadable.len(),
            unreadable.join(", ")
        );
    }
    Ok(())
}

/// All diagnostics JSON for a file or directory. Shared by the CLI handler and
/// tests. See [`report_with`] for the file/dir walk.
pub fn diagnostics_report(path: &str) -> Result<serde_json::Value> {
    report_with(path, BoneWorkspace::diagnostics_for)
}

/// Lint-only diagnostics JSON for a file or directory.
pub fn lint_report(path: &str) -> Result<serde_json::Value> {
    report_with(path, BoneWorkspace::lint_diagnostics)
}

/// Diagnostics JSON for a file or a directory, using `diag_fn` to select which
/// diagnostics to report. A directory walks every `.rock` under it, loads each
/// one's project once (whole-compilation-unit, so cross-file symbols and the
/// whole-project `unused_fn` lint resolve), and reports the files that have any,
/// plus an `unreadable` list naming each file it could not read.
fn report_with(path: &str, diag_fn: DiagFn) -> Result<serde_json::Value> {
    if fs_err::metadata(path)?.is_dir() {
        return dir_report(path, diag_fn);
    }
    let (file_path, bone) = open_file(path)?;
    Ok(json!({
        "file": file_path,
        "backend": "bone",
        "diagnostics": diags_to_json(diag_fn(&bone, &file_path)),
    }))
}

fn dir_report(dir: &str, diag_fn: DiagFn) -> Result<serde_json::Value> {
    let mut files = rock_host::project::find_rock_files_at_path(dir)?;
    files.sort();

    let mut bone = BoneWorkspace::new();
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

    // Hole diagnostics reparse each file, so the per-file queries run on the pool.
    use bone_core::prelude::*;
    let per_file: Vec<Vec<lsp_types::Diagnostic>> =
        files.par_iter().map(|f| diag_fn(&bone, f)).collect();
    let mut file_reports = Vec::new();
    let mut total = 0usize;
    for (f, diags) in files.iter().zip(per_file) {
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

/// `rock lsp definition <file:line:col>`.
pub fn definition(file_path: &str, line: u32, column: u32) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    let loc = bone.definition(&file_path, Position::new(line - 1, column - 1));
    emit_definitions(&file_path, line, column, loc)
}

/// `rock lsp type-definition <file:line:col>`.
pub fn type_definition(file_path: &str, line: u32, column: u32) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

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
    print_json(&output)?;
    Ok(())
}

/// `rock lsp completion <file:line:col>`.
pub fn completion(file_path: &str, line: u32, column: u32, explain: bool) -> Result<()> {
    let (file_path, mut bone) = open_file(file_path)?;

    let pos = Position::new(line - 1, column - 1);
    let items = bone.completion(&file_path, pos);

    if explain {
        // Ranking breakdown for humans: server order with each item's RankKey
        // rungs decoded (rank_key.rs) -- a mis-rank reads as named fields, not
        // digit columns. Items without a decodable key print the raw string.
        for (i, it) in items.iter().enumerate() {
            let st = it.sort_text.as_deref().unwrap_or("");
            let ex =
                bone_query::explain_sort_text(st).unwrap_or_else(|| format!("raw sort_text: {st}"));
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
    print_json(&output)?;
    Ok(())
}

/// `rock lsp references <file:line:col>`.
pub fn references(file_path: &str, line: u32, column: u32) -> Result<()> {
    // Load the whole project so cross-file references are found.
    let (file_path, bone) = open_project(file_path);

    let pos = Position::new(line - 1, column - 1);
    let locs = bone.references(&file_path, pos);

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
        "references": serde_json::to_value(&locs)?,
    });
    print_json(&output)?;
    Ok(())
}

/// `rock lsp rename <file:line:col> <new_name>`.
pub fn rename(file_path: &str, line: u32, column: u32, new_name: &str) -> Result<()> {
    // Load the whole project so rename rewrites cross-file references too.
    let (file_path, bone) = open_project(file_path);

    let pos = Position::new(line - 1, column - 1);
    let mut changes = serde_json::Map::new();
    let renamed = bone.rename(&file_path, pos, new_name);
    let refusal = renamed.as_ref().err().map(|r| r.to_string());
    if let Some(edit) = renamed.ok().flatten() {
        if let Some(ch) = edit.changes {
            for (uri, edits) in ch {
                // Shape matches the harness applier: { range, new_text }.
                let arr: Vec<serde_json::Value> = edits
                    .iter()
                    .map(|e| {
                        json!({
                            "range": e.range,
                            "new_text": e.new_text,
                        })
                    })
                    .collect();
                changes.insert(uri.to_string(), json!(arr));
            }
        }
    }

    let mut output = json!({ "backend": "bone", "changes": changes });
    if let Some(refusal) = refusal {
        output["error"] = json!(refusal);
    }
    print_json(&output)?;
    Ok(())
}

/// `rock lsp signature-help <file:line:col>`.
pub fn signature_help(file_path: &str, line: u32, column: u32) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    let pos = Position::new(line - 1, column - 1);
    match bone.signature_help(&file_path, pos) {
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

/// `rock lsp document-symbols <file>`.
pub fn document_symbols(file_path: &str) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    let symbols = bone.document_symbols(&file_path);
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "symbols": serde_json::to_value(&symbols)?,
    });
    print_json(&output)?;
    Ok(())
}

/// `rock lsp workspace-symbols <file> <query>`.
pub fn workspace_symbols(file_path: &str, query: &str) -> Result<()> {
    // Load the whole project so symbols from every user file are searched.
    let (file_path, bone) = open_project(file_path);

    let symbols = bone.workspace_symbols(query);
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "query": query,
        "symbols": serde_json::to_value(&symbols)?,
    });
    print_json(&output)?;
    Ok(())
}

/// `rock lsp semantic-tokens <file>`.
pub fn semantic_tokens(file_path: &str) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    let tokens = bone.semantic_tokens(&file_path);
    let output = json!({
        "file": file_path,
        "backend": "bone",
        "count": tokens.data.len(),
        "data": serde_json::to_value(&tokens)?["data"],
    });
    print_json(&output)?;
    Ok(())
}

/// `rock lsp inlay-hints <file>`. Hints over the whole file.
pub fn inlay_hints(file_path: &str) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

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
    print_json(&output)?;
    Ok(())
}

/// Canonicalize `file_path` and open just that file on a fresh workspace.
fn open_file(file_path: &str) -> Result<(String, BoneWorkspace)> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
    bone.update(&file_path, src);
    Ok((file_path, bone))
}

/// Canonicalize `file_path` and load its whole project, so cross-file
/// queries see every file.
fn open_project(file_path: &str) -> (String, BoneWorkspace) {
    let file_path = canonical(file_path);
    let mut bone = BoneWorkspace::new();
    bone.load_project(&file_path);
    (file_path, bone)
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn canonical(file_path: &str) -> String {
    // Strip Windows' `\\?\` verbatim prefix so this matches the module keys
    // bone-query builds; the bare `fs_err::canonicalize` form won't string-match
    // them and breaks goto/hover/etc. resolution on Windows.
    bone_query::canonicalize_lenient(file_path)
}
