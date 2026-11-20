//! `rock lsp <subcommand>`: the server, and one-shot JSON queries over the same
//! workspace it uses. Invoked from `rock-cli` after it strips `lsp`.

use anyhow::{Result, anyhow};
use bone_query::{BoneWorkspace, FileUriExt};
use clap::{Arg, ArgAction, ArgMatches, Command};
use lsp_types::{HoverContents, MarkedString, Position};
use serde_json::{Value, json};

const FILE_COMMANDS: &[(&str, &str)] = &[
    ("diagnostics", "Get diagnostics for a Rock file"),
    (
        "lint",
        "Run lints (unused_var, unused_fn) over a Rock file or directory",
    ),
    ("semantic-tokens", "Get semantic tokens for a Rock file"),
    ("inlay-hints", "Get inlay hints for a Rock file"),
    ("document-symbols", "Get document symbols for a Rock file"),
];

const POSITION_COMMANDS: &[(&str, &str)] = &[
    ("hover", "Get hover information at a specific position"),
    ("completion", "Get completions at a specific position"),
    ("definition", "Go to definition at a specific position"),
    (
        "type-definition",
        "Go to the declaration of the type of the value at a specific position",
    ),
    ("references", "Find references at a specific position"),
    ("rename", "Rename symbol at a specific position"),
    (
        "signature-help",
        "Get signature help at a specific position",
    ),
];

fn build_cli() -> Command {
    let file_arg = || {
        Arg::new("file")
            .required(true)
            .help("The Rock file to analyze")
    };
    let position_arg = || {
        Arg::new("position")
            .required(true)
            .help("The position in format file:line:column (1-indexed)")
    };
    let mut cli = Command::new("rock lsp")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Rock LSP server and CLI tools")
        .subcommand(
            Command::new("serve").about("Start the LSP server (default if no command specified)"),
        )
        .subcommand(
            Command::new("workspace-symbols")
                .about("Search for symbols in the workspace")
                .arg(file_arg().help("The Rock file to analyze (acts as workspace root)"))
                .arg(Arg::new("query").required(true).help("The search query")),
        );
    for (name, about) in FILE_COMMANDS {
        cli = cli.subcommand(Command::new(*name).about(*about).arg(file_arg()));
    }
    for (name, about) in POSITION_COMMANDS {
        let mut command = Command::new(*name).about(*about).arg(position_arg());
        match *name {
            "completion" => {
                command = command.arg(
                    Arg::new("explain")
                        .long("explain")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Human-readable ranking breakdown: one line per item with the decoded \
                     RankKey rungs (exact/type/band/case/kind/dist/score) instead of JSON",
                        ),
                )
            }
            "rename" => {
                command = command.arg(
                    Arg::new("new_name")
                        .required(true)
                        .help("The new name for the symbol"),
                )
            }
            _ => {}
        }
        cli = cli.subcommand(command);
    }
    cli
}

fn arg_str<'a>(m: &'a ArgMatches, name: &str) -> &'a str {
    m.get_one::<String>(name).map(|s| s.as_str()).unwrap_or("")
}

/// Parse a position string in format "file:line:column" (1-indexed).
fn parse_position(position: &str) -> Result<(String, u32, u32)> {
    let parts: Vec<&str> = position.rsplitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(anyhow!(
            "Invalid position format. Expected 'file:line:column'"
        ));
    }
    let column: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow!("Invalid column number: {}", parts[0]))?;
    let line: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow!("Invalid line number: {}", parts[1]))?;
    if line == 0 || column == 0 {
        return Err(anyhow!(
            "Line and column numbers must be 1-indexed (got line={line}, column={column})"
        ));
    }
    Ok((parts[2].to_string(), line, column))
}

fn default_debug_file() -> std::path::PathBuf {
    #[cfg(windows)]
    return std::env::temp_dir().join("lsp_debug.log");
    #[cfg(not(windows))]
    std::path::PathBuf::from("/tmp/lsp_debug.log")
}

fn open_debug_log(log_path: &str) -> Result<fs_err::File> {
    fs_err::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| anyhow!("ROCK_LSP_DEBUG: cannot open {log_path}: {e}"))
}

/// Warn floor refined by `RUST_LOG`; an unparsable `RUST_LOG` is an error.
fn stderr_log_filter(rust_log: Option<&str>) -> Result<rock_log::Filter> {
    rock_log::Filter::parse(log::LevelFilter::Warn, rust_log, &[]).map_err(|e| {
        anyhow!(
            "RUST_LOG={:?} is not a valid log filter: {e}",
            rust_log.unwrap_or_default()
        )
    })
}

