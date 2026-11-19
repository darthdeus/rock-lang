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
            // If the file does not type-check, surface that as the hover body.
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

type DiagFn = fn(&BoneWorkspace, &str) -> Vec<lsp_types::Diagnostic>;

pub fn diagnostics(path: &str) -> Result<()> {
    print_report(diagnostics_report(path)?)
}

pub fn lint(path: &str) -> Result<()> {
    print_report(lint_report(path)?)
}

/// Fail when some files were unreadable: their diagnostics are missing.
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

pub fn diagnostics_report(path: &str) -> Result<serde_json::Value> {
    report_with(path, BoneWorkspace::diagnostics_for)
}

pub fn lint_report(path: &str) -> Result<serde_json::Value> {
    report_with(path, BoneWorkspace::lint_diagnostics)
}

/// Each project loads once as a whole compilation unit so cross-file symbols
/// and the `unused_fn` lint resolve.
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
        // Once a file's compilation unit is loaded, its siblings already have buffer text.
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

pub fn definition(file_path: &str, line: u32, column: u32) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    let loc = bone.definition(&file_path, Position::new(line - 1, column - 1));
    emit_definitions(&file_path, line, column, loc)
}

pub fn type_definition(file_path: &str, line: u32, column: u32) -> Result<()> {
    let (file_path, bone) = open_file(file_path)?;

    let loc = bone.type_definition(&file_path, Position::new(line - 1, column - 1));
    emit_definitions(&file_path, line, column, loc)
}

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

pub fn completion(file_path: &str, line: u32, column: u32, explain: bool) -> Result<()> {
    let (file_path, mut bone) = open_file(file_path)?;

    let pos = Position::new(line - 1, column - 1);
    let items = bone.completion(&file_path, pos);

    if explain {
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

pub fn references(file_path: &str, line: u32, column: u32) -> Result<()> {
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

pub fn rename(file_path: &str, line: u32, column: u32, new_name: &str) -> Result<()> {
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

pub fn workspace_symbols(file_path: &str, query: &str) -> Result<()> {
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

fn open_file(file_path: &str) -> Result<(String, BoneWorkspace)> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
    bone.update(&file_path, src);
    Ok((file_path, bone))
}

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
    // Strip Windows' `\\?\` verbatim prefix so this matches bone-query's module keys.
    bone_query::canonicalize_lenient(file_path)
}
