use anyhow::Result;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use super::parallel_runner::{self, FileTestResult};

/// `ROCK_BACKEND` override (if any) for the *spawned* `rock-lsp` child running
/// `.rock.lsp` fixtures. The bone-backed LSP (`rock-lang/crates/rock-lsp/src/lib.rs`) is
/// the language-server front for the skull/bone stack, so `rock test
/// --backend {skull,bone}` runs fixtures through it; `--backend rock` uses the
/// legacy LSP unless `ROCK_LSP_TEST_BACKEND=bone` forces the bone path (the
/// standalone parity lens, the compiler contract).
///
/// We deliberately set this on the CHILD only, never the parent's
/// `ROCK_BACKEND`: a parent override would also switch `rock test` itself,
/// which then type-checks the `«cursor»`-marked fixture as Rock source and
/// chokes on the marker.
fn lsp_child_backend_env(backend: crate::Backend) -> Option<(&'static str, &'static str)> {
    let bone = matches!(backend, crate::Backend::Bone | crate::Backend::Skull)
        || std::env::var("ROCK_LSP_TEST_BACKEND").ok().as_deref() == Some("bone");
    bone.then_some(("ROCK_BACKEND", "bone"))
}

/// LSP test file structure
#[derive(Debug, Clone)]
struct LspTest {
    name: String,
    test_type: String,  // completion, goto_definition, hover, etc.
    code: String,
    /// Multi-file projects use `FILES:` / `FILE:` blocks in the test source.
    /// When non-empty, `code` holds the content of the primary file (the one
    /// containing the `«cursor»` or `«start»` marker), and `files` holds every
    /// file in the project including the primary. `primary_file` names the
    /// primary file's relative path within the project.
    ///
    /// Empty for single-file tests.
    files: Vec<(String, String)>,
    primary_file: String,
    operation: LspOperation,
    expected: Vec<String>,
    /// For single-file rename: full post-rename file content, compared exactly
    /// against the post-edit buffer. `None` when the EXPECTED block is a
    /// `- snippet` list (substring-matched via `expected` instead).
    expected_code: Option<String>,
    /// For multi-file rename: expected per-file content after edits are applied.
    expected_files: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
enum LspOperation {
    Completion { line: usize, character: usize },
    GotoDefinition { line: usize, character: usize },
    GotoTypeDefinition { line: usize, character: usize },
    Hover { line: usize, character: usize },
    References { line: usize, character: usize },
    InlayHints,
    Rename { line: usize, character: usize, new_name: String },
    SemanticTokens,
    SignatureHelp { line: usize, character: usize },
    DocumentSymbols,
    WorkspaceSymbols { query: String },
}

/// An edit sequence test: multiple steps driving a persistent LSP server
#[derive(Debug, Clone)]
struct EditSequenceTest {
    name: String,
    steps: Vec<EditStep>,
}

/// How a step modifies the document
#[derive(Debug, Clone)]
enum StepMode {
    /// Replace the full document at once (existing behavior)
    FullReplace { code: String },
    /// Type characters one at a time, starting at (line, col) in the previous document
    TypeAt { line: usize, col: usize, text: String },
    /// Delete N characters backwards from (line, col) in the previous document
    DeleteAt { line: usize, col: usize, count: usize },
}

/// A single step in an edit sequence
#[derive(Debug, Clone)]
struct EditStep {
    /// How this step modifies the document
    mode: StepMode,
    /// Checks to perform after applying this step
    checks: Vec<EditStepCheck>,
}

/// A check to perform after an edit step
#[derive(Debug, Clone)]
struct EditStepCheck {
    operation: EditStepOperation,
    expected: Vec<String>,
}

#[derive(Debug, Clone)]
enum EditStepOperation {
    SemanticTokens,
    Diagnostics,
    Completion { line: usize, character: usize },
    Hover { line: usize, character: usize },
    /// `CHECK: references(line, char)` -- EXPECTED lines are `LINE:CHAR`
    /// start positions; the returned set must match exactly (order-free).
    References { line: usize, character: usize },
}

/// Check if the given path contains LSP tests
pub fn has_lsp_tests(path: &Path) -> bool {
    if path.is_file() {
        return path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s == "lsp")
            .unwrap_or(false);
    }

    // Check if it's the lsp test directory
    if path.ends_with("tests/lsp") {
        return true;
    }

    // Check if any parent is tests/lsp
    let mut current = path;
    while let Some(parent) = current.parent() {
        if parent.ends_with("tests/lsp") {
            return true;
        }
        current = parent;
    }