/// Run a `rock lsp` subcommand; `args[0]` is a display name. Callers must not have
/// installed a global logger first.
pub fn run_cli(args: Vec<String>) -> Result<()> {
    // ROCK_LSP_DEBUG=1 -> lsp_debug.log in the temp dir; a path -> that path at trace
    // level; unset -> stderr at warn (or RUST_LOG).
    match std::env::var("ROCK_LSP_DEBUG") {
        Ok(val) => {
            let log_path = if val == "1" || val.is_empty() {
                default_debug_file().to_string_lossy().into_owned()
            } else {
                val
            };
            let file = open_debug_log(&log_path)?;
            let filter = rock_log::Filter::parse(log::LevelFilter::Trace, None, &[])
                .map_err(|e| anyhow!("ROCK_LSP_DEBUG filter: {e}"))?;
            rock_log::init_filtered(filter, rock_log::Sink::File(file.into_parts().0));
            log::trace!("LSP_MAIN_STARTED (logging to {log_path})");
        }
        Err(_) => {
            let rust_log = std::env::var("RUST_LOG").ok();
            let filter = stderr_log_filter(rust_log.as_deref())?;
            rock_log::init_filtered(filter, rock_log::Sink::Stderr);
        }
    }

    let matches = build_cli().get_matches_from(args);
    let report = match matches.subcommand() {
        None | Some(("serve", _)) => return crate::run_lsp_server(),
        Some(("diagnostics", m)) => return print_report(diagnostics_report(arg_str(m, "file"))?),
        Some(("lint", m)) => return print_report(lint_report(arg_str(m, "file"))?),
        Some(("workspace-symbols", m)) => {
            let (file, bone) = open_project(arg_str(m, "file"));
            let query = arg_str(m, "query");
            json!({
                "file": file,
                "backend": "bone",
                "query": query,
                "symbols": serde_json::to_value(bone.workspace_symbols(query))?,
            })
        }
        Some((name, m)) if POSITION_COMMANDS.iter().any(|(n, _)| *n == name) => {
            position_query(name, m)?
        }
        Some((name, m)) => file_query(name, arg_str(m, "file"))?,
    };
    if !report.is_null() {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

fn file_query(name: &str, path: &str) -> Result<Value> {
    let (file, bone) = open_file(path)?;
    let mut report = json!({ "file": file, "backend": "bone" });
    match name {
        "semantic-tokens" => {
            let tokens = bone.semantic_tokens(&file);
            report["count"] = json!(tokens.data.len());
            report["data"] = serde_json::to_value(&tokens)?["data"].take();
        }
        "inlay-hints" => {
            let full = lsp_types::Range::new(Position::new(0, 0), Position::new(u32::MAX, 0));
            report["hints"] = serde_json::to_value(bone.inlay_hints(&file, full))?;
        }
        "document-symbols" => {
            report["symbols"] = serde_json::to_value(bone.document_symbols(&file))?;
        }
        _ => unreachable!("unhandled subcommand: {name}"),
    }
    Ok(report)
}

/// A null result means the query printed its own output.
fn position_query(name: &str, m: &ArgMatches) -> Result<Value> {
    let (path, line, column) = parse_position(arg_str(m, "position"))?;
    // CLI positions are 1-based; LSP positions are 0-based.
    let pos = Position::new(line - 1, column - 1);
    let (file, mut bone) = match name {
        "references" | "rename" => open_project(&path),
        _ => open_file(&path)?,
    };
    let (key, value) = match name {
        "hover" => ("hover", hover_json(&bone, &file, pos)),
        "definition" => ("definitions", locations_json(bone.definition(&file, pos))?),
        "type-definition" => (
            "definitions",
            locations_json(bone.type_definition(&file, pos))?,
        ),
        "references" => (
            "references",
            serde_json::to_value(bone.references(&file, pos))?,
        ),
        "completion" => {
            let items = bone.completion(&file, pos);
            if m.get_flag("explain") {
                for (i, it) in items.iter().enumerate() {
                    let st = it.sort_text.as_deref().unwrap_or("");
                    let ex = bone_query::explain_sort_text(st)
                        .unwrap_or_else(|| format!("raw sort_text: {st}"));
                    println!("{:3}. {:<40} {}", i + 1, it.label, ex);
                }
                return Ok(Value::Null);
            }
            ("completions", serde_json::to_value(&items)?)
        }
        "signature-help" => {
            // Printed from the struct to keep its field order.
            if let Some(help) = bone.signature_help(&file, pos) {
                println!("{}", serde_json::to_string_pretty(&help)?);
                return Ok(Value::Null);
            }
            return Ok(json!({
                "signatures": [],
                "activeSignature": null,
                "activeParameter": null,
            }));
        }
        "rename" => return Ok(rename_json(&bone, &file, pos, arg_str(m, "new_name"))),
        _ => unreachable!("unhandled subcommand: {name}"),
    };
    let mut report = json!({
        "file": file,
        "backend": "bone",
        "position": { "line": line - 1, "character": column - 1 },
    });
    report[key] = value;
    Ok(report)
}

fn hover_json(bone: &BoneWorkspace, file: &str, pos: Position) -> Value {
    let marked_string = |marked: MarkedString| match marked {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(block) => {
            format!("```{}\n{}\n```", block.language, block.value)
        }
    };
    let Some(hover) = bone.hover(file, pos) else {
        // If the file does not type-check, surface that as the hover body.
        let diags = bone.diagnostics_for(file);
        let contents = if diags.is_empty() {
            String::new()
        } else {
            let joined = diags
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            format!("bone: file does not type-check\n\n{joined}")
        };
        return json!({ "contents": contents, "range": null });
    };
    let contents = match hover.contents {
        HoverContents::Markup(markup) => markup.value,
        HoverContents::Scalar(marked) => marked_string(marked),
        HoverContents::Array(marked) => marked
            .into_iter()
            .map(marked_string)
            .collect::<Vec<_>>()
            .join("\n"),
    };
    json!({ "contents": contents, "range": hover.range })
}

fn locations_json(loc: Option<lsp_types::Location>) -> Result<Value> {
    let locations = loc
        .into_iter()
        .map(|loc| Ok(json!({ "file": loc.uri.to_file_path()?, "range": loc.range })))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(locations))
}

fn rename_json(bone: &BoneWorkspace, file: &str, pos: Position, new_name: &str) -> Value {
    let renamed = bone.rename(file, pos, new_name);
    let refusal = renamed.as_ref().err().map(|r| r.to_string());
    let mut changes = serde_json::Map::new();
    let edits = renamed.ok().flatten().and_then(|edit| edit.changes);
    for (uri, edits) in edits.into_iter().flatten() {
        // Shape matches the harness applier: { range, new_text }.
        let edits = edits
            .iter()
            .map(|e| json!({ "range": e.range, "new_text": e.new_text }))
            .collect();
        changes.insert(uri.to_string(), Value::Array(edits));
    }
    let mut report = json!({ "backend": "bone", "changes": changes });
    if let Some(refusal) = refusal {
        report["error"] = json!(refusal);
    }
    report
}

type DiagFn = fn(&BoneWorkspace, &str) -> Vec<lsp_types::Diagnostic>;

/// Fail when some files were unreadable: their diagnostics are missing.
pub fn print_report(report: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&report)?);
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

