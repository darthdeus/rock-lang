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
pub fn hover(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;

    let mut bone = BoneWorkspace::new();
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

/// `rock-lsp diagnostics <file> --backend bone`.
pub fn diagnostics(file_path: &str) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;

    let mut bone = BoneWorkspace::new();
    bone.update(&file_path, src);

    let diags = bone.diagnostics_for(&file_path);
    let diags_json: Vec<serde_json::Value> = diags
        .iter()
        .map(|d| serde_json::to_value(d).unwrap_or(serde_json::Value::Null))
        .collect();

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "diagnostics": diags_json,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp definition <file:line:col> --backend bone`.
pub fn definition(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
    bone.update(&file_path, src);

    let loc = bone.definition(&file_path, Position::new(line - 1, column - 1));
    emit_definitions(&file_path, line, column, loc)
}

/// `rock-lsp type-definition <file:line:col> --backend bone`.
pub fn type_definition(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
    let definitions: Vec<serde_json::Value> = loc
        .into_iter()
        .map(|loc| {
            json!({
                "file": loc.uri.path().as_str(),
                "range": {
                    "start": { "line": loc.range.start.line, "character": loc.range.start.character },
                    "end":   { "line": loc.range.end.line,   "character": loc.range.end.character },
                },
            })
        })
        .collect();

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
pub fn completion(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
    bone.update(&file_path, src);

    let pos = Position::new(line - 1, column - 1);
    let items = bone.completion(&file_path, pos);

    let output = json!({
        "file": file_path,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
        "completions": serde_json::to_value(&items)?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// `rock-lsp references <file:line:col> --backend bone`.
pub fn references(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let mut bone = BoneWorkspace::new();
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
pub fn prepare_rename(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
pub fn rename(file_path: &str, line: u32, column: u32, new_name: &str) -> Result<()> {
    let file_path = canonical(file_path);
    let mut bone = BoneWorkspace::new();
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
pub fn signature_help(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
pub fn document_symbols(file_path: &str) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
pub fn hole_info(file_path: &str, line: u32, column: u32) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
pub fn workspace_symbols(file_path: &str, query: &str) -> Result<()> {
    let file_path = canonical(file_path);
    let mut bone = BoneWorkspace::new();
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
pub fn semantic_tokens(file_path: &str) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
pub fn inlay_hints(file_path: &str) -> Result<()> {
    let file_path = canonical(file_path);
    let src = fs_err::read_to_string(&file_path)?;
    let mut bone = BoneWorkspace::new();
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