    // Check if directory contains .lsp files or has an lsp subdirectory
    if path.is_dir() {
        // First check for .lsp files directly in this directory
        if let Ok(entries) = fs_err::read_dir(path) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if let Some(ext) = entry_path.extension() {
                    if ext == "lsp" {
                        return true;
                    }
                }
                // Also check if there's an 'lsp' subdirectory with .lsp files (recursively)
                if entry_path.is_dir() && entry_path.file_name().and_then(|s| s.to_str()) == Some("lsp") {
                    // Recursively check if the lsp subdirectory contains .lsp files
                    for entry in walkdir::WalkDir::new(&entry_path).into_iter().flatten() {
                        if entry.file_type().is_file() {
                            if let Some(ext) = entry.path().extension() {
                                if ext == "lsp" {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    false
}

/// Find the LSP test directory from a given path
pub fn find_lsp_test_dir(path: &Path) -> Option<PathBuf> {
    if path.ends_with("tests/lsp") && path.is_dir() {
        return Some(path.to_path_buf());
    }

    // If path is tests/lsp/something, return tests/lsp
    if let Some(parent) = path.parent() {
        if parent.ends_with("tests/lsp") && parent.is_dir() {
            return Some(parent.to_path_buf());
        }
    }

    // If path is tests/, check for tests/lsp
    let lsp_subdir = path.join("lsp");
    if lsp_subdir.exists() && lsp_subdir.is_dir() {
        return Some(lsp_subdir);
    }

    // If this is an LSP test file, use its parent directory
    if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("lsp") {
        return path.parent().map(|p| p.to_path_buf());
    }

    None
}

/// Strip `«cursor»`, `«start»`, and `«end»` markers from a raw line.
/// Returns (clean_line, Option<cursor_char>). `«start»` counts as the cursor
/// for rename-style tests; `«cursor»` takes precedence if both appear.
fn strip_markers(raw: &str) -> (String, Option<usize>) {
    let mut clean = raw.to_string();
    // Prefer «cursor» if present, otherwise fall back to «start».
    let cursor_pos = if let Some(p) = clean.find("«cursor»") {
        clean = clean.replace("«cursor»", "");
        clean = clean.replace("«start»", "").replace("«end»", "");
        Some(p)
    } else if let Some(p) = clean.find("«start»") {
        clean = clean.replace("«start»", "").replace("«end»", "");
        Some(p)
    } else {
        clean = clean.replace("«end»", "");
        None
    };
    (clean, cursor_pos)
}

/// Parse a single FILES: / FILE: block sequence until the next `OPERATION:` or
/// `EXPECTED:` line. Returns the collected files (name, content) with all
/// cursor markers stripped, plus the cursor position if any.
#[allow(clippy::type_complexity)]
fn parse_files_block<'a, I>(
    lines: &mut std::iter::Peekable<I>,
    stop_prefixes: &[&str],
) -> (
    Vec<(String, String)>,
    Option<(String, usize, usize)>, // (file_name, line, char)
)
where
    I: Iterator<Item = &'a str>,
{
    let mut files: Vec<(String, String)> = Vec::new();
    let mut cursor: Option<(String, usize, usize)> = None;

    // Optional leading `---` after `FILES:` / `EXPECTED:`
    if lines.peek().map(|l| l.starts_with("---")).unwrap_or(false) {
        lines.next();
    }

    while let Some(line) = lines.peek() {
        if stop_prefixes.iter().any(|p| line.starts_with(p)) {
            break;
        }
        if let Some(rest) = line.strip_prefix("FILE:") {
            let name = rest.trim().to_string();
            lines.next();
            // consume leading `---`
            if lines.peek().map(|l| l.starts_with("---")).unwrap_or(false) {
                lines.next();
            }
            // collect content until next `---` or stop prefix
            let mut content_lines: Vec<String> = Vec::new();
            let mut file_line_num: usize = 0;
            while let Some(l) = lines.peek() {
                if l.starts_with("---") {
                    lines.next();
                    break;
                }
                if stop_prefixes.iter().any(|p| l.starts_with(p)) {
                    break;
                }
                let raw = lines.next().unwrap();
                let (clean, cursor_pos) = strip_markers(raw);
                if let Some(c) = cursor_pos {
                    if cursor.is_none() {
                        cursor = Some((name.clone(), file_line_num, c));
                    }
                }
                content_lines.push(clean);
                file_line_num += 1;
            }
            files.push((name, content_lines.join("\n")));
        } else {
            // Blank line or stray text inside FILES: block — skip.
            lines.next();
        }
    }

    (files, cursor)
}

/// Parse an LSP test file
fn parse_lsp_test_file(path: &Path) -> Result<Vec<LspTest>> {
    let contents = fs_err::read_to_string(path)?;
    let mut tests = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        if line.starts_with("TEST:") {
            let name = line.strip_prefix("TEST:").unwrap().trim().to_string();

            // Parse test type
            let test_type = if let Some(next_line) = lines.next() {
                if next_line.starts_with("TYPE:") {
                    next_line.strip_prefix("TYPE:").unwrap().trim().to_string()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            // Multi-file mode: `FILES:` after `TYPE:` instead of a code block.
            let is_multi_file =
                lines.peek().map(|l| l.trim() == "FILES:").unwrap_or(false);

            let mut files: Vec<(String, String)> = Vec::new();
            let mut primary_file = String::new();
            let mut cursor_line: Option<usize> = None;
            let mut cursor_char: Option<usize> = None;
            let mut code = String::new();

            if is_multi_file {
                lines.next(); // consume `FILES:`
                let (parsed_files, cur) =
                    parse_files_block(&mut lines, &["OPERATION:"]);
                if let Some((fname, l, c)) = cur {
                    primary_file = fname.clone();
                    cursor_line = Some(l);
                    cursor_char = Some(c);
                    // Primary file code is the one holding the cursor marker.
                    for (name, content) in &parsed_files {
                        if name == &fname {
                            code = content.clone();
                            break;
                        }
                    }
                }
                files = parsed_files;
            } else {
                // Skip separator
                if let Some(sep) = lines.next() {
                    if !sep.starts_with("---") {
                        continue;
                    }
                }

                // Collect code until we hit the next separator
                let mut code_lines = Vec::new();
                let mut line_num = 0;

                while let Some(line) = lines.peek() {
                    if line.starts_with("---") {
                        lines.next();
                        break;
                    }
                    let line = lines.next().unwrap();
                    let (clean, cursor_pos) = strip_markers(line);
                    if let Some(c) = cursor_pos {
                        if cursor_line.is_none() {
                            cursor_line = Some(line_num);
                            cursor_char = Some(c);
                        }
                    }
                    code_lines.push(clean);
                    line_num += 1;
                }

                code = code_lines.join("\n");
            }

            // Parse operation
            let operation = if let Some(op_line) = lines.next() {
                if op_line.starts_with("OPERATION:") {
                    let op_type = op_line.strip_prefix("OPERATION:").unwrap().trim();

                    // Some operations don't need positions
                    match op_type {
                        "inlay_hints" | "semantic_tokens" | "full_document" | "document_symbols" => {
                            // These operations work on the full document
                            match op_type {
                                "inlay_hints" => LspOperation::InlayHints,
                                "semantic_tokens" | "full_document" => LspOperation::SemanticTokens,
                                "document_symbols" => LspOperation::DocumentSymbols,
                                _ => continue,
                            }
                        }
                        "workspace_symbols" => {
                            // Workspace symbols needs a query
                            let query = if let Some(query_line) = lines.peek() {
                                if query_line.starts_with("QUERY:") {
                                    let query_line = lines.next().unwrap();
                                    query_line.strip_prefix("QUERY:").unwrap().trim().to_string()
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            };
                            LspOperation::WorkspaceSymbols { query }
                        }
                        _ => {
                            // Other operations need a position
                            let (line, character) = if let Some(pos_line) = lines.peek() {
                                if pos_line.starts_with("POSITION:") {
                                    let pos_line = lines.next().unwrap();
                                    parse_position(pos_line)?
                                } else if let (Some(l), Some(c)) = (cursor_line, cursor_char) {
                                    (l, c)
                                } else {
                                    continue;
                                }
                            } else if let (Some(l), Some(c)) = (cursor_line, cursor_char) {
                                (l, c)
                            } else {
                                continue;
                            };

                            match op_type {
                                "completion" => LspOperation::Completion { line, character },
                                "goto_definition" => LspOperation::GotoDefinition { line, character },
                                "goto_type_definition" => LspOperation::GotoTypeDefinition { line, character },
                                "hover" => LspOperation::Hover { line, character },
                                "references" => LspOperation::References { line, character },
                                "code_action" => {
                                    // The rock-lsp CLI has no `code-actions`
                                    // subcommand, so these tests cannot run.
                                    // Skip at parse time rather than failing
                                    // loudly — matches the prior silent no-op
                                    // behaviour. When code-action CLI support
                                    // is added, remove this skip.
                                    let _ = line;
                                    let _ = character;
                                    continue;
                                }
                                "signature_help" => LspOperation::SignatureHelp { line, character },
                                "rename" => {
                                    // For rename, we need to parse the new name
                                    // Look for NEW_NAME: in the next line
                                    if let Some(name_line) = lines.peek() {
                                        if name_line.starts_with("NEW_NAME:") {
                                            let name_line = lines.next().unwrap();
                                            let new_name = name_line.strip_prefix("NEW_NAME:").unwrap().trim().to_string();
                                            LspOperation::Rename { line, character, new_name }
                                        } else {
                                            // Default new name if not specified
                                            LspOperation::Rename { line, character, new_name: "renamed".to_string() }
                                        }
                                    } else {
                                        LspOperation::Rename { line, character, new_name: "renamed".to_string() }
                                    }
                                }
                                _ => continue,
                            }
                        }
                    }
                } else {
                    continue;
                }
            } else {
                continue;
            };

            // Skip separator if present
            if let Some(line) = lines.peek() {
                if line.starts_with("---") {
                    lines.next();
                }
            }

            // Parse expected results
            let mut expected = Vec::new();
            let mut expected_code: Option<String> = None;
            let mut rename_lines: Vec<String> = Vec::new();
            let mut expected_files: Vec<(String, String)> = Vec::new();
            if let Some(exp_line) = lines.next() {
                if exp_line.starts_with("EXPECTED:") {
                    // Multi-file EXPECTED uses `FILE: name / --- / content / ---` blocks
                    // up to `===`. Detect by peeking for `FILE:`.
                    let is_multi_expected = is_multi_file
                        && lines
                            .peek()
                            .map(|l| {
                                l.starts_with("FILE:")
                                    || (l.starts_with("---")
                                        && {
                                            // Peek one more: not straightforward with
                                            // `Peekable`, so just allow the `---` leader.
                                            true
                                        })
                            })
                            .unwrap_or(false);

                    if is_multi_expected {
                        let (parsed, _cur) =
                            parse_files_block(&mut lines, &["==="]);
                        expected_files = parsed;
                        // Consume `===`
                        if lines.peek().map(|l| l.starts_with("===")).unwrap_or(false) {
                            lines.next();
                        }
                    } else {
                    // FOOT-GUN: every new TYPE: must have a matching branch
                    // below, or its expected lines will silently be dropped
                    // and the test will no-op. See the compiler contract.
                    while let Some(line) = lines.peek() {
                        if line.starts_with("===") {
                            lines.next();
                            break;
                        }
                        let line = lines.next().unwrap();
                        if test_type == "rename" {
                            // Raw capture; snippet-vs-full-code is decided
                            // after the loop from the first non-empty line.
                            rename_lines.push(line.to_string());
                        } else if line.starts_with("- ") {
                            expected.push(line.strip_prefix("- ").unwrap().to_string());
                        } else if line.starts_with("file:") || line.starts_with("line:") || line.starts_with("character:") || line.starts_with("min_tokens:") {
                            expected.push(line.to_string());
                        } else if (test_type == "goto_definition" || test_type == "goto_type_definition") && line.trim().eq_ignore_ascii_case("none") {
                            // `EXPECTED: none` — goto-def/type-def should return no targets.
                            expected.push("none".to_string());
                        } else if test_type == "hover" {
                            // For hover tests, include all lines as expected content
                            expected.push(line.to_string());
                        } else if test_type == "inlay_hints" {
                            // Expected block is a re-rendering of the source
                            // with `«hint::LABEL»` markers inline. Validator
                            // parses these into (line, col, label) triples.
                            expected.push(line.to_string());
                        } else if test_type == "signature_help" {
                            // Bare signature strings like `greet(name: string,
                            // times: int)` match any returned signature. A
                            // line starting with `ACTIVE:` matches the
                            // *active* signature only (used to verify overload
                            // resolution picked the right arm).
                            if !line.trim().is_empty() {
                                expected.push(line.to_string());
                            }
                        }
                    }
                    }
                }
            }

            // Single-file rename EXPECTED comes in two shapes: a `- snippet`
            // list (substring-matched) or the full post-rename file content
            // (compared exactly -- a snippet match can't catch missed sites).
            if test_type == "rename" && expected_files.is_empty() {
                let snippet_style = rename_lines
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.starts_with("- "));
                if snippet_style {
                    expected.extend(
                        rename_lines
                            .iter()
                            .filter_map(|l| l.strip_prefix("- ").map(str::to_string)),
                    );
                } else {
                    expected_code = Some(rename_lines.join("\n"));
                }
            }

            tests.push(LspTest {
                name,
                test_type,
                code,
                files,
                primary_file,
                operation,
                expected,
                expected_code,
                expected_files,
            });
        }
    }

    Ok(tests)
}

/// Parse edit sequence tests from a test file
fn parse_edit_sequence_tests(path: &Path) -> Result<Vec<EditSequenceTest>> {
    let contents = std::fs::read_to_string(path)?;
    let mut tests = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        if !line.starts_with("TEST:") {
            continue;
        }
        let name = line.strip_prefix("TEST:").unwrap().trim().to_string();

        // Parse test type
        let test_type = match lines.next() {
            Some(next_line) if next_line.starts_with("TYPE:") => {
                next_line.strip_prefix("TYPE:").unwrap().trim().to_string()
            }
            _ => continue,
        };

        if test_type != "edit_sequence" {
            // Skip non-edit_sequence tests - consume until ===
            for line in lines.by_ref() {
                if line.starts_with("===") {
                    break;
                }
            }
            continue;
        }

        // Skip separator after TYPE
        if let Some(sep) = lines.next() {
            if !sep.starts_with("---") {
                continue;
            }
        }

        let mut steps = Vec::new();

        // Parse steps until ===
        while let Some(&next) = lines.peek() {

            if next.starts_with("===") {
                lines.next();
                break;
            }

            if next.starts_with("STEP") {
                let step_line = lines.next().unwrap(); // consume the STEP line

                // Parse step mode from the STEP line
                // Formats:
                //   STEP N:                          -> FullReplace
                //   STEP N (type at LINE:COL):       -> TypeAt
                //   STEP N (delete at LINE:COL, N):  -> DeleteAt
                let step_mode_hint = if let Some(paren_start) = step_line.find('(') {
                    let paren_content = &step_line[paren_start..];
                    Some(paren_content.to_string())
                } else {
                    None
                };

                // Collect body lines until ---
                let mut code_lines = Vec::new();
                while let Some(line) = lines.peek() {
                    if line.starts_with("---") {
                        lines.next();
                        break;
                    }
                    code_lines.push(lines.next().unwrap().to_string());
                }
                let body = code_lines.join("\n");

                // Determine the step mode
                let mode = if let Some(ref hint) = step_mode_hint {
                    if hint.contains("type at") {
                        // Parse (type at LINE:COL):
                        let coords = hint
                            .trim_start_matches(|c: char| c != 't')
                            .strip_prefix("type at ")
                            .and_then(|s| s.strip_suffix("):"))
                            .or_else(|| hint
                                .trim_start_matches(|c: char| c != 't')
                                .strip_prefix("type at ")
                                .and_then(|s| s.strip_suffix(")")))
                            .unwrap_or("0:0");
                        let parts: Vec<&str> = coords.split(':').collect();
                        let line = parts.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                        let col = parts.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                        StepMode::TypeAt { line, col, text: body }
                    } else if hint.contains("delete at") {
                        // Parse (delete at LINE:COL, COUNT):
                        let inner = hint
                            .trim_start_matches(|c: char| c != 'd')
                            .strip_prefix("delete at ")
                            .and_then(|s| s.strip_suffix("):"))
                            .or_else(|| hint
                                .trim_start_matches(|c: char| c != 'd')
                                .strip_prefix("delete at ")
                                .and_then(|s| s.strip_suffix(")")))
                            .unwrap_or("0:0, 1");
                        let parts: Vec<&str> = inner.split(',').collect();
                        let coords: Vec<&str> = parts.first().unwrap_or(&"0:0").split(':').collect();
                        let line = coords.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                        let col = coords.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                        let count = parts.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(1);
                        StepMode::DeleteAt { line, col, count }
                    } else {
                        StepMode::FullReplace { code: body }
                    }
                } else {
                    StepMode::FullReplace { code: body }
                };

                // Parse checks for this step
                let mut checks = Vec::new();
                while let Some(line) = lines.peek() {
                    if line.starts_with("STEP") || line.starts_with("===") {
                        break;
                    }
                    if line.starts_with("CHECK:") {
                        let check_line = lines.next().unwrap();
                        let op_str = check_line.strip_prefix("CHECK:").unwrap().trim();

                        let operation = match op_str {
                            "semantic_tokens" => EditStepOperation::SemanticTokens,
                            "diagnostics" => EditStepOperation::Diagnostics,
                            _ if op_str.starts_with("completion") => {
                                // CHECK: completion(line, char)
                                if let Some(params) = op_str.strip_prefix("completion(")
                                    .and_then(|s| s.strip_suffix(')'))
                                {
                                    let parts: Vec<&str> = params.split(',').collect();
                                    if parts.len() == 2 {
                                        let line = parts[0].trim().parse().unwrap_or(0);
                                        let character = parts[1].trim().parse().unwrap_or(0);
                                        EditStepOperation::Completion { line, character }
                                    } else {
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                            }
                            _ if op_str.starts_with("hover") => {
                                if let Some(params) = op_str.strip_prefix("hover(")
                                    .and_then(|s| s.strip_suffix(')'))
                                {
                                    let parts: Vec<&str> = params.split(',').collect();
                                    if parts.len() == 2 {
                                        let line = parts[0].trim().parse().unwrap_or(0);
                                        let character = parts[1].trim().parse().unwrap_or(0);
                                        EditStepOperation::Hover { line, character }
                                    } else {
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                            }
                            _ if op_str.starts_with("references") => {
                                if let Some(params) = op_str.strip_prefix("references(")
                                    .and_then(|s| s.strip_suffix(')'))
                                {
                                    let parts: Vec<&str> = params.split(',').collect();
                                    if parts.len() == 2 {
                                        let line = parts[0].trim().parse().unwrap_or(0);
                                        let character = parts[1].trim().parse().unwrap_or(0);
                                        EditStepOperation::References { line, character }
                                    } else {
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                            }
                            _ => {
                                lines.next();
                                continue;
                            }
                        };

                        // Parse EXPECTED: section
                        let mut expected = Vec::new();
                        if let Some(exp_line) = lines.peek() {
                            if exp_line.starts_with("EXPECTED:") {
                                lines.next();
                                while let Some(line) = lines.peek() {
                                    if line.starts_with("---") || line.starts_with("STEP")
                                        || line.starts_with("===") || line.starts_with("CHECK:")
                                    {
                                        break;
                                    }
                                    let line = lines.next().unwrap();
                                    if line.starts_with("- ") {
                                        expected.push(line.strip_prefix("- ").unwrap().to_string());
                                    } else if !line.trim().is_empty() {
                                        expected.push(line.to_string());
                                    }
                                }
                            }
                        }

                        checks.push(EditStepCheck { operation, expected });
                    } else {
                        lines.next();
                    }
                }

                steps.push(EditStep { mode, checks });
            } else {
                lines.next(); // skip unrecognized lines
            }
        }

        if !steps.is_empty() {
            tests.push(EditSequenceTest { name, steps });
        }
    }

    Ok(tests)
}

/// Normalise an inlay-hint label for comparison: trim whitespace, strip a
/// leading `:`, trim again. Handles both the marker encoding convention
/// (`«hint:: int»` → `: int`) and the LSP's padded labels (`: int`) by
/// reducing both to `int`. Similarly `«hint::key:»` → `:key:` → `key:`
/// matches the param-hint label `key:` directly.
fn normalise_hint_label(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix(':').unwrap_or(t);
    t.trim().to_string()
}

/// Extract `«hint::LABEL»` markers from a rendered-expected block for
/// inlay-hint tests. Returns (line, stripped_char, label) triples. Each
/// marker consumed on a line shifts subsequent marker positions left by
/// the marker's full width, so the returned column matches where the
/// LSP reports the hint in the clean source.
fn parse_hint_markers(lines: &[String]) -> Vec<(u64, u64, String)> {
    const OPEN: &str = "«hint:";
    const CLOSE: &str = "»";
    let mut out = Vec::new();
    for (line_idx, raw) in lines.iter().enumerate() {
        let mut rest: &str = raw;
        let mut stripped_offset: usize = 0; // byte offset in the stripped line
        while let Some(open_rel) = rest.find(OPEN) {
            // Chars before the marker contribute to the stripped offset.
            let before = &rest[..open_rel];
            stripped_offset += before.chars().count();
            let after_open = &rest[open_rel + OPEN.len()..];
            let Some(close_rel) = after_open.find(CLOSE) else {
                break;
            };
            let label = &after_open[..close_rel];
            out.push((line_idx as u64, stripped_offset as u64, label.to_string()));
            rest = &after_open[close_rel + CLOSE.len()..];
        }
    }
    out
}

/// Apply a list of LSP text edits to a source string and return the result.
/// Edits are sorted descending by start position so earlier positions don't
/// shift under later edits. Line/character are 0-indexed.
fn apply_text_edits(source: &str, edits: &[&serde_json::Value]) -> String {
    let mut edits: Vec<&serde_json::Value> = edits.to_vec();
    let pos = |v: &serde_json::Value, which: &str| -> (u64, u64) {
        let range = v.get("range").and_then(|r| r.get(which));
        let line = range.and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0);
        let ch = range.and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0);
        (line, ch)
    };
    edits.sort_by_key(|e| std::cmp::Reverse(pos(e, "start")));

    let mut result = source.to_string();
    for edit in edits {
        let Some(new_text) = edit.get("new_text").and_then(|t| t.as_str()) else {
            continue;
        };
        let (sl, sc) = pos(edit, "start");
        let (el, ec) = pos(edit, "end");
        let (sl, sc, el, ec) = (sl as usize, sc as usize, el as usize, ec as usize);

        let lines: Vec<&str> = result.lines().collect();
        let mut new_lines: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i < sl || i > el {
                new_lines.push(line.to_string());
            } else if i == sl && i == el {
                let before = &line[..sc.min(line.len())];
                let after = &line[ec.min(line.len())..];
                new_lines.push(format!("{}{}{}", before, new_text, after));
            } else if i == sl {
                let before = &line[..sc.min(line.len())];
                new_lines.push(format!("{}{}", before, new_text));
            } else if i == el {
                let after = &line[ec.min(line.len())..];
                if let Some(last) = new_lines.last_mut() {
                    last.push_str(after);
                }
            }
        }
        result = new_lines.join("\n");
    }
    result
}

/// Parse position from a POSITION: line
fn parse_position(line: &str) -> Result<(usize, usize)> {
    let pos_str = line.strip_prefix("POSITION:").unwrap().trim();

    // Parse "line=X, character=Y" format
    let parts: Vec<&str> = pos_str.split(',').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!("Invalid position format: {}", pos_str));
    }

    let line = parts[0].trim()
        .strip_prefix("line=")
        .ok_or_else(|| anyhow::anyhow!("Missing 'line=' in position"))?
        .parse::<usize>()?;

    let character = parts[1].trim()
        .strip_prefix("character=")
        .ok_or_else(|| anyhow::anyhow!("Missing 'character=' in position"))?
        .parse::<usize>()?;

    Ok((line, character))
}

/// Environment variable that points the fixture runner at a different server
/// build. Tests use it to drive the runner with a stub server; it is the only
/// override, so every fixture runs one known binary.
const LSP_TEST_BIN_ENV: &str = "ROCK_LSP_TEST_BIN";

/// The binary every fixture request runs: the executable that is running this
/// suite, so a fixture always tests the build under test. Probing a working
/// directory for `target/debug/rock` (and falling back to `cargo run`) used to
/// pick a different build than the one running, and `cargo run` also made the
/// suite slow and flaky under parallel load.
fn lsp_server_bin() -> Result<PathBuf> {
    if let Some(requested) = std::env::var_os(LSP_TEST_BIN_ENV) {
        let path = PathBuf::from(requested);
        if !path.is_file() {
            return Err(anyhow::anyhow!(
                "{LSP_TEST_BIN_ENV} points at {}, which is not a file",
                path.display()
            ));
        }
        return Ok(path);
    }
    std::env::current_exe().map_err(|e| {
        anyhow::anyhow!("cannot find the running executable to use as the LSP server: {e}")
    })
}

/// Run a single LSP test
fn run_lsp_test(test: &LspTest, backend: crate::Backend) -> Result<bool> {
    // Create a test file in a local test directory
    let test_dir = std::path::PathBuf::from(".lsp-test-tmp");
    fs_err::create_dir_all(&test_dir)?;

    // Sanitise the test name for filesystem use.
    let safe_name: String = test
        .name
        .chars()
        .map(|c| if c == ' ' || c == '/' { '_' } else { c })
        .collect();

    // Multi-file tests get their own subdirectory containing `stick.toml` plus
    // every `FILE:` entry. The primary file (the one with the cursor marker)
    // is the path passed to the rock-lsp CLI. `project.toml` in the spec is
    // translated to `stick.toml` since that's Rock's real project manifest.
    let (test_file, project_dir): (PathBuf, Option<PathBuf>) = if !test.files.is_empty() {
        let proj = test_dir.join(&safe_name);
        fs_err::create_dir_all(&proj)?;
        for (name, content) in &test.files {
            // Skip user-supplied manifests — we write our own below so the
            // format matches what rock-lsp's stick.toml parser expects.
            if name == "project.toml" || name == "stick.toml" {
                continue;
            }
            fs_err::write(proj.join(name), content)?;
        }
        fs_err::write(
            proj.join("stick.toml"),
            "[package]\nname = \"lsp_test\"\nversion = \"0.1.0\"\n",
        )?;
        let primary = if test.primary_file.is_empty() {
            test.files.first().map(|(n, _)| n.clone()).unwrap_or_default()
        } else {
            test.primary_file.clone()
        };
        (proj.join(&primary), Some(proj))
    } else {
        // Each single-file test gets its OWN subdirectory (with a stick.toml),
        // not a shared file in `.lsp-test-tmp/`. Operations that scan the whole
        // project (`references`, `workspace-symbols` -> `load_project`) would
        // otherwise pick up sibling fixtures' symbols and race against their
        // parallel create/delete. Isolating per test makes the project bound
        // to exactly this file.
        let proj = test_dir.join(&safe_name);
        fs_err::create_dir_all(&proj)?;
        fs_err::write(
            proj.join("stick.toml"),
            "[package]\nname = \"lsp_test\"\nversion = \"0.1.0\"\n",
        )?;
        let tf = proj.join(format!("{}.rock", safe_name));
        fs_err::write(&tf, &test.code)?;
        (tf, Some(proj))
    };

    // Build the command based on the operation
    // Note: The CLI expects 1-indexed line and column numbers
    let args: Vec<String> = match &test.operation {
        LspOperation::Completion { line, character } => {
            vec!["completion".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1)]
        }
        LspOperation::GotoDefinition { line, character } => {
            vec!["definition".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1)]
        }
        LspOperation::GotoTypeDefinition { line, character } => {
            vec!["type-definition".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1)]
        }
        LspOperation::Hover { line, character } => {
            vec!["hover".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1)]
        }
        LspOperation::References { line, character } => {
            vec!["references".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1)]
        }
        LspOperation::InlayHints => {
            vec!["inlay-hints".to_string(), test_file.display().to_string()]
        }
        LspOperation::Rename { line, character, new_name } => {
            vec!["rename".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1), new_name.clone()]
        }
        LspOperation::SemanticTokens => {
            vec!["semantic-tokens".to_string(), test_file.display().to_string()]
        }
        LspOperation::SignatureHelp { line, character } => {
            vec!["signature-help".to_string(), format!("{}:{}:{}", test_file.display(), line + 1, character + 1)]
        }
        LspOperation::DocumentSymbols => {
            vec!["document-symbols".to_string(), test_file.display().to_string()]
        }
        LspOperation::WorkspaceSymbols { query } => {
            vec!["workspace-symbols".to_string(), test_file.display().to_string(), query.clone()]
        }
    };