pub fn diagnostics_report(path: &str) -> Result<Value> {
    report_with(path, BoneWorkspace::diagnostics_for)
}

pub fn lint_report(path: &str) -> Result<Value> {
    report_with(path, BoneWorkspace::lint_diagnostics)
}

/// Each project loads once as a whole compilation unit so cross-file symbols
/// and the `unused_fn` lint resolve.
fn report_with(path: &str, diag_fn: DiagFn) -> Result<Value> {
    if fs_err::metadata(path)?.is_dir() {
        return dir_report(path, diag_fn);
    }
    let (file_path, bone) = open_file(path)?;
    Ok(json!({
        "file": file_path,
        "backend": "bone",
        "diagnostics": diag_fn(&bone, &file_path),
    }))
}

fn dir_report(dir: &str, diag_fn: DiagFn) -> Result<Value> {
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
        file_reports.push(json!({ "file": f, "diagnostics": diags }));
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

fn canonical(file_path: &str) -> String {
    // Strip Windows' `\\?\` verbatim prefix so this matches bone-query's module keys.
    bone_query::canonicalize_lenient(file_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    fn enabled(rust_log: Option<&str>, probes: &[(&str, Level)]) -> Vec<bool> {
        let filter = stderr_log_filter(rust_log).unwrap();
        probes
            .iter()
            .map(|(target, level)| filter.enabled(target, *level))
            .collect()
    }

    #[test]
    fn rust_log_accepts_standard_directives() {
        use Level::*;
        assert_eq!(
            enabled(
                Some("rock_lsp=debug"),
                &[("rock_lsp", Debug), ("other", Warn), ("other", Info)]
            ),
            [true, true, false]
        );
        assert_eq!(
            enabled(
                Some("info,bone_query=trace"),
                &[("x", Info), ("x", Debug), ("bone_query", Trace)]
            ),
            [true, false, true]
        );
        assert_eq!(enabled(None, &[("x", Warn), ("x", Info)]), [true, false]);
    }

    #[test]
    fn rust_log_rejects_unparsable_directives() {
        for bad in ["rock_lsp=loud", "a=b=c"] {
            let err = stderr_log_filter(Some(bad)).err().expect(bad).to_string();
            assert!(err.contains("RUST_LOG") && err.contains(bad), "{err}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn debug_log_uses_the_system_temp_directory() {
        assert_eq!(
            default_debug_file(),
            std::env::temp_dir().join("lsp_debug.log")
        );
    }

    #[test]
    fn unopenable_debug_log_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-dir").join("lsp.log");
        let err = open_debug_log(path.to_str().unwrap())
            .expect_err("open must fail")
            .to_string();
        assert!(
            err.contains("ROCK_LSP_DEBUG") && err.contains("missing-dir"),
            "{err}"
        );
    }
}