    // Execute the LSP command - use the already built binary for speed. The
    // one-shot handlers live under `rock lsp <subcommand>`; there is no longer
    // a separate `rock-lsp` binary (folded in by 6072750d1), so prepend `lsp`.
    // Execute the LSP command against the running executable. The one-shot
    // handlers live under `rock lsp <subcommand>`; there is no longer a
    // separate `rock-lsp` binary (folded in by 6072750d1), so prepend `lsp`.
    let rock_bin = lsp_server_bin()?;
    let mut cmd = Command::new(&rock_bin);
    cmd.arg("lsp");
    cmd.args(&args);
    if let Some((k, v)) = lsp_child_backend_env(backend) {
        cmd.env(k, v);
    }
    let output = cmd.output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Clean up test file / project dir
    if let Some(dir) = &project_dir {
        let _ = fs_err::remove_dir_all(dir);
    } else {
        let _ = fs_err::remove_file(&test_file);
    }

    // Check if there was a real error (not just warnings)
    // Only fail if we see actual error output patterns, not just the word "error" in warnings
    if !output.status.success() {
        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
            eprintln!("    LSP command failed with exit code: {:?}", output.status.code());
            if !stderr.is_empty() {
                eprintln!("    LSP stderr: {}", stderr);
            }
        }
        return Ok(false);
    }

    // Parse the JSON output for completion tests
    let mut all_found = true;

    if test.test_type == "completion" {
        // Try to parse as JSON for completion results
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            if let Some(completions) = json.get("completions").and_then(|c| c.as_array()) {
                let labels: Vec<String> = completions
                    .iter()
                    .filter_map(|c| c.get("label").and_then(|l| l.as_str()))
                    .map(|s| s.to_string())
                    .collect();

                // Check each expected item
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    Checking completions for test '{}'", test.name);
                    eprintln!("    Available completions: {:?}", labels);
                    eprintln!("    Expected items: {:?}", test.expected);
                }
                for expected_item in &test.expected {
                    let found = labels.iter().any(|label| label.contains(expected_item));
                    if !found {
                        all_found = false;
                        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                            eprintln!("    Expected '{}' not found in completions", expected_item);
                            eprintln!("    Available completions: {:?}", labels);
                        }
                    } else if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Found expected item: '{}'", expected_item);
                    }
                }
            } else {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    No completions array found in JSON output");
                }
            }
        } else {
            // Fallback to string contains for non-JSON output
            for expected_item in &test.expected {
                if !stdout.contains(expected_item) {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Expected '{}' not found in output", expected_item);
                    }
                }
            }
        }
    } else if test.test_type == "goto_definition" || test.test_type == "goto_type_definition" {
        // For goto_definition / goto_type_definition, check the output format
        // (both emit the same `definitions` array shape).
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            // The actual LSP output has definitions[0].range.start.{line,character}
            // Get the first definition if it exists
            let definitions_arr = json.get("definitions").and_then(|d| d.as_array());
            let definition = definitions_arr.and_then(|arr| arr.first());

            // Support `EXPECTED: none` — assert that goto-def found no targets.
            // Useful for built-in types, keywords, and positions that have no
            // definition to navigate to (the LSP should return [] not an error).
            let expects_none = test
                .expected
                .iter()
                .any(|e| e.trim().eq_ignore_ascii_case("none"));
            if expects_none {
                if definitions_arr.map(|a| a.is_empty()).unwrap_or(false) {
                    // pass — no definitions as expected
                } else {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!(
                            "    Expected no definitions, got {:?}",
                            definitions_arr
                        );
                    }
                }
            } else if let Some(def) = definition {
                // Check file, line, character in the definition
                for expected_item in &test.expected {
                    if expected_item.starts_with("file:") {
                        // For goto_definition tests, we're actually testing the same file,
                        // so we can skip the file check since it will be the temp file
                        // The test expectations use "main.rock" but that's just a placeholder
                        continue;
                    } else if expected_item.starts_with("line:") {
                        let expected_line: usize = expected_item.strip_prefix("line:").unwrap().trim().parse().unwrap_or(0);
                        if let Some(line) = def.get("range")
                            .and_then(|r| r.get("start"))
                            .and_then(|s| s.get("line"))
                            .and_then(|l| l.as_u64()) {
                            if line != expected_line as u64 {
                                all_found = false;
                                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                    eprintln!("    Expected line {}, got {}", expected_line, line);
                                }
                            }
                        } else {
                            all_found = false;
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!("    No line field found in definition range");
                            }
                        }
                    } else if expected_item.starts_with("character:") {
                        let expected_char: usize = expected_item.strip_prefix("character:").unwrap().trim().parse().unwrap_or(0);
                        if let Some(character) = def.get("range")
                            .and_then(|r| r.get("start"))
                            .and_then(|s| s.get("character"))
                            .and_then(|c| c.as_u64()) {
                            if character != expected_char as u64 {
                                all_found = false;
                                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                    eprintln!("    Expected character {}, got {}", expected_char, character);
                                }
                            }
                        } else {
                            all_found = false;
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!("    No character field found in definition range");
                            }
                        }
                    }
                }
            } else {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    No definitions found in output");
                    eprintln!("    Actual output: {}", stdout);
                }
            }
        } else {
            // Fallback to string contains
            for expected_item in &test.expected {
                if !stdout.contains(expected_item) {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Expected '{}' not found in output", expected_item);
                    }
                }
            }
        }
    } else if test.test_type == "references" {
        // Strict set-equality: the (line, character) pairs returned by the
        // LSP must equal exactly the pairs expected — no extras, no missing,
        // order ignored. `file:` lines in EXPECTED are treated as
        // placeholders and ignored, mirroring the goto_definition validator
        // (the harness writes a single tempfile named after the test, not
        // `main.rock`; multi-file `.rock.lsp` is unsupported, so cross-file
        // assertions live in `rock-lang/crates/rock-lsp/tests/bone/` instead).
        //
        // `EXPECTED:` entries come in groups of three consecutive lines:
        // `file: ...`, `line: N`, `character: N`. A single line containing
        // `none` (case-insensitive) asserts the empty set.
        let parsed = serde_json::from_str::<serde_json::Value>(&stdout);
        let actual_refs: Vec<(u64, u64)> = match &parsed {
            Ok(json) => json
                .get("references")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|loc| {
                            let start = loc.get("range")?.get("start")?;
                            let line = start.get("line")?.as_u64()?;
                            let character = start.get("character")?.as_u64()?;
                            Some((line, character))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        let expects_none = test
            .expected
            .iter()
            .any(|e| e.trim().eq_ignore_ascii_case("none"));

        if expects_none {
            if !actual_refs.is_empty() {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    Expected no references, got {:?}", actual_refs);
                }
            }
        } else if parsed.is_err() {
            all_found = false;
            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                eprintln!("    Failed to parse references JSON output");
            }
        } else {
            // Group flat EXPECTED lines into (line, character) pairs.
            // `file:` lines act as group separators / placeholders.
            let mut expected_refs: Vec<(u64, u64)> = Vec::new();
            let mut cur_line: Option<u64> = None;
            for item in &test.expected {
                let item = item.trim();
                if item.starts_with("file:") {
                    cur_line = None;
                } else if let Some(l) = item.strip_prefix("line:") {
                    cur_line = l.trim().parse().ok();
                } else if let Some(c) = item.strip_prefix("character:") {
                    if let (Some(l), Ok(ch)) = (cur_line, c.trim().parse::<u64>()) {
                        expected_refs.push((l, ch));
                    }
                }
            }

            let mut a = actual_refs.clone();
            let mut e = expected_refs.clone();
            a.sort();
            e.sort();
            if a != e {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    let actual_set: std::collections::BTreeSet<_> =
                        actual_refs.iter().collect();
                    let expected_set: std::collections::BTreeSet<_> =
                        expected_refs.iter().collect();
                    let missing: Vec<_> =
                        expected_set.difference(&actual_set).collect();
                    let extra: Vec<_> =
                        actual_set.difference(&expected_set).collect();
                    if !missing.is_empty() {
                        eprintln!("    Missing references (expected, not returned):");
                        for r in &missing {
                            eprintln!("      {:?}", r);
                        }
                    }
                    if !extra.is_empty() {
                        eprintln!("    Extra references (returned, not expected):");
                        for r in &extra {
                            eprintln!("      {:?}", r);
                        }
                    }
                }
            }
        }
    } else if test.test_type == "semantic_tokens" {
        // For semantic_tokens, check for minimum number of tokens
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            // The rock-lsp outputs in a custom format with count and tokens fields
            let token_count = if let Some(count) = json.get("count").and_then(|c| c.as_u64()) {
                count as usize
            } else if let Some(tokens) = json.get("tokens").and_then(|t| t.as_array()) {
                tokens.len()
            } else if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
                data.len() / 5 // Each token is 5 integers in LSP format
            } else {
                0
            };

            if token_count > 0 {
                // Check if we have the minimum expected tokens
                for expected_item in &test.expected {
                    if expected_item.starts_with("min_tokens:") {
                        let min_tokens: usize = expected_item
                            .strip_prefix("min_tokens:")
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap_or(0);

                        if token_count < min_tokens {
                            all_found = false;
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!("    Expected at least {} tokens, got {}", min_tokens, token_count);
                            }
                        }
                    }
                }
            } else {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    No semantic tokens data found in output");
                }
            }
        } else {
            all_found = false;
            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                eprintln!("    Failed to parse semantic tokens JSON output");
            }
        }
    } else if test.test_type == "signature_help" {
        // For signature_help, check the signature in the output.
        // The LSP may return multiple overloads under `signatures`; an
        // expected substring passes if it's found in *any* of them. This
        // lets overload-heavy tests assert on the right arm even when
        // overload resolution picks a different one as active.
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let labels: Vec<String> = if let Some(sig) =
                json.get("signature").and_then(|s| s.as_str())
            {
                vec![sig.to_string()]
            } else if let Some(sigs) = json.get("signatures").and_then(|s| s.as_array()) {
                sigs.iter()
                    .filter_map(|s| s.get("label").and_then(|l| l.as_str()).map(|s| s.to_string()))
                    .collect()
            } else {
                Vec::new()
            };

            let active_idx = json
                .get("activeSignature")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let active_label = labels.get(active_idx).cloned().unwrap_or_default();

            if test.expected.is_empty() {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!(
                        "    signature_help test has an empty EXPECTED block — \
                         assertion would pass vacuously, treating as failure"
                    );
                }
            }
            for expected_item in &test.expected {
                // `ACTIVE: ...` asserts against the active signature only —
                // proves overload resolution picked the right arm. Plain
                // lines match any returned signature.
                let (needle, search_target): (&str, &[String]) =
                    if let Some(rest) = expected_item.strip_prefix("ACTIVE:") {
                        (rest.trim(), std::slice::from_ref(&active_label))
                    } else {
                        (expected_item.as_str(), labels.as_slice())
                    };
                let found = search_target.iter().any(|l| l.contains(needle));
                if !found {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Expected '{}' not found (searched {:?})", needle, search_target);
                        eprintln!("    All signatures: {:?}", labels);
                    }
                }
            }
        } else {
            // Fallback to string matching
            for expected_item in &test.expected {
                if !stdout.contains(expected_item) {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Expected '{}' not found in output", expected_item);
                    }
                }
            }
        }
    } else if test.test_type == "inlay_hints" {
        // Parse `«hint::LABEL»` markers out of the expected source and
        // assert each (line, char, label) triple appears in the LSP's
        // `hints` array. Subset semantics — extra hints are allowed; every
        // expected hint must be found.
        let expected_hints = parse_hint_markers(&test.expected);
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let actual: Vec<(u64, u64, String)> = json
                .get("hints")
                .and_then(|h| h.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|h| {
                            let line = h.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_u64())?;
                            let ch = h.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_u64())?;
                            let label = h.get("label").and_then(|v| v.as_str())?.to_string();
                            Some((line, ch, label))
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Match on (line, normalised_label). Column is intentionally
            // loose — test files place `«hint:...»` right before the value,
            // but the LSP reports `position` differently for type hints
            // (padding_left, before the value) vs parameter hints
            // (padding_right, at the leading whitespace), so column
            // equality is off by one in some cases. Normalised label
            // strips a leading `:` so that `: int` (type-hint LSP label)
            // and `«hint:: int»` (marker content `: int`) both match, as
            // does `«hint::key:»` (marker content `:key:`) vs `key:`.
            for (line, _ch, label) in &expected_hints {
                let norm_exp = normalise_hint_label(label);
                let found = actual.iter().any(|(al, _ac, alabel)| {
                    al == line && normalise_hint_label(alabel) == norm_exp
                });
                if !found {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!(
                            "    Expected inlay hint on line {} with label {:?} (normalised {:?}) not found",
                            line, label, norm_exp
                        );
                        eprintln!("    Actual hints: {:?}", actual);
                    }
                }
            }
            if expected_hints.is_empty() {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    No «hint::...» markers parsed from EXPECTED block");
                }
            }
        } else {
            all_found = false;
            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                eprintln!("    Failed to parse inlay-hints JSON output");
            }
        }
    } else if test.test_type == "hover" {
        // For hover, check the hover contents
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            // Get the hover contents
            let hover_contents = json.get("hover")
                .and_then(|h| h.get("contents"))
                .and_then(|c| {
                    // Handle both string and MarkupContent formats
                    if let Some(s) = c.as_str() {
                        Some(s.to_string())
                    } else if let Some(markup) = c.as_object() {
                        markup.get("value").and_then(|v| v.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                });

            if let Some(contents) = hover_contents {
                // Join all expected lines into a single expected string
                let expected_full = test.expected.join("\n");

                // Normalize whitespace for comparison
                let contents_normalized = contents.trim();
                let expected_normalized = expected_full.trim();

                if contents_normalized != expected_normalized {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Hover content mismatch");
                        eprintln!("    Expected:");
                        eprintln!("{}", expected_normalized);
                        eprintln!("    Actual:");
                        eprintln!("{}", contents_normalized);
                    }
                }
            } else {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    No hover contents found in output");
                    eprintln!("    Actual output: {}", stdout);
                }
            }
        } else {
            all_found = false;
            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                eprintln!("    Failed to parse hover JSON output");
                eprintln!("    Actual output: {}", stdout);
            }
        }
    } else if test.test_type == "document_symbols" || test.test_type == "workspace_symbols" {
        // For symbol tests, check the symbol names and kinds
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let symbols = if let Some(syms) = json.get("symbols").and_then(|s| s.as_array()) {
                syms.clone()
            } else if json.as_array().is_some() {
                json.as_array().unwrap().clone()
            } else {
                Vec::new()
            };

            for expected_item in &test.expected {
                // Expected format: "SymbolName: SymbolKind"
                let parts: Vec<&str> = expected_item.split(':').collect();
                if parts.len() == 2 {
                    let expected_name = parts[0].trim();
                    let expected_kind = parts[1].trim();

                    let found = symbols.iter().any(|sym| {
                        let name_matches = sym.get("name")
                            .and_then(|n| n.as_str())
                            .map(|n| n == expected_name)
                            .unwrap_or(false);

                        let kind_matches = sym.get("kind")
                            .and_then(|k| k.as_str())
                            .map(|k| k.to_lowercase().contains(&expected_kind.to_lowercase()))
                            .unwrap_or(false);

                        name_matches && kind_matches
                    });

                    if !found {
                        all_found = false;
                        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                            eprintln!("    Expected symbol '{}' of kind '{}' not found", expected_name, expected_kind);
                        }
                    }
                } else {
                    // Just check for symbol name
                    let found = symbols.iter().any(|sym| {
                        sym.get("name")
                            .and_then(|n| n.as_str())
                            .map(|n| n == expected_item)
                            .unwrap_or(false)
                    });

                    if !found {
                        all_found = false;
                        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                            eprintln!("    Expected symbol '{}' not found", expected_item);
                        }
                    }
                }
            }
        } else {
            // Fallback to string matching
            for expected_item in &test.expected {
                if !stdout.contains(expected_item) {
                    all_found = false;
                    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                        eprintln!("    Expected '{}' not found in output", expected_item);
                    }
                }
            }
        }
    } else if test.test_type == "rename" {
        // For rename tests, apply the text edits and check the resulting code
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            if let Some(changes) = json.get("changes").and_then(|c| c.as_object()) {
                // Multi-file rename: apply edits per file, compare against
                // expected per-file content.
                if !test.expected_files.is_empty() {
                    for (fname, expected_content) in &test.expected_files {
                        // Skip non-rock manifest entries that the renamer never touches.
                        if fname == "project.toml" || fname == "stick.toml" {
                            continue;
                        }
                        let original = test
                            .files
                            .iter()
                            .find(|(n, _)| n == fname)
                            .map(|(_, c)| c.clone())
                            .unwrap_or_default();
                        // Find edits whose URI ends with `/<fname>`.
                        let edits_for_file: Vec<&serde_json::Value> = changes
                            .iter()
                            .filter(|(uri, _)| uri.ends_with(&format!("/{}", fname)))
                            .filter_map(|(_, edits)| edits.as_array())
                            .flat_map(|arr| arr.iter())
                            .collect();
                        let actual = apply_text_edits(&original, &edits_for_file);
                        let expected_norm = expected_content.trim_end().to_string();
                        let actual_norm = actual.trim_end().to_string();
                        if expected_norm != actual_norm {
                            all_found = false;
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!("    Rename mismatch in file {}", fname);
                                eprintln!("    Expected:\n{}", expected_norm);
                                eprintln!("    Actual:\n{}", actual_norm);
                            }
                        }
                    }
                } else {
                // Single-file rename: apply all edits to one result buffer,
                // then compare -- exactly against `expected_code`, or by
                // substring for `- snippet` style expectations.
                let mut result_code = test.code.clone();

                for (_file_uri, edits) in changes {
                    if let Some(edits_array) = edits.as_array() {
                        // Sort edits by position in reverse order (so we apply from end to start)
                        let mut edits_sorted: Vec<_> = edits_array.iter().collect();
                        edits_sorted.sort_by(|a, b| {
                            let start_a = a.get("range")
                                .and_then(|r| r.get("start"))
                                .map(|s| {
                                    let line = s.get("line").and_then(|l| l.as_u64()).unwrap_or(0);
                                    let char = s.get("character").and_then(|c| c.as_u64()).unwrap_or(0);
                                    (line, char)
                                })
                                .unwrap_or((0, 0));
                            let start_b = b.get("range")
                                .and_then(|r| r.get("start"))
                                .map(|s| {
                                    let line = s.get("line").and_then(|l| l.as_u64()).unwrap_or(0);
                                    let char = s.get("character").and_then(|c| c.as_u64()).unwrap_or(0);
                                    (line, char)
                                })
                                .unwrap_or((0, 0));
                            // Reverse order - apply from end to start
                            start_b.cmp(&start_a)
                        });

                        // Apply edits in reverse order
                        for edit in edits_sorted {
                            if let (Some(new_text), Some(range)) = (
                                edit.get("new_text").and_then(|t| t.as_str()),
                                edit.get("range")
                            ) {
                                let start_line = range.get("start")
                                    .and_then(|s| s.get("line"))
                                    .and_then(|l| l.as_u64())
                                    .unwrap_or(0) as usize;
                                let start_char = range.get("start")
                                    .and_then(|s| s.get("character"))
                                    .and_then(|c| c.as_u64())
                                    .unwrap_or(0) as usize;
                                let end_line = range.get("end")
                                    .and_then(|e| e.get("line"))
                                    .and_then(|l| l.as_u64())
                                    .unwrap_or(0) as usize;
                                let end_char = range.get("end")
                                    .and_then(|e| e.get("character"))
                                    .and_then(|c| c.as_u64())
                                    .unwrap_or(0) as usize;

                                // Apply the edit
                                let lines: Vec<&str> = result_code.lines().collect();
                                let mut new_lines: Vec<String> = Vec::new();

                                for (i, line) in lines.iter().enumerate() {
                                    if i < start_line || i > end_line {
                                        new_lines.push(line.to_string());
                                    } else if i == start_line && i == end_line {
                                        // Edit is on a single line
                                        let before = &line[..start_char.min(line.len())];
                                        let after = &line[end_char.min(line.len())..];
                                        new_lines.push(format!("{}{}{}", before, new_text, after));
                                    } else if i == start_line {
                                        let before = &line[..start_char.min(line.len())];
                                        new_lines.push(format!("{}{}", before, new_text));
                                    } else if i == end_line {
                                        let after = &line[end_char.min(line.len())..];
                                        // Append to the last line
                                        if let Some(last) = new_lines.last_mut() {
                                            last.push_str(after);
                                        }
                                    }
                                    // Lines between start and end are removed
                                }

                                result_code = new_lines.join("\n");
                            }
                        }
                    }
                }

                // Now check if expected snippets are in the result
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    Result code after rename:");
                    for (i, line) in result_code.lines().enumerate() {
                        eprintln!("      {}: {}", i, line);
                    }
                }

                if let Some(expected_code) = &test.expected_code {
                    let expected_norm = expected_code.trim_end();
                    let actual_norm = result_code.trim_end();
                    if expected_norm != actual_norm {
                        all_found = false;
                        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                            eprintln!("    Rename mismatch");
                            eprintln!("    Expected:\n{}", expected_norm);
                            eprintln!("    Actual:\n{}", actual_norm);
                        }
                    }
                } else {
                    for expected_item in &test.expected {
                        if !result_code.contains(expected_item) {
                            all_found = false;
                            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                                eprintln!("    Expected '{}' not found in output", expected_item);
                            }
                        }
                    }
                }
                } // end single-file rename branch
            } else {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    No changes found in rename output");
                }
            }
        } else {
            all_found = false;
            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                eprintln!("    Failed to parse rename JSON output");
            }
        }
    } else {
        // For other test types, use simple string matching
        for expected_item in &test.expected {
            if !stdout.contains(expected_item) {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!("    Expected '{}' not found in output", expected_item);
                }
            }
        }
    }

    // Debug output if verbose and test failed
    if std::env::var("ROCK_TEST_VERBOSE").is_ok() && !all_found {
        eprintln!("    Actual output: {}", stdout);
    }

    Ok(all_found)
}

// ── Edit Sequence Test Runner (JSON-RPC over stdio) ──────────────────────────

/// Helper: encode a JSON-RPC message with Content-Length header
fn encode_jsonrpc(json: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_string(json).unwrap();
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

/// Helper: read one JSON-RPC message from a BufReader
fn read_jsonrpc(reader: &mut BufReader<std::process::ChildStdout>) -> Result<serde_json::Value> {
    // Read headers until empty line
    let mut content_length: usize = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some(len_str) = header.strip_prefix("Content-Length:") {
            content_length = len_str.trim().parse()?;
        }
    }

    if content_length == 0 {
        return Err(anyhow::anyhow!("No Content-Length in JSON-RPC response"));
    }

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    Ok(value)
}

/// Reads from the dedicated reader thread spawned by `run_edit_sequence_test`
/// instead of directly from stdout, so the server never blocks waiting for
/// us to drain. See the long comment at the spawn site for the pipe-buffer
/// deadlock this avoids (`edit_sequence_stress` non-determinism).
fn read_response_from_chan(rx: &std::sync::mpsc::Receiver<serde_json::Value>, expected_id: i64) -> Result<serde_json::Value> {
    loop {
        let msg = rx.recv().map_err(|e| anyhow::anyhow!("LSP reader channel closed: {}", e))?;
        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
            if id == expected_id {
                return Ok(msg);
            }
        }
    }
}

/// Run a single edit sequence test against a persistent LSP server
fn file_uri(path: &Path) -> Result<String> {
    let path = path.to_str().ok_or_else(|| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    Ok(lsp_types::Uri::from_file_path(path)?.as_str().to_string())
}

/// The file every server this run spawns appends its stderr to. Unique per
/// run, so concurrent runs (or another user's) never share or clobber it;
/// named once on stderr so a hang or panic leaves a findable breadcrumb.
fn server_stderr_log() -> Result<PathBuf> {
    static LOG: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let mut log = LOG.lock().map_err(|_| anyhow::anyhow!("LSP server stderr log lock poisoned"))?;
    if let Some(path) = &*log {
        return Ok(path.clone());
    }
    let path = new_server_stderr_log(&std::env::temp_dir())?;
    eprintln!("LSP server stderr: {}", path.display());
    *log = Some(path.clone());
    Ok(path)
}

fn new_server_stderr_log(dir: &Path) -> Result<PathBuf> {
    let file = tempfile::Builder::new()
        .prefix("rock-lsp-test-stderr-")
        .suffix(".log")
        .tempfile_in(dir)
        .map_err(|e| anyhow::anyhow!("cannot create the LSP server stderr log in {}: {e}", dir.display()))?;
    let (_, path) = file.keep().map_err(|e| anyhow::anyhow!("cannot keep the LSP server stderr log: {e}"))?;
    Ok(path)
}

fn run_edit_sequence_test(test: &EditSequenceTest, backend: crate::Backend) -> Result<(usize, usize, Vec<String>)> {
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let mut passed = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    // Create temp file for the test
    let test_dir = PathBuf::from(".lsp-test-tmp");
    std::fs::create_dir_all(&test_dir)?;
    let test_file = test_dir.join(format!(
        "{}.rock",
        test.name.chars().map(|c| if c == ' ' || c == '/' { '_' } else { c }).collect::<String>()
    ));
    let test_uri = file_uri(&std::fs::canonicalize(&test_dir)?.join(test_file.file_name().unwrap()))?;

    // Write initial content so the LSP can find it
    let initial_code = match &test.steps[0].mode {
        StepMode::FullReplace { code } => code.clone(),
        StepMode::TypeAt { .. } | StepMode::DeleteAt { .. } => {
            // First step must be FullReplace to establish the initial document
            return Err(anyhow::anyhow!("First step in edit_sequence must be a full document (STEP N:), not a typing/delete step"));
        }
    };
    std::fs::write(&test_file, &initial_code)?;

    // Start the LSP server subprocess. Route stderr to a known-location
    // log file so that when the server hangs / panics during a test, the
    // operator has a breadcrumb. `Stdio::null()` here previously hid real
    // server bugs (e.g. `Failed to create URI for diagnostics: <unknown>`
    // spamming during stress tests went unnoticed for months).
    let stderr_log_path = server_stderr_log()?;
    let stderr_for = || -> std::io::Result<Stdio> {
        Ok(Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_log_path)?,
        ))
    };
    // `rock lsp serve` is the interactive server (no separate `rock-lsp`
    // binary since 6072750d1; bare `rock lsp` also defaults to serve).
    let rock_bin = lsp_server_bin()?;
    let mut cmd = Command::new(&rock_bin);
    cmd.args(["lsp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_for()?);
    if let Some((k, v)) = lsp_child_backend_env(backend) {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;

    // CRITICAL: drain stdout in a dedicated thread. The lsp-server crate uses
    // bounded(0) crossbeam channels for stdio, so its writer thread blocks on
    // `stdout.write_all` whenever the OS pipe is full — and the server's
    // compile path itself blocks on `connection.sender.send()` once the
    // writer is stuck. If this test client only drains stdout when it calls
    // `read_response`, a stress test that fires hundreds of `didChange`
    // notifications without an intervening request can fill the 64 KiB pipe
    // buffer (~5 KiB diagnostic notifications × 12+ unread keystrokes is
    // enough), wedge the server mid-`send`, and produce the
    // `edit_sequence_stress` non-determinism investigated in TODO.md
    // ("Sentinel: edit_sequence_stress"). The drainer below stuffs every
    // incoming message onto an unbounded channel; `read_response` reads
    // from the channel instead of from stdout directly, so the server
    // never blocks waiting for us.
    let (msg_tx, msg_rx) = std::sync::mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_jsonrpc(&mut reader) {
                Ok(msg) => {
                    if msg_tx.send(msg).is_err() {
                        return; // receiver dropped
                    }
                }
                Err(_) => return, // pipe closed / parse error / EOF
            }
        }
    });

    // Watchdog: kill the rock-lsp subprocess if it doesn't finish the test
    // within the timeout. Without this, a wedged server would block
    // `read_response` forever (no read-deadline on the pipe), and the
    // parallel runner would stall the whole suite indefinitely. See
    // sentinel in TODO.md for the racy stress-test that motivated this.
    let timeout_secs: u64 = std::env::var("ROCK_LSP_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let child_pid = child.id();
    let test_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_done = test_done.clone();
    let watchdog_name = test.name.clone();
    std::thread::spawn(move || {
        let start = bone_core::prelude::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(timeout_secs) {
            if watchdog_done.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !watchdog_done.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[lsp-test watchdog] killing pid {} after {}s — test '{}' wedged",
                child_pid, timeout_secs, watchdog_name
            );
            #[cfg(unix)]
            unsafe {
                libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
            }
        }
    });

    let mut next_id: i64 = 1;
    let mut version: i64 = 1;

    // --- Initialize ---
    let init_id = next_id;
    next_id += 1;
    let init_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": init_id,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": [],
                        "tokenModifiers": []
                    }
                }
            },
            "rootUri": file_uri(&std::fs::canonicalize(&test_dir)?)?,
            "workspaceFolders": null
        }
    });
    stdin.write_all(&encode_jsonrpc(&init_request))?;
    stdin.flush()?;

    let _init_response = read_response_from_chan(&msg_rx, init_id)?;

    // Send initialized notification
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    stdin.write_all(&encode_jsonrpc(&initialized))?;
    stdin.flush()?;

    // --- Open document ---
    let mut current_doc = initial_code.clone();

    let did_open = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": test_uri,
                "languageId": "rock",
                "version": version,
                "text": current_doc
            }
        }
    });
    stdin.write_all(&encode_jsonrpc(&did_open))?;
    stdin.flush()?;

    // No sleep needed: the LSP server compiles synchronously inside didChange/didOpen
    // (see reload_sources_on_change_workspace in compile.rs). The next request we send
    // won't be read until compilation finishes, so read_response is the sync point.
    //
    // If compilation ever becomes async, this will need a sync mechanism — either a
    // "compilation finished" notification from the server, or a poll loop that retries
    // the query until the result stabilizes.

    /// Helper: send a didChange with full document text
    fn send_did_change(
        stdin: &mut std::process::ChildStdin,
        uri: &str,
        version: i64,
        text: &str,
    ) -> Result<()> {
        let did_change = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "version": version
                },
                "contentChanges": [
                    { "text": text }
                ]
            }
        });
        stdin.write_all(&encode_jsonrpc(&did_change))?;
        stdin.flush()?;
        Ok(())
    }

    /// Helper: insert a character at (line, col) in a document string
    fn insert_char_at(doc: &str, line: usize, col: usize, ch: char) -> String {
        let mut lines: Vec<String> = doc.lines().map(|l| l.to_string()).collect();
        // Ensure enough lines exist
        while lines.len() <= line {
            lines.push(String::new());
        }
        let target_line = &mut lines[line];
        // Clamp col to line length
        let col = col.min(target_line.len());
        target_line.insert(col, ch);
        lines.join("\n")
    }

    /// Helper: delete a character at (line, col-1) in a document string (backspace)
    fn delete_char_at(doc: &str, line: usize, col: usize) -> (String, usize, usize) {
        let mut lines: Vec<String> = doc.lines().map(|l| l.to_string()).collect();
        if col > 0 && line < lines.len() {
            let col = col.min(lines[line].len());
            if col > 0 {
                lines[line].remove(col - 1);
                return (lines.join("\n"), line, col - 1);
            }
        }
        // Backspace at col 0: join with previous line
        if line > 0 && line < lines.len() {
            let prev_len = lines[line - 1].len();
            let current = lines.remove(line);
            lines[line - 1].push_str(&current);
            return (lines.join("\n"), line - 1, prev_len);
        }
        (lines.join("\n"), line, col)
    }

    // --- Walk through each step ---
    for (step_idx, step) in test.steps.iter().enumerate() {
        // Apply the step's edit to the document
        if step_idx > 0 {
            match &step.mode {
                StepMode::FullReplace { code } => {
                    version += 1;
                    current_doc = code.clone();
                    std::fs::write(&test_file, &current_doc)?;
                    send_did_change(&mut stdin, &test_uri, version, &current_doc)?;
                }
                StepMode::TypeAt { line, col, text } => {
                    let mut cursor_line = *line;
                    let mut cursor_col = *col;

                    if verbose {
                        eprintln!("    Step {}: typing {} chars at {}:{}", step_idx + 1, text.len(), line, col);
                    }

                    for ch in text.chars() {
                        version += 1;
                        if ch == '\n' {
                            // Simulate pressing Enter: split the line at cursor
                            let mut lines: Vec<String> = current_doc.lines().map(|l| l.to_string()).collect();
                            while lines.len() <= cursor_line {
                                lines.push(String::new());
                            }
                            let col_clamped = cursor_col.min(lines[cursor_line].len());
                            let rest = lines[cursor_line][col_clamped..].to_string();
                            lines[cursor_line].truncate(col_clamped);
                            lines.insert(cursor_line + 1, rest);
                            current_doc = lines.join("\n");
                            cursor_line += 1;
                            cursor_col = 0;
                        } else {
                            current_doc = insert_char_at(&current_doc, cursor_line, cursor_col, ch);
                            cursor_col += ch.len_utf8();
                        }
                        send_did_change(&mut stdin, &test_uri, version, &current_doc)?;
                    }

                    std::fs::write(&test_file, &current_doc)?;
                }
                StepMode::DeleteAt { line, col, count } => {
                    let mut cursor_line = *line;
                    let mut cursor_col = *col;

                    if verbose {
                        eprintln!("    Step {}: deleting {} chars at {}:{}", step_idx + 1, count, line, col);
                    }

                    for _ in 0..*count {
                        version += 1;
                        let (new_doc, new_line, new_col) = delete_char_at(&current_doc, cursor_line, cursor_col);
                        current_doc = new_doc;
                        cursor_line = new_line;
                        cursor_col = new_col;
                        send_did_change(&mut stdin, &test_uri, version, &current_doc)?;
                    }

                    std::fs::write(&test_file, &current_doc)?;
                }
            }
        }

        // Run checks for this step
        for check in &step.checks {
            let check_passed = match &check.operation {
                EditStepOperation::SemanticTokens => {
                    let req_id = next_id;
                    next_id += 1;
                    let request = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "textDocument/semanticTokens/full",
                        "params": {
                            "textDocument": { "uri": test_uri }
                        }
                    });
                    stdin.write_all(&encode_jsonrpc(&request))?;
                    stdin.flush()?;

                    let response = read_response_from_chan(&msg_rx, req_id)?;
                    check_semantic_tokens_response(&response, &check.expected, step_idx, verbose)
                }
                EditStepOperation::Diagnostics => {
                    // Diagnostics come as notifications -- for now just check no crash
                    // TODO: capture diagnostics from notifications between steps
                    if verbose {
                        eprintln!("    Step {}: diagnostics check (pass-through)", step_idx + 1);
                    }
                    true
                }
                EditStepOperation::Completion { line, character } => {
                    let req_id = next_id;
                    next_id += 1;
                    let request = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "textDocument/completion",
                        "params": {
                            "textDocument": { "uri": test_uri },
                            "position": { "line": line, "character": character }
                        }
                    });
                    stdin.write_all(&encode_jsonrpc(&request))?;
                    stdin.flush()?;

                    let response = read_response_from_chan(&msg_rx, req_id)?;
                    check_completion_response(&response, &check.expected, step_idx, verbose)
                }
                EditStepOperation::Hover { line, character } => {
                    let req_id = next_id;
                    next_id += 1;
                    let request = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "textDocument/hover",
                        "params": {
                            "textDocument": { "uri": test_uri },
                            "position": { "line": line, "character": character }
                        }
                    });
                    stdin.write_all(&encode_jsonrpc(&request))?;
                    stdin.flush()?;

                    let response = read_response_from_chan(&msg_rx, req_id)?;
                    check_hover_response(&response, &check.expected, step_idx, verbose)
                }
                EditStepOperation::References { line, character } => {
                    let req_id = next_id;
                    next_id += 1;
                    let request = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "textDocument/references",
                        "params": {
                            "textDocument": { "uri": test_uri },
                            "position": { "line": line, "character": character },
                            "context": { "includeDeclaration": true }
                        }
                    });
                    stdin.write_all(&encode_jsonrpc(&request))?;
                    stdin.flush()?;

                    let response = read_response_from_chan(&msg_rx, req_id)?;
                    check_references_response(&response, &check.expected, step_idx, verbose)
                }
            };

            if check_passed {
                passed += 1;
            } else {
                failed += 1;
                failures.push(format!("step {} {:?}", step_idx + 1, check.operation));
            }
        }
    }

    // --- Shutdown ---
    let shutdown_id = next_id;
    let shutdown_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": shutdown_id,
        "method": "shutdown",
        "params": null
    });
    stdin.write_all(&encode_jsonrpc(&shutdown_request))?;
    stdin.flush()?;

    // Try to read shutdown response, but don't fail if the server is slow
    let _ = read_response_from_chan(&msg_rx, shutdown_id);

    let exit_notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    });
    stdin.write_all(&encode_jsonrpc(&exit_notification))?;
    stdin.flush()?;

    // Wait for process to exit
    let _ = child.wait();

    // Stop the watchdog (we exited cleanly).
    test_done.store(true, std::sync::atomic::Ordering::Relaxed);

    // Clean up test file
    let _ = std::fs::remove_file(&test_file);

    Ok((passed, failed, failures))
}

/// Check semantic tokens in an LSP response
fn check_semantic_tokens_response(
    response: &serde_json::Value,
    expected: &[String],
    step_idx: usize,
    verbose: bool,
) -> bool {
    let result = response.get("result");

    // Count tokens from the response
    let token_count = result
        .and_then(|r| r.get("data"))
        .and_then(|d| d.as_array())
        .map(|arr| arr.len() / 5) // Each token is 5 integers
        .unwrap_or(0);

    if verbose {
        eprintln!("    Step {}: semantic_tokens count = {}", step_idx + 1, token_count);
    }

    let mut all_ok = true;
    for exp in expected {
        if let Some(min_str) = exp.strip_prefix("min_tokens:") {
            let min_tokens: usize = min_str.trim().parse().unwrap_or(0);
            if token_count < min_tokens {
                all_ok = false;
                if verbose {
                    eprintln!(
                        "    Step {}: FAIL - expected at least {} tokens, got {}",
                        step_idx + 1, min_tokens, token_count
                    );
                }
            }
        } else if exp == "non_zero" {
            if token_count == 0 {
                all_ok = false;
                if verbose {
                    eprintln!("    Step {}: FAIL - expected non-zero tokens, got 0", step_idx + 1);
                }
            }
        }
    }
    all_ok
}

/// Check completion items in an LSP response
fn check_completion_response(
    response: &serde_json::Value,
    expected: &[String],
    step_idx: usize,
    verbose: bool,
) -> bool {
    let items = response
        .get("result")
        .and_then(|r| {
            // CompletionList or array
            r.get("items").or(Some(r))
        })
        .and_then(|v| v.as_array());

    let labels: Vec<String> = items
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("label").and_then(|l| l.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    if verbose {
        eprintln!("    Step {}: completion labels = {:?}", step_idx + 1, labels);
    }

    let mut all_ok = true;
    for exp in expected {
        if !labels.iter().any(|l| l.contains(exp)) {
            all_ok = false;
            if verbose {
                eprintln!("    Step {}: FAIL - expected completion '{}' not found", step_idx + 1, exp);
            }
        }
    }
    all_ok
}

/// Check hover result in an LSP response
/// Validate a `textDocument/references` response: EXPECTED lines are `L:C`
/// start positions (0-based); the returned start set must match EXACTLY --
/// offset bugs show up as a set mismatch, which contains-matching would hide.
fn check_references_response(
    response: &serde_json::Value,
    expected: &[String],
    step_idx: usize,
    verbose: bool,
) -> bool {
    let mut got: Vec<(u64, u64)> = response
        .get("result")
        .and_then(|r| r.as_array())
        .map(|locs| {
            locs.iter()
                .filter_map(|loc| {
                    let start = loc.get("range")?.get("start")?;
                    Some((
                        start.get("line")?.as_u64()?,
                        start.get("character")?.as_u64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    got.sort();

    let mut want: Vec<(u64, u64)> = expected
        .iter()
        .filter_map(|l| {
            let (a, b) = l.trim().split_once(':')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .collect();
    want.sort();

    if verbose {
        eprintln!("    Step {}: references = {:?}", step_idx + 1, got);
    }
    let ok = got == want;
    if !ok && verbose {
        eprintln!(
            "    Step {}: FAIL - references mismatch: got {:?}, want {:?}",
            step_idx + 1,
            got,
            want
        );
    }
    ok
}

fn check_hover_response(
    response: &serde_json::Value,
    expected: &[String],
    step_idx: usize,
    verbose: bool,
) -> bool {
    let hover_text = response
        .get("result")
        .and_then(|r| r.get("contents"))
        .and_then(|c| {
            // MarkupContent or string
            c.get("value").and_then(|v| v.as_str())
                .or_else(|| c.as_str())
        })
        .unwrap_or("");

    if verbose {
        eprintln!("    Step {}: hover = {:?}", step_idx + 1, hover_text);
    }

    // `EXPECTED: none` -- the hover must be empty (used by sentinels pinning
    // a missing-hover bug; contains-matching can't express absence).
    if expected.len() == 1 && expected[0].trim().eq_ignore_ascii_case("none") {
        let ok = hover_text.is_empty();
        if !ok && verbose {
            eprintln!(
                "    Step {}: FAIL - expected empty hover, got {:?}",
                step_idx + 1,
                hover_text
            );
        }
        return ok;
    }

    let mut all_ok = true;
    for exp in expected {
        if !hover_text.contains(exp) {
            all_ok = false;
            if verbose {
                eprintln!("    Step {}: FAIL - expected '{}' in hover", step_idx + 1, exp);
            }
        }
    }
    all_ok
}

/// Run all tests (single-shot + edit_sequence) in a single .rock.lsp file.
/// Returns a `FileTestResult` for the shared parallel runner.
fn run_lsp_file(file: &Path, display_name: &str, backend: crate::Backend) -> FileTestResult {
    let start_time = bone_core::prelude::Instant::now();

    let tests = match parse_lsp_test_file(file) {
        Ok(t) => t,
        Err(e) => {
            return FileTestResult {
                file_name: display_name.to_string(),
                passed: 0,
                failed: 1,
                ignored: 0,
                duration: start_time.elapsed(),
                compilation_time: None,
                failed_test_details: vec![("parse".to_string(), e.to_string())],
            };
        }
    };

    let edit_seq_tests = match parse_edit_sequence_tests(file) {
        Ok(t) => t,
        Err(e) => {
            return FileTestResult {
                file_name: display_name.to_string(),
                passed: 0,
                failed: 1,
                ignored: 0,
                duration: start_time.elapsed(),
                compilation_time: None,
                failed_test_details: vec![("parse".to_string(), e.to_string())],
            };
        }
    };

    let mut file_passed = 0;
    let mut file_failed = 0;
    let mut failed_details = Vec::new();

    // Run regular single-shot tests
    for test in tests.iter() {
        match run_lsp_test(test, backend) {
            Ok(true) => {
                file_passed += 1;
            }
            Ok(false) => {
                file_failed += 1;
                failed_details.push((format!("{} ({})", test.name, test.test_type), String::new()));
            }
            Err(e) => {
                file_failed += 1;
                failed_details.push((format!("{} ({})", test.name, test.test_type), e.to_string()));
            }
        }
    }

    // Run edit sequence tests (persistent LSP server per test)
    for seq_test in edit_seq_tests.iter() {
        match run_edit_sequence_test(seq_test, backend) {
            Ok((_p, f, failure_details)) => {
                if f == 0 {
                    file_passed += 1;
                } else {
                    let desc = format!("{} (edit_sequence)", seq_test.name);
                    failed_details.push((desc, failure_details.join(", ")));
                    file_failed += 1;
                }
            }
            Err(e) => {
                file_failed += 1;
                let desc = format!("{} (edit_sequence)", seq_test.name);
                failed_details.push((desc, e.to_string()));
            }
        }
    }

    FileTestResult {
        file_name: display_name.to_string(),
        passed: file_passed,
        failed: file_failed,
        ignored: 0,
        duration: start_time.elapsed(),
        compilation_time: None,
        failed_test_details: failed_details,
    }
}

/// Compute a display name for a test file relative to the test root.
/// e.g. `tests/lsp/completion/basic.rock.lsp` relative to `tests/lsp/`
/// becomes `completion/basic.rock`. Files directly in the root become
/// just `basic.rock`.
fn display_name_for_lsp_file(file: &Path, root: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    // Strip the trailing .lsp extension (keeping .rock)
    let s = relative.to_string_lossy();
    s.strip_suffix(".lsp").unwrap_or(&s).to_string()
}

/// Run the `.rock.lsp` suite under `path` (a file or a tree containing
/// `tests/lsp`) if present, with the spawned `rock-lsp` child routed to
/// `backend`'s LSP. Returns `(total, passed, failed, ignored)`; zeros when
/// there are no fixtures. The single dispatch point shared by every backend's
/// `test` runner (rock's `do_test_multiple`, `skull_test`, `bone_test`) so the
/// "is there an LSP suite here + which dir + which backend" logic lives once.
pub fn run_lsp_suite(
    path: &Path,
    backend: crate::Backend,
    verbose: bool,
) -> Result<(usize, usize, usize, usize)> {
    if !has_lsp_tests(path) {
        return Ok((0, 0, 0, 0));
    }
    if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("lsp") {
        println!("\nRunning LSP test file {}...", path.display());
        run_lsp_tests(path, backend, verbose)
    } else if let Some(lsp_dir) = find_lsp_test_dir(path) {
        println!("\nRunning LSP tests in {}...", lsp_dir.display());
        run_lsp_tests(&lsp_dir, backend, verbose)
    } else {
        Ok((0, 0, 0, 0))
    }
}

pub fn run_lsp_tests(
    dir: &Path,
    backend: crate::Backend,
    _verbose: bool,
) -> Result<(usize, usize, usize, usize)> {
    // Find all .lsp files in the directory
    let mut lsp_files = Vec::new();
    let root_dir = if dir.is_file() {
        dir.parent().unwrap_or(dir).to_path_buf()
    } else {
        dir.to_path_buf()
    };

    if dir.is_file() && dir.extension().and_then(|s| s.to_str()) == Some("lsp") {
        lsp_files.push(dir.to_path_buf());
    } else if dir.is_dir() {
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("lsp") {
                    lsp_files.push(path.to_path_buf());
                }
            }
        }
    }

    // Skip files flagged as stress / slow unless explicitly enabled.
    // `*_stress.rock.lsp` is the convention. Currently the only entry,
    // `edit_sequence_stress.rock.lsp`, runs deterministically in ~3.3s
    // — fast enough that it could go in the default suite, but the rest
    // of the LSP suite finishes in ~2.8s so doubling that for one stress
    // file isn't worth the inner-loop cost. Set ROCK_LSP_RUN_STRESS=1
    // to opt in. (The historical non-determinism — 3s/90s wedges —
    // was a pipe-buffer deadlock in this test client; see the comment
    // at the reader-thread spawn in `run_edit_sequence_test`.)
    let run_stress = std::env::var("ROCK_LSP_RUN_STRESS").is_ok();
    if !run_stress {
        let before = lsp_files.len();
        lsp_files.retain(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.ends_with("_stress.rock.lsp"))
                .unwrap_or(false)
        });
        let skipped = before - lsp_files.len();
        if skipped > 0 {
            println!(
                "(skipped {} *_stress.rock.lsp file(s); set ROCK_LSP_RUN_STRESS=1 to include)",
                skipped
            );
        }
    }

    lsp_files.sort();

    if lsp_files.is_empty() {
        return Ok((0, 0, 0, 0));
    }

    let start_time = bone_core::prelude::Instant::now();

    let (total, passed, failed, ignored) = parallel_runner::run_files_parallel(
        lsp_files,
        None,
        move |file| {
            let display_name = display_name_for_lsp_file(file, &root_dir);
            run_lsp_file(file, &display_name, backend)
        },
    );

    // Print section summary (matches formatter/error test pattern)
    if total > 0 {
        use colored::Colorize;
        let result_text = if failed == 0 { "ok".green() } else { "FAILED".red() };
        println!(
            "\nLSP tests: {}. {} passed; {} failed; {} ignored; finished in {:.2}s",
            result_text, passed, failed, ignored, start_time.elapsed().as_secs_f64()
        );
    }

    Ok((total, passed, failed, ignored))
}
#[cfg(test)]
mod stderr_log_tests {
    use super::new_server_stderr_log;

    #[test]
    fn each_run_gets_its_own_server_stderr_log() {
        let dir = tempfile::tempdir().unwrap();
        let a = new_server_stderr_log(dir.path()).unwrap();
        let b = new_server_stderr_log(dir.path()).unwrap();
        assert_ne!(a, b);
        for p in [&a, &b] {
            assert!(p.starts_with(dir.path()) && p.is_file(), "{}", p.display());
        }
    }
}

#[cfg(test)]
mod server_bin_tests {
    use super::*;

    /// `ROCK_LSP_TEST_BIN` is process-global, so tests that set it take this
    /// lock and never observe each other's override.
    static BIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points the runner at `path` for the guard's lifetime, then restores the
    /// previous value.
    struct BinOverride {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for BinOverride {
        fn drop(&mut self) {
            // SAFETY: holding BIN_ENV_LOCK keeps other override-setting tests out.
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(LSP_TEST_BIN_ENV, value),
                    None => std::env::remove_var(LSP_TEST_BIN_ENV),
                }
            }
        }
    }

    fn override_server_bin(path: &Path) -> BinOverride {
        let lock = BIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os(LSP_TEST_BIN_ENV);
        // SAFETY: holding BIN_ENV_LOCK keeps other override-setting tests out.
        unsafe { std::env::set_var(LSP_TEST_BIN_ENV, path) };
        BinOverride { previous, _lock: lock }
    }

    /// Write an executable stub server that records each spawn in
    /// `<dir>/invocations` and prints `reply`, so a fixture's verdict comes
    /// from the runner's reply handling rather than from the compiler.
    fn stub_server(dir: &Path, reply: &str) -> PathBuf {
        let marker = dir.join("invocations");
        let path = dir.join("stub-rock");
        let quoted = reply.replace('\'', r"'\''");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\nprintf '%s' '{}'\n",
                marker.display(),
                quoted
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Run `fixture` (one `TEST:` block) against a stub server that prints
    /// `reply`. Returns the file result and how many times the stub was run.
    fn run_against_stub(fixture: &str, reply: &str) -> (FileTestResult, usize) {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_server(dir.path(), reply);
        let marker = dir.path().join("invocations");
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, fixture).unwrap();
        let _override = override_server_bin(&bin);
        let result = run_lsp_file(&path, "fixture", crate::Backend::Bone);
        let invocations = std::fs::read_to_string(&marker)
            .map(|log| log.lines().count())
            .unwrap_or(0);
        (result, invocations)
    }

    #[test]
    fn the_lsp_server_is_the_running_executable() {
        let _lock = BIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os(LSP_TEST_BIN_ENV);
        // SAFETY: holding BIN_ENV_LOCK keeps other override-setting tests out.
        unsafe { std::env::remove_var(LSP_TEST_BIN_ENV) };
        let bin = lsp_server_bin();
        // SAFETY: as above; restore before asserting so a panic keeps it clean.
        unsafe {
            match previous {
                Some(value) => std::env::set_var(LSP_TEST_BIN_ENV, value),
                None => std::env::remove_var(LSP_TEST_BIN_ENV),
            }
        }
        assert_eq!(
            bin.unwrap(),
            std::env::current_exe().unwrap(),
            "a fixture must run the executable that is running it"
        );
    }

    #[test]
    fn the_binary_override_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub_server(dir.path(), "{}");
        let _override = override_server_bin(&bin);
        assert_eq!(lsp_server_bin().unwrap(), bin);
    }

    #[test]
    fn an_override_that_is_not_a_file_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-rock");
        let _override = override_server_bin(&missing);
        let message = lsp_server_bin().unwrap_err().to_string();
        assert!(
            message.contains(LSP_TEST_BIN_ENV) && message.contains("no-such-rock"),
            "{message}"
        );
    }

    /// The stub records every spawn, so the fixture proves which binary ran.
    #[test]
    fn a_fixture_runs_on_the_overridden_binary() {
        let fixture = "TEST: stub completion\n\
TYPE: completion\n\
---\n\
fn main() {\n\
    let stub_only_symbol_xyz = 1;\n\
    let same = stub_only_«cursor»symbol_xyz;\n\
}\n\
---\n\
OPERATION: completion\n\
EXPECTED:\n\
- stub_only_symbol_xyz\n\
===\n";
        let (result, invocations) = run_against_stub(
            fixture,
            r#"{"completions": [{"label": "stub_only_symbol_xyz"}]}"#,
        );
        assert_eq!(
            (invocations, result.passed, result.failed),
            (1, 1, 0),
            "details: {:?}",
            result.failed_test_details
        );
    }
}
