use super::parallel_runner::{self, FileTestResult};
use anyhow::Result;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `ROCK_BACKEND` override (if any) for the *spawned* `rock-lsp` child running
/// `.rock.lsp` fixtures. The bone-backed LSP (`rock-lang/crates/rock-lsp/src/lib.rs`) is
/// the language-server front for the skull/bone stack, so `rock test
/// --backend {skull,bone}` runs fixtures through it; `--backend rock` uses the
/// legacy LSP unless `ROCK_LSP_TEST_BACKEND=bone` forces the bone path.
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
    test_type: String, // completion, goto_definition, hover, etc.
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
    /// For code actions: the title of the action to apply. Its result is
    /// compared against `expected_code`.
    action_title: Option<String>,
    /// For code actions: `NO_ACTION:` asserts that no action titled
    /// `action_title` is offered, so there is no result to compare.
    action_absent: bool,
    /// For rename: `REFUSED: <text>` asserts the server declines the rename
    /// with an error containing `<text>` and edits nothing.
    rename_refused: Option<String>,
}

#[derive(Debug, Clone)]
enum LspOperation {
    Completion {
        line: usize,
        character: usize,
    },
    GotoDefinition {
        line: usize,
        character: usize,
    },
    GotoTypeDefinition {
        line: usize,
        character: usize,
    },
    Hover {
        line: usize,
        character: usize,
    },
    References {
        line: usize,
        character: usize,
    },
    InlayHints,
    Rename {
        line: usize,
        character: usize,
        new_name: String,
    },
    SemanticTokens,
    SignatureHelp {
        line: usize,
        character: usize,
    },
    DocumentSymbols,
    WorkspaceSymbols {
        query: String,
    },
    CodeAction {
        line: usize,
        character: usize,
    },
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
    TypeAt {
        line: usize,
        col: usize,
        text: String,
    },
    /// Delete N characters backwards from (line, col) in the previous document
    DeleteAt {
        line: usize,
        col: usize,
        count: usize,
    },
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
    Completion {
        line: usize,
        character: usize,
    },
    Hover {
        line: usize,
        character: usize,
    },
    /// `CHECK: references(line, char)` -- EXPECTED lines are `LINE:CHAR`
    /// start positions; the returned set must match exactly (order-free).
    References {
        line: usize,
        character: usize,
    },
}

/// Check if the given path contains LSP tests
pub fn has_lsp_tests(path: &Path) -> bool {
    if path.is_file() {
        return path
            .extension()
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
                if entry_path.is_dir()
                    && entry_path.file_name().and_then(|s| s.to_str()) == Some("lsp")
                {
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

/// A malformed-fixture error naming the file and the test. Every input the
/// parser cannot read lands here: a fixture that is skipped instead would
/// drop out of the run and its absence would pass unnoticed.
fn fixture_error(path: &Path, name: &str, msg: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{}: test '{}': {}", path.display(), name, msg)
}

/// Consume a block's remaining lines through its `===` terminator (or the end
/// of the file). A `TEST:` line before `===` means the terminator is missing,
/// which would merge two fixtures.
fn skip_to_block_end<'a, I>(
    lines: &mut std::iter::Peekable<I>,
    path: &Path,
    name: &str,
) -> Result<()>
where
    I: Iterator<Item = &'a str>,
{
    while let Some(line) = lines.peek() {
        if line.starts_with("TEST:") {
            return Err(fixture_error(path, name, "block has no closing `===`"));
        }
        if lines.next().unwrap().starts_with("===") {
            return Ok(());
        }
    }
    Ok(())
}

/// Parse a single FILES: / FILE: block sequence until the next line starting
/// with one of `stop_prefixes`. Returns the collected files (name, content)
/// with all cursor markers stripped, plus the cursor position if any.
#[allow(clippy::type_complexity)]
fn parse_files_block<'a, I>(
    lines: &mut std::iter::Peekable<I>,
    stop_prefixes: &[&str],
    path: &Path,
    name: &str,
) -> Result<(
    Vec<(String, String)>,
    Option<(String, usize, usize)>, // (file_name, line, char)
)>
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
            let file_name = rest.trim().to_string();
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
                        cursor = Some((file_name.clone(), file_line_num, c));
                    }
                }
                content_lines.push(clean);
                file_line_num += 1;
            }
            files.push((file_name, content_lines.join("\n")));
        } else if line.trim().is_empty() {
            lines.next();
        } else {
            return Err(fixture_error(
                path,
                name,
                format!("expected `FILE:` in a files block, found {line:?}"),
            ));
        }
    }

    if files.is_empty() {
        return Err(fixture_error(
            path,
            name,
            "files block has no `FILE:` entries",
        ));
    }
    Ok((files, cursor))
}

/// The position a positional operation runs at: an explicit `POSITION:` line,
/// else the code block's cursor marker. A fixture with neither has nowhere to
/// send the request.
fn operation_position<'a, I>(
    lines: &mut std::iter::Peekable<I>,
    cursor: Option<(usize, usize)>,
    path: &Path,
    name: &str,
    op_type: &str,
) -> Result<(usize, usize)>
where
    I: Iterator<Item = &'a str>,
{
    if let Some(pos_line) = lines.next_if(|l| l.starts_with("POSITION:")) {
        return parse_position(pos_line).map_err(|e| fixture_error(path, name, e));
    }
    cursor.ok_or_else(|| {
        fixture_error(
            path,
            name,
            format!("OPERATION '{op_type}' needs a `POSITION:` line or a cursor marker"),
        )
    })
}

/// Whether a line outside any block is shaped like part of a fixture: a
/// `---`/`===` delimiter or an upper-case `KEYWORD:` such as `TYPE:`, or any
/// spelling of `test:`.
fn looks_like_directive(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with("---") || line.starts_with("===") {
        return true;
    }
    let Some((keyword, _)) = line.split_once(':') else {
        return false;
    };
    let keyword = keyword.trim();
    keyword.eq_ignore_ascii_case("test")
        || (!keyword.is_empty() && keyword.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
}

/// Parse an LSP test file
fn parse_lsp_test_file(path: &Path) -> Result<Vec<LspTest>> {
    let contents = fs_err::read_to_string(path)?;
    let mut tests = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(name) = line.strip_prefix("TEST:") else {
            // Prose notes between blocks are allowed. A line shaped like a
            // fixture directive is a mangled header, and the block under it
            // would be skipped along with the prose.
            if looks_like_directive(line) {
                return Err(anyhow::anyhow!(
                    "{}: expected `TEST:` before {line:?}",
                    path.display()
                ));
            }
            continue;
        };
        let name = name.trim().to_string();

        let test_type = match lines.next() {
            Some(next_line) if next_line.starts_with("TYPE:") => {
                next_line.strip_prefix("TYPE:").unwrap().trim().to_string()
            }
            Some(next_line) => {
                return Err(fixture_error(
                    path,
                    &name,
                    format!("expected `TYPE:` after `TEST:`, found {next_line:?}"),
                ));
            }
            None => return Err(fixture_error(path, &name, "file ends before `TYPE:`")),
        };

        // A TYPE the harness cannot decode is refused here, naming itself: a
        // dropped fixture is a vacuous pass, and comparing its raw reply
        // would accept output the server never shaped. `edit_sequence`
        // fixtures are read by `parse_edit_sequence_tests`.
        let type_kind = fixture_reply_kind(&test_type);
        if type_kind.is_none() && test_type != "edit_sequence" {
            return Err(anyhow::anyhow!(
                "{}: test '{}' has TYPE '{}', which has no reply decoder",
                path.display(),
                name,
                test_type
            ));
        }
        if test_type == "edit_sequence" {
            skip_to_block_end(&mut lines, path, &name)?;
            continue;
        }

        // Multi-file mode: `FILES:` after `TYPE:` instead of a code block.
        let is_multi_file = lines.peek().map(|l| l.trim() == "FILES:").unwrap_or(false);

        let mut files: Vec<(String, String)> = Vec::new();
        let mut primary_file = String::new();
        let mut cursor: Option<(usize, usize)> = None;
        let mut code = String::new();

        if is_multi_file {
            lines.next(); // consume `FILES:`
            let (parsed_files, cur) = parse_files_block(&mut lines, &["OPERATION:"], path, &name)?;
            if let Some((fname, l, c)) = cur {
                primary_file = fname.clone();
                cursor = Some((l, c));
                // Primary file code is the one holding the cursor marker.
                for (file_name, content) in &parsed_files {
                    if file_name == &fname {
                        code = content.clone();
                        break;
                    }
                }
            }
            files = parsed_files;
        } else {
            match lines.next() {
                Some(sep) if sep.starts_with("---") => {}
                Some(sep) => {
                    return Err(fixture_error(
                        path,
                        &name,
                        format!("expected `---` or `FILES:` after `TYPE:`, found {sep:?}"),
                    ));
                }
                None => {
                    return Err(fixture_error(
                        path,
                        &name,
                        "file ends before the code block",
                    ));
                }
            }

            // Collect code until the closing separator.
            let mut code_lines = Vec::new();
            let mut closed = false;
            for line in lines.by_ref() {
                if line.starts_with("---") {
                    closed = true;
                    break;
                }
                let (clean, cursor_pos) = strip_markers(line);
                if let Some(c) = cursor_pos {
                    if cursor.is_none() {
                        cursor = Some((code_lines.len(), c));
                    }
                }
                code_lines.push(clean);
            }
            if !closed {
                return Err(fixture_error(
                    path,
                    &name,
                    "file ends before the code block's closing `---`",
                ));
            }
            code = code_lines.join("\n");
        }

        let op_type = match lines.next() {
            Some(op_line) => match op_line.strip_prefix("OPERATION:") {
                Some(op) => op.trim(),
                None => {
                    return Err(fixture_error(
                        path,
                        &name,
                        format!("expected `OPERATION:` after the code block, found {op_line:?}"),
                    ));
                }
            },
            None => return Err(fixture_error(path, &name, "file ends before `OPERATION:`")),
        };
        let at = |lines: &mut std::iter::Peekable<_>| {
            operation_position(lines, cursor, path, &name, op_type)
        };
        let operation = match op_type {
            "inlay_hints" => LspOperation::InlayHints,
            "semantic_tokens" | "full_document" => LspOperation::SemanticTokens,
            "document_symbols" => LspOperation::DocumentSymbols,
            "workspace_symbols" => {
                let query = match lines.next_if(|l| l.starts_with("QUERY:")) {
                    Some(q) => q.strip_prefix("QUERY:").unwrap().trim().to_string(),
                    None => String::new(),
                };
                LspOperation::WorkspaceSymbols { query }
            }
            "completion" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::Completion { line, character }
            }
            "goto_definition" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::GotoDefinition { line, character }
            }
            "goto_type_definition" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::GotoTypeDefinition { line, character }
            }
            "hover" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::Hover { line, character }
            }
            "references" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::References { line, character }
            }
            "signature_help" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::SignatureHelp { line, character }
            }
            "code_action" => {
                let (line, character) = at(&mut lines)?;
                LspOperation::CodeAction { line, character }
            }
            "rename" => {
                let (line, character) = at(&mut lines)?;
                let new_name = match lines.next_if(|l| l.starts_with("NEW_NAME:")) {
                    Some(n) => n.strip_prefix("NEW_NAME:").unwrap().trim().to_string(),
                    None => "renamed".to_string(),
                };
                LspOperation::Rename {
                    line,
                    character,
                    new_name,
                }
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "{}: test '{}' has OPERATION '{}', which has no reply decoder",
                    path.display(),
                    name,
                    op_type
                ));
            }
        };

        // The TYPE names the decoder and the OPERATION names the request; a
        // fixture whose halves disagree would run one request through another
        // type's validator.
        if let Some(type_kind) = type_kind {
            if type_kind != operation.reply_kind() {
                return Err(anyhow::anyhow!(
                    "{}: test '{}' has TYPE '{}' but OPERATION '{}' answers a {:?} reply",
                    path.display(),
                    name,
                    test_type,
                    op_type,
                    operation.reply_kind()
                ));
            }
        }

        // Skip separator if present
        lines.next_if(|l| l.starts_with("---"));

        match lines.next() {
            Some(exp_line) if exp_line.starts_with("EXPECTED:") => {}
            Some(exp_line) => {
                return Err(fixture_error(
                    path,
                    &name,
                    format!("expected `EXPECTED:` after the operation, found {exp_line:?}"),
                ));
            }
            None => return Err(fixture_error(path, &name, "file ends before `EXPECTED:`")),
        }

        let mut expected = Vec::new();
        let mut expected_code: Option<String> = None;
        let mut rename_lines: Vec<String> = Vec::new();
        let mut expected_files: Vec<(String, String)> = Vec::new();
        let mut action_title: Option<String> = None;
        let mut action_absent = false;
        let mut rename_refused: Option<String> = None;
        let mut result_lines: Option<Vec<String>> = None;

        // Multi-file EXPECTED uses `FILE: name / --- / content / ---` blocks
        // up to `===`.
        let is_multi_expected = is_multi_file
            && lines
                .peek()
                .is_some_and(|l| l.starts_with("FILE:") || l.starts_with("---"));

        if is_multi_expected {
            let (parsed, _cur) = parse_files_block(&mut lines, &["===", "TEST:"], path, &name)?;
            expected_files = parsed;
            if lines.next_if(|l| l.starts_with("===")).is_none() && lines.peek().is_some() {
                return Err(fixture_error(path, &name, "block has no closing `===`"));
            }
        } else {
            // The last block may end at the end of the file without `===`.
            for line in lines.by_ref() {
                if line.starts_with("===") {
                    break;
                }
                if line.starts_with("TEST:") {
                    return Err(fixture_error(path, &name, "block has no closing `===`"));
                }
                // Every TYPE needs a branch here; a line no branch claims is
                // an error rather than an expectation that silently vanishes.
                if test_type == "rename" {
                    // Raw capture; snippet-vs-full-code is decided after the
                    // loop from the first non-empty line. `REFUSED:` stands
                    // alone, so it takes no code with it.
                    if let Some(reason) = line.strip_prefix("REFUSED:") {
                        if rename_refused.is_some() || !rename_lines.is_empty() {
                            return Err(fixture_error(
                                path,
                                &name,
                                "`REFUSED:` must be the whole EXPECTED block",
                            ));
                        }
                        rename_refused = Some(reason.trim().to_string());
                    } else if rename_refused.is_some() {
                        if !line.trim().is_empty() {
                            return Err(fixture_error(
                                path,
                                &name,
                                "`REFUSED:` must be the whole EXPECTED block",
                            ));
                        }
                    } else {
                        rename_lines.push(line.to_string());
                    }
                } else if test_type == "code_action" {
                    // `ACTION_TITLE: <title>`, then `RESULT:` and the whole
                    // file after the action's edit is applied; or a lone
                    // `NO_ACTION: <title>` for an action that must not be
                    // offered.
                    if let Some(result) = &mut result_lines {
                        result.push(line.to_string());
                    } else if let Some((title, absent)) = line
                        .strip_prefix("ACTION_TITLE:")
                        .map(|t| (t, false))
                        .or_else(|| line.strip_prefix("NO_ACTION:").map(|t| (t, true)))
                    {
                        if action_title.is_some() {
                            return Err(fixture_error(
                                path,
                                &name,
                                "more than one `ACTION_TITLE:` or `NO_ACTION:`",
                            ));
                        }
                        action_title = Some(title.trim().to_string());
                        action_absent = absent;
                    } else if line.trim() == "RESULT:" {
                        result_lines = Some(Vec::new());
                    } else if !line.trim().is_empty() {
                        return Err(fixture_error(
                            path,
                            &name,
                            format!(
                                "expected `ACTION_TITLE:`, `NO_ACTION:` or `RESULT:`, found {line:?}"
                            ),
                        ));
                    }
                } else if test_type == "hover" || test_type == "inlay_hints" {
                    // Hover: every line is expected content. Inlay hints: the
                    // block re-renders the source with `«hint::LABEL»`
                    // markers inline, parsed into (line, col, label) triples.
                    expected.push(line.to_string());
                } else if let Some(item) = line.strip_prefix("- ") {
                    expected.push(item.to_string());
                } else if line.starts_with("file:")
                    || line.starts_with("line:")
                    || line.starts_with("character:")
                    || line.starts_with("min_tokens:")
                {
                    expected.push(line.to_string());
                } else if (test_type == "goto_definition"
                    || test_type == "goto_type_definition"
                    || test_type == "references")
                    && line.trim().eq_ignore_ascii_case("none")
                {
                    // `EXPECTED: none` -- this request should return no
                    // targets (goto-def/type-def/references).
                    expected.push("none".to_string());
                } else if (test_type == "document_symbols" || test_type == "workspace_symbols")
                    && line.contains(':')
                {
                    // `Name: Kind`, the same item a `- Name: Kind` line gives.
                    expected.push(line.to_string());
                } else if test_type == "signature_help" && !line.trim().is_empty() {
                    // Bare signature strings like `greet(name: string,
                    // times: int)` match any returned signature. A line
                    // starting with `ACTIVE:` matches the *active* signature
                    // only (used to verify overload resolution picked the
                    // right arm).
                    expected.push(line.to_string());
                } else if !line.trim().is_empty() {
                    return Err(fixture_error(
                        path,
                        &name,
                        format!("unrecognised `{test_type}` expectation {line:?}"),
                    ));
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
            } else if rename_lines.iter().any(|l| !l.trim().is_empty()) {
                expected_code = Some(rename_lines.join("\n"));
            }
        }

        if test_type == "code_action" {
            if action_title.as_deref().is_none_or(str::is_empty) {
                return Err(fixture_error(
                    path,
                    &name,
                    "code action has no `ACTION_TITLE:` or `NO_ACTION:`",
                ));
            }
            match (action_absent, result_lines) {
                (true, Some(_)) => {
                    return Err(fixture_error(
                        path,
                        &name,
                        "`NO_ACTION:` has no result; drop `RESULT:`",
                    ));
                }
                (true, None) => {}
                (false, None) => {
                    return Err(fixture_error(path, &name, "code action has no `RESULT:`"));
                }
                (false, Some(result)) => expected_code = Some(result.join("\n")),
            }
        }

        // An EXPECTED block with nothing in it asserts nothing, so the
        // fixture would pass whatever the server answered.
        if expected.iter().all(|e| e.trim().is_empty())
            && expected_code.is_none()
            && expected_files.is_empty()
            && !action_absent
            && rename_refused.as_deref().is_none_or(str::is_empty)
        {
            return Err(fixture_error(path, &name, "EXPECTED block is empty"));
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
            action_title,
            action_absent,
            rename_refused,
        });
    }

    Ok(tests)
}

/// The two numbers of a `LINE:COL` or `LINE, COL` pair, or `None` when the text
/// is not exactly two unsigned integers.
fn parse_pair(text: &str, sep: char) -> Option<(usize, usize)> {
    let (a, b) = text.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// The step mode a `STEP N ...:` header names, with `body` as the text it
/// types or the document it installs.
fn parse_step_mode(header: &str, body: String) -> Option<StepMode> {
    let header = header.trim().strip_suffix(':')?;
    let Some(open) = header.find('(') else {
        return Some(StepMode::FullReplace { code: body });
    };
    let inner = header[open + 1..].strip_suffix(')')?.trim();
    if let Some(coords) = inner.strip_prefix("type at ") {
        let (line, col) = parse_pair(coords, ':')?;
        Some(StepMode::TypeAt {
            line,
            col,
            text: body,
        })
    } else if let Some(rest) = inner.strip_prefix("delete at ") {
        let (coords, count) = rest.split_once(',')?;
        let (line, col) = parse_pair(coords, ':')?;
        Some(StepMode::DeleteAt {
            line,
            col,
            count: count.trim().parse().ok()?,
        })
    } else {
        None
    }
}

/// The request a `CHECK:` line names, or `None` when it names none the runner
/// sends or its position does not parse.
fn parse_check_operation(op: &str) -> Option<EditStepOperation> {
    match op {
        "semantic_tokens" => return Some(EditStepOperation::SemanticTokens),
        "diagnostics" => return Some(EditStepOperation::Diagnostics),
        _ => {}
    }
    let (kind, args) = op.split_once('(')?;
    let (line, character) = parse_pair(args.strip_suffix(')')?, ',')?;
    match kind.trim() {
        "completion" => Some(EditStepOperation::Completion { line, character }),
        "hover" => Some(EditStepOperation::Hover { line, character }),
        "references" => Some(EditStepOperation::References { line, character }),
        _ => None,
    }
}

/// Parse edit sequence tests from a test file. `parse_lsp_test_file` has
/// already checked the block structure; this rejects malformed steps and
/// checks, which would otherwise drop out of the sequence unnoticed.
fn parse_edit_sequence_tests(path: &Path) -> Result<Vec<EditSequenceTest>> {
    let contents = std::fs::read_to_string(path)?;
    let mut tests = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(name) = line.strip_prefix("TEST:") else {
            continue;
        };
        let name = name.trim().to_string();

        let test_type = match lines.next() {
            Some(next_line) if next_line.starts_with("TYPE:") => {
                next_line.strip_prefix("TYPE:").unwrap().trim().to_string()
            }
            _ => return Err(fixture_error(path, &name, "expected `TYPE:` after `TEST:`")),
        };

        if test_type != "edit_sequence" {
            skip_to_block_end(&mut lines, path, &name)?;
            continue;
        }

        match lines.next() {
            Some(sep) if sep.starts_with("---") => {}
            other => {
                return Err(fixture_error(
                    path,
                    &name,
                    format!("expected `---` after `TYPE:`, found {other:?}"),
                ));
            }
        }

        let mut steps = Vec::new();

        // Parse steps until ===
        while let Some(next) = lines.next() {
            if next.starts_with("===") {
                break;
            }
            if next.trim().is_empty() || next.starts_with("---") {
                continue;
            }
            if !next.starts_with("STEP") {
                return Err(fixture_error(
                    path,
                    &name,
                    format!("expected `STEP` in an edit sequence, found {next:?}"),
                ));
            }

            // Formats:
            //   STEP N:                          -> FullReplace
            //   STEP N (type at LINE:COL):       -> TypeAt
            //   STEP N (delete at LINE:COL, N):  -> DeleteAt
            let step_line = next;

            // Collect body lines until ---
            let mut code_lines = Vec::new();
            for line in lines.by_ref() {
                if line.starts_with("---") {
                    break;
                }
                code_lines.push(line.to_string());
            }
            let mode = parse_step_mode(step_line, code_lines.join("\n")).ok_or_else(|| {
                fixture_error(path, &name, format!("malformed step header {step_line:?}"))
            })?;

            // Parse checks for this step
            let mut checks = Vec::new();
            while let Some(line) = lines.peek() {
                if line.starts_with("STEP") || line.starts_with("===") {
                    break;
                }
                let line = lines.next().unwrap();
                if line.trim().is_empty() || line.starts_with("---") {
                    continue;
                }
                let Some(op_str) = line.strip_prefix("CHECK:") else {
                    return Err(fixture_error(
                        path,
                        &name,
                        format!("expected `CHECK:` after a step, found {line:?}"),
                    ));
                };
                let op_str = op_str.trim();
                let operation = parse_check_operation(op_str).ok_or_else(|| {
                    fixture_error(path, &name, format!("malformed check `CHECK: {op_str}`"))
                })?;

                let mut expected = Vec::new();
                if lines.next_if(|l| l.starts_with("EXPECTED:")).is_some() {
                    while let Some(line) = lines.peek() {
                        if line.starts_with("---")
                            || line.starts_with("STEP")
                            || line.starts_with("===")
                            || line.starts_with("CHECK:")
                        {
                            break;
                        }
                        let line = lines.next().unwrap();
                        if let Some(item) = line.strip_prefix("- ") {
                            expected.push(item.to_string());
                        } else if !line.trim().is_empty() {
                            expected.push(line.to_string());
                        }
                    }
                }

                checks.push(EditStepCheck {
                    operation,
                    expected,
                });
            }

            steps.push(EditStep { mode, checks });
        }

        if steps.is_empty() {
            return Err(fixture_error(path, &name, "edit sequence has no steps"));
        }
        tests.push(EditSequenceTest { name, steps });
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
        let line = range
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0);
        let ch = range
            .and_then(|s| s.get("character"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0);
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

    let line = parts[0]
        .trim()
        .strip_prefix("line=")
        .ok_or_else(|| anyhow::anyhow!("Missing 'line=' in position"))?
        .parse::<usize>()?;

    let character = parts[1]
        .trim()
        .strip_prefix("character=")
        .ok_or_else(|| anyhow::anyhow!("Missing 'character=' in position"))?
        .parse::<usize>()?;

    Ok((line, character))
}

/// The binary every fixture request runs: the executable that is running this
/// suite, so a fixture always tests the build under test. Probing a working
/// directory for `target/debug/rock` (and falling back to `cargo run`) used to
/// pick a different build than the one running, and `cargo run` also made the
/// suite slow and flaky under parallel load.
fn lsp_server_bin() -> Result<PathBuf> {
    std::env::current_exe().map_err(|e| {
        anyhow::anyhow!("cannot find the running executable to use as the LSP server: {e}")
    })
}

/// The server a fixture talks to: the binary to spawn and the backend its
/// LSP routes to. Tests pass a stub binary here.
#[derive(Clone, Copy)]
struct LspServerCmd<'a> {
    bin: &'a Path,
    backend: crate::Backend,
}

impl LspServerCmd<'_> {
    /// `<bin> lsp` with the backend's environment. The one-shot handlers and
    /// the interactive server both live under `rock lsp`; there is no
    /// separate `rock-lsp` binary.
    fn command(&self) -> Command {
        let mut cmd = Command::new(self.bin);
        cmd.arg("lsp");
        if let Some((k, v)) = lsp_child_backend_env(self.backend) {
            cmd.env(k, v);
        }
        cmd
    }
}

/// The shape a fixture's reply must have, one variant per decoder. The
/// comparison in `run_lsp_test` matches on this, so a new request cannot be
/// added without a decoder to compare its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyKind {
    Completion,
    Definition,
    References,
    SemanticTokens,
    SignatureHelp,
    InlayHints,
    Hover,
    Symbols,
    Rename,
    CodeAction,
}

impl LspOperation {
    /// The decoder for the reply this request answers with. Total over the
    /// operations, so no request can reach the comparison without a shape.
    fn reply_kind(&self) -> ReplyKind {
        match self {
            LspOperation::Completion { .. } => ReplyKind::Completion,
            LspOperation::GotoDefinition { .. } | LspOperation::GotoTypeDefinition { .. } => {
                ReplyKind::Definition
            }
            LspOperation::Hover { .. } => ReplyKind::Hover,
            LspOperation::References { .. } => ReplyKind::References,
            LspOperation::InlayHints => ReplyKind::InlayHints,
            LspOperation::Rename { .. } => ReplyKind::Rename,
            LspOperation::SemanticTokens => ReplyKind::SemanticTokens,
            LspOperation::SignatureHelp { .. } => ReplyKind::SignatureHelp,
            LspOperation::DocumentSymbols | LspOperation::WorkspaceSymbols { .. } => {
                ReplyKind::Symbols
            }
            LspOperation::CodeAction { .. } => ReplyKind::CodeAction,
        }
    }
}

/// The decoder a fixture's `TYPE:` names, or `None` when the harness has none.
/// `edit_sequence` fixtures are read by `parse_edit_sequence_tests` instead.
fn fixture_reply_kind(test_type: &str) -> Option<ReplyKind> {
    Some(match test_type {
        "completion" => ReplyKind::Completion,
        "goto_definition" | "goto_type_definition" => ReplyKind::Definition,
        "references" => ReplyKind::References,
        // `full_document` is the older name for a whole-file token request; it
        // builds the same operation as `semantic_tokens`.
        "semantic_tokens" | "full_document" => ReplyKind::SemanticTokens,
        "signature_help" => ReplyKind::SignatureHelp,
        "inlay_hints" => ReplyKind::InlayHints,
        "hover" => ReplyKind::Hover,
        "document_symbols" | "workspace_symbols" => ReplyKind::Symbols,
        "rename" => ReplyKind::Rename,
        "code_action" => ReplyKind::CodeAction,
        _ => return None,
    })
}

/// The refusal half of a rename reply. A fixture that expects `REFUSED:` needs
/// an `error` containing its text and no edits; any other fixture fails on an
/// `error`, so an unexpected refusal is not mistaken for an empty rename.
fn check_rename_refusal(test: &LspTest, request: &str, json: &serde_json::Value) -> Result<()> {
    let error = json.get("error").and_then(|e| e.as_str());
    let edits = json
        .get("changes")
        .and_then(|c| c.as_object())
        .is_some_and(|c| {
            c.values()
                .any(|e| e.as_array().is_some_and(|a| !a.is_empty()))
        });
    match (&test.rename_refused, error) {
        (Some(want), Some(got)) if got.contains(want.as_str()) && !edits => Ok(()),
        (Some(want), Some(got)) if !got.contains(want.as_str()) => Err(anyhow::anyhow!(
            "{request}: rename refused with {got:?}, which does not contain {want:?}"
        )),
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "{request}: a refused rename still returned edits; raw reply: {json}"
        )),
        (Some(want), None) => Err(anyhow::anyhow!(
            "{request}: rename was not refused (expected {want:?}); raw reply: {json}"
        )),
        (None, Some(got)) => Err(anyhow::anyhow!("{request}: rename refused: {got}")),
        (None, None) => Ok(()),
    }
}

/// Decode a fixture reply. A reply is JSON by contract, so one that does not
/// decode is a server bug: it fails the fixture and quotes the raw text.
/// Falling back to a substring match would pass malformed output that happens
/// to contain the expected text.
fn decode_reply(request: &str, raw: &str) -> Result<serde_json::Value> {
    serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("{request}: reply is not JSON ({e}); raw reply: {raw}"))
}

/// The array every reply to `request` must carry under `key`. This is the shape
/// check that runs before any comparison, so a reply missing the field cannot
/// pass as an empty result.
fn reply_array<'a>(
    request: &str,
    raw: &str,
    json: &'a serde_json::Value,
    key: &str,
) -> Result<&'a Vec<serde_json::Value>> {
    json.get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("{request}: reply has no `{key}` array; raw reply: {raw}"))
}

/// The name of a symbol's `kind`. The server sends the numeric LSP
/// `SymbolKind`; a name is accepted as is.
fn symbol_kind_name(kind: &serde_json::Value) -> Option<&str> {
    if let Some(name) = kind.as_str() {
        return Some(name);
    }
    const NAMES: [&str; 26] = [
        "File",
        "Module",
        "Namespace",
        "Package",
        "Class",
        "Method",
        "Property",
        "Field",
        "Constructor",
        "Enum",
        "Interface",
        "Function",
        "Variable",
        "Constant",
        "String",
        "Number",
        "Boolean",
        "Array",
        "Object",
        "Key",
        "Null",
        "EnumMember",
        "Struct",
        "Event",
        "Operator",
        "TypeParameter",
    ];
    let index = usize::try_from(kind.as_u64()?).ok()?.checked_sub(1)?;
    NAMES.get(index).copied()
}

/// Run a single LSP test
fn run_lsp_test(test: &LspTest, server: LspServerCmd) -> Result<bool> {
    let (test_file, project_dir) = write_fixture_project(test)?;

    let Some(args) = one_shot_args(&test.operation, &test_file) else {
        let LspOperation::CodeAction { line, character } = test.operation else {
            unreachable!("only a code action has no one-shot subcommand");
        };
        let result = run_code_action_test(test, server, &test_file, line, character);
        let _ = fs_err::remove_dir_all(&project_dir);
        return result;
    };

    let output = server.command().args(&args).output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let _ = fs_err::remove_dir_all(&project_dir);

    // Check if there was a real error (not just warnings)
    // Only fail if we see actual error output patterns, not just the word "error" in warnings
    if !output.status.success() {
        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
            eprintln!(
                "    LSP command failed with exit code: {:?}",
                output.status.code()
            );
            if !stderr.is_empty() {
                eprintln!("    LSP stderr: {}", stderr);
            }
        }
        return Ok(false);
    }

    // Names the request in every decode and shape failure below.
    let request = format!("{} test '{}'", test.test_type, test.name);

    // Dispatch on the request, not the `TYPE:` spelling: a fixture that
    // reached `TYPE: full_document` used to fall through to a raw-output
    // substring match. `ReplyKind` has one arm per decoder.
    let all_found = match test.operation.reply_kind() {
        ReplyKind::Completion => check_completion_reply(test, &request, &stdout)?,
        ReplyKind::Definition => check_definition_reply(test, &request, &stdout)?,
        ReplyKind::References => check_references_reply(test, &request, &stdout)?,
        ReplyKind::SemanticTokens => check_semantic_tokens_reply(test, &request, &stdout)?,
        ReplyKind::SignatureHelp => check_signature_help_reply(test, &request, &stdout)?,
        ReplyKind::InlayHints => check_inlay_hints_reply(test, &request, &stdout)?,
        ReplyKind::Hover => check_hover_reply(test, &request, &stdout)?,
        ReplyKind::Symbols => check_symbols_reply(test, &request, &stdout)?,
        // Answered over JSON-RPC by `run_code_action_test` before any
        // one-shot command runs.
        ReplyKind::CodeAction => {
            return Err(anyhow::anyhow!(
                "{request}: a code action is not a one-shot request"
            ));
        }
        ReplyKind::Rename => check_rename_reply(test, &request, &stdout)?,
    };

    // Debug output if verbose and test failed
    if std::env::var("ROCK_TEST_VERBOSE").is_ok() && !all_found {
        eprintln!("    Actual output: {}", stdout);
    }

    Ok(all_found)
}

/// Write the fixture's files into a fresh project directory under
/// `.lsp-test-tmp/` and return the primary file and that directory.
fn write_fixture_project(test: &LspTest) -> Result<(PathBuf, PathBuf)> {
    // Create a test file in a local test directory
    let test_dir = std::path::PathBuf::from(".lsp-test-tmp");
    fs_err::create_dir_all(&test_dir)?;

    // Sanitise the test name for filesystem use.
    let safe_name: String = test
        .name
        .chars()
        .map(|c| if c == ' ' || c == '/' { '_' } else { c })
        .collect();
    // Fixture names repeat across files and fixtures run in parallel, so the
    // directory carries the process and a counter; the file keeps the name.
    static NEXT_PROJECT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT_PROJECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let proj = test_dir.join(format!("{safe_name}-{}-{n}", std::process::id()));
    fs_err::create_dir_all(&proj)?;

    // Multi-file tests get their own subdirectory containing `stick.toml` plus
    // every `FILE:` entry. The primary file (the one with the cursor marker)
    // is the path passed to the rock-lsp CLI. `project.toml` in the spec is
    // translated to `stick.toml` since that's Rock's real project manifest.
    if !test.files.is_empty() {
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
            test.files
                .first()
                .map(|(n, _)| n.clone())
                .unwrap_or_default()
        } else {
            test.primary_file.clone()
        };
        Ok((proj.join(&primary), proj))
    } else {
        // Each single-file test gets its OWN subdirectory (with a stick.toml),
        // not a shared file in `.lsp-test-tmp/`. Operations that scan the whole
        // project (`references`, `workspace-symbols` -> `load_project`) would
        // otherwise pick up sibling fixtures' symbols and race against their
        // parallel create/delete. Isolating per test makes the project bound
        // to exactly this file.
        fs_err::write(
            proj.join("stick.toml"),
            "[package]\nname = \"lsp_test\"\nversion = \"0.1.0\"\n",
        )?;
        let tf = proj.join(format!("{}.rock", safe_name));
        fs_err::write(&tf, &test.code)?;
        Ok((tf, proj))
    }
}

/// The `rock lsp` subcommand and arguments that answer `op` on `test_file`,
/// or `None` when no one-shot subcommand answers it. The CLI takes 1-indexed
/// lines and columns.
fn one_shot_args(op: &LspOperation, test_file: &Path) -> Option<Vec<String>> {
    Some(match op {
        LspOperation::Completion { line, character } => vec![
            "completion".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
        ],
        LspOperation::GotoDefinition { line, character } => vec![
            "definition".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
        ],
        LspOperation::GotoTypeDefinition { line, character } => vec![
            "type-definition".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
        ],
        LspOperation::Hover { line, character } => vec![
            "hover".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
        ],
        LspOperation::References { line, character } => vec![
            "references".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
        ],
        LspOperation::InlayHints => {
            vec!["inlay-hints".to_string(), test_file.display().to_string()]
        }
        LspOperation::Rename {
            line,
            character,
            new_name,
        } => vec![
            "rename".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
            new_name.clone(),
        ],
        LspOperation::SemanticTokens => vec![
            "semantic-tokens".to_string(),
            test_file.display().to_string(),
        ],
        LspOperation::SignatureHelp { line, character } => vec![
            "signature-help".to_string(),
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1),
        ],
        LspOperation::DocumentSymbols => vec![
            "document-symbols".to_string(),
            test_file.display().to_string(),
        ],
        LspOperation::WorkspaceSymbols { query } => vec![
            "workspace-symbols".to_string(),
            test_file.display().to_string(),
            query.clone(),
        ],
        // No one-shot subcommand answers a code action; it goes to the
        // interactive server over JSON-RPC, as an editor sends it.
        LspOperation::CodeAction { .. } => return None,
    })
}

/// The `check_*_reply` functions compare one one-shot reply with the
/// fixture's `EXPECTED:`. `Ok(false)` is a mismatch; `Err` is a reply that
/// does not decode or has the wrong shape.
fn check_completion_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // `completions` must decode as an array before any label is compared.
    let json = decode_reply(request, stdout)?;
    let completions = reply_array(request, stdout, &json, "completions")?;
    let labels: Vec<String> = completions
        .iter()
        .filter_map(|c| c.get("label").and_then(|l| l.as_str()))
        .map(|s| s.to_string())
        .collect();

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
    Ok(all_found)
}

fn check_definition_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // For goto_definition / goto_type_definition, check the output format
    // (both emit the same `definitions` array shape).
    let json = decode_reply(request, stdout)?;
    let definitions = reply_array(request, stdout, &json, "definitions")?;
    // The actual LSP output has definitions[0].range.start.{line,character}
    let definition = definitions.first();

    // Support `EXPECTED: none` — assert that goto-def found no targets.
    // Useful for built-in types, keywords, and positions that have no
    // definition to navigate to (the LSP should return [] not an error).
    let expects_none = test
        .expected
        .iter()
        .any(|e| e.trim().eq_ignore_ascii_case("none"));
    if expects_none {
        if !definitions.is_empty() {
            all_found = false;
            if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                eprintln!("    Expected no definitions, got {:?}", definitions);
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
                let expected_line: usize = expected_item
                    .strip_prefix("line:")
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap_or(0);
                if let Some(line) = def
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64())
                {
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
                let expected_char: usize = expected_item
                    .strip_prefix("character:")
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap_or(0);
                if let Some(character) = def
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("character"))
                    .and_then(|c| c.as_u64())
                {
                    if character != expected_char as u64 {
                        all_found = false;
                        if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                            eprintln!(
                                "    Expected character {}, got {}",
                                expected_char, character
                            );
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
    Ok(all_found)
}

fn check_references_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
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
    //
    // The `references` array must decode before the set is built: an
    // undecodable or shapeless reply is a server bug, never an empty
    // result, so it cannot satisfy `EXPECTED: none`.
    let json = decode_reply(request, stdout)?;
    let actual_refs: Vec<(u64, u64)> = reply_array(request, stdout, &json, "references")?
        .iter()
        .filter_map(|loc| {
            let start = loc.get("range")?.get("start")?;
            let line = start.get("line")?.as_u64()?;
            let character = start.get("character")?.as_u64()?;
            Some((line, character))
        })
        .collect();

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
                let actual_set: std::collections::BTreeSet<_> = actual_refs.iter().collect();
                let expected_set: std::collections::BTreeSet<_> = expected_refs.iter().collect();
                let missing: Vec<_> = expected_set.difference(&actual_set).collect();
                let extra: Vec<_> = actual_set.difference(&expected_set).collect();
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
    Ok(all_found)
}

fn check_semantic_tokens_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // For semantic_tokens, check for minimum number of tokens
    let json = decode_reply(request, stdout)?;
    // The rock-lsp outputs in a custom format with count and tokens fields
    let token_count = if let Some(count) = json.get("count").and_then(|c| c.as_u64()) {
        count as usize
    } else if let Some(tokens) = json.get("tokens").and_then(|t| t.as_array()) {
        tokens.len()
    } else if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        data.len() / 5 // Each token is 5 integers in LSP format
    } else {
        return Err(anyhow::anyhow!(
            "{request}: reply has no `count`, `tokens` or `data` field; raw reply: {stdout}"
        ));
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
                        eprintln!(
                            "    Expected at least {} tokens, got {}",
                            min_tokens, token_count
                        );
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
    Ok(all_found)
}

fn check_signature_help_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // For signature_help, check the signature in the output.
    // The LSP may return multiple overloads under `signatures`; an
    // expected substring passes if it's found in *any* of them. This
    // lets overload-heavy tests assert on the right arm even when
    // overload resolution picks a different one as active.
    let json = decode_reply(request, stdout)?;
    let labels: Vec<String> = if let Some(sig) = json.get("signature").and_then(|s| s.as_str()) {
        vec![sig.to_string()]
    } else if let Some(sigs) = json.get("signatures").and_then(|s| s.as_array()) {
        sigs.iter()
            .filter_map(|s| {
                s.get("label")
                    .and_then(|l| l.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    } else {
        return Err(anyhow::anyhow!(
            "{request}: reply has no `signature` or `signatures` field; raw reply: {stdout}"
        ));
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
                eprintln!(
                    "    Expected '{}' not found (searched {:?})",
                    needle, search_target
                );
                eprintln!("    All signatures: {:?}", labels);
            }
        }
    }
    Ok(all_found)
}

fn check_inlay_hints_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // Parse `«hint::LABEL»` markers out of the expected source and
    // assert each (line, char, label) triple appears in the LSP's
    // `hints` array. Subset semantics — extra hints are allowed; every
    // expected hint must be found.
    let expected_hints = parse_hint_markers(&test.expected);
    let json = decode_reply(request, stdout)?;
    let actual: Vec<(u64, u64, String)> = reply_array(request, stdout, &json, "hints")?
        .iter()
        .filter_map(|h| {
            let line = h
                .get("position")
                .and_then(|p| p.get("line"))
                .and_then(|v| v.as_u64())?;
            let ch = h
                .get("position")
                .and_then(|p| p.get("character"))
                .and_then(|v| v.as_u64())?;
            let label = h.get("label").and_then(|v| v.as_str())?.to_string();
            Some((line, ch, label))
        })
        .collect();

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
        let found = actual
            .iter()
            .any(|(al, _ac, alabel)| al == line && normalise_hint_label(alabel) == norm_exp);
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
    Ok(all_found)
}

fn check_hover_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // For hover, check the hover contents
    let json = decode_reply(request, stdout)?;
    let hover_contents = json
        .get("hover")
        .and_then(|h| h.get("contents"))
        .and_then(|c| {
            // Handle both string and MarkupContent formats
            if let Some(s) = c.as_str() {
                Some(s.to_string())
            } else if let Some(markup) = c.as_object() {
                markup
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!("{request}: reply has no `hover.contents`; raw reply: {stdout}")
        })?;

    // Join all expected lines into a single expected string
    let expected_full = test.expected.join("\n");

    // Normalize whitespace for comparison
    let contents_normalized = hover_contents.trim();
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
    Ok(all_found)
}

fn check_symbols_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // For symbol tests, check the symbol names and kinds
    let json = decode_reply(request, stdout)?;
    let symbols = if let Some(syms) = json.get("symbols").and_then(|s| s.as_array()) {
        syms.clone()
    } else if let Some(syms) = json.as_array() {
        syms.clone()
    } else {
        return Err(anyhow::anyhow!(
            "{request}: reply has no `symbols` array; raw reply: {stdout}"
        ));
    };

    for expected_item in &test.expected {
        // Expected format: "SymbolName: SymbolKind"
        let parts: Vec<&str> = expected_item.split(':').collect();
        if parts.len() == 2 {
            let expected_name = parts[0].trim();
            let expected_kind = parts[1].trim();

            let found = symbols.iter().any(|sym| {
                let name_matches = sym
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n == expected_name)
                    .unwrap_or(false);

                let kind_matches = sym
                    .get("kind")
                    .and_then(symbol_kind_name)
                    .map(|k| k.to_lowercase().contains(&expected_kind.to_lowercase()))
                    .unwrap_or(false);

                name_matches && kind_matches
            });

            if !found {
                all_found = false;
                if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
                    eprintln!(
                        "    Expected symbol '{}' of kind '{}' not found",
                        expected_name, expected_kind
                    );
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
    Ok(all_found)
}

fn check_rename_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let mut all_found = true;
    // For rename tests, apply the text edits and check the resulting code
    let json = decode_reply(request, stdout)?;
    check_rename_refusal(test, request, &json)?;
    if test.rename_refused.is_some() {
        // Refused as expected: nothing to apply.
    } else if let Some(changes) = json.get("changes").and_then(|c| c.as_object()) {
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
            let result_code = apply_rename_changes(&test.code, changes);

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
        return Err(anyhow::anyhow!(
            "{request}: reply has no `changes` object; raw reply: {stdout}"
        ));
    }
    Ok(all_found)
}

/// Apply a single-file rename's `changes` to `code`, each file's edits from
/// the last position to the first.
fn apply_rename_changes(
    code: &str,
    changes: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut result_code = code.to_string();
    for (_file_uri, edits) in changes {
        if let Some(edits_array) = edits.as_array() {
            // Sort edits by position in reverse order (so we apply from end to start)
            let mut edits_sorted: Vec<_> = edits_array.iter().collect();
            edits_sorted.sort_by(|a, b| {
                let start_a = a
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .map(|s| {
                        let line = s.get("line").and_then(|l| l.as_u64()).unwrap_or(0);
                        let char = s.get("character").and_then(|c| c.as_u64()).unwrap_or(0);
                        (line, char)
                    })
                    .unwrap_or((0, 0));
                let start_b = b
                    .get("range")
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
                    edit.get("range"),
                ) {
                    let start_line = range
                        .get("start")
                        .and_then(|s| s.get("line"))
                        .and_then(|l| l.as_u64())
                        .unwrap_or(0) as usize;
                    let start_char = range
                        .get("start")
                        .and_then(|s| s.get("character"))
                        .and_then(|c| c.as_u64())
                        .unwrap_or(0) as usize;
                    let end_line = range
                        .get("end")
                        .and_then(|e| e.get("line"))
                        .and_then(|l| l.as_u64())
                        .unwrap_or(0) as usize;
                    let end_char = range
                        .get("end")
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
    result_code
}

// ── Code Action Test Runner (JSON-RPC over stdio) ───────────────────────────

/// Open the fixture in a fresh `rock lsp serve`, request the code actions at
/// (`line`, `character`), apply the edit of the one the fixture names, and
/// compare the file with the fixture's `RESULT:`.
fn run_code_action_test(
    test: &LspTest,
    server: LspServerCmd,
    test_file: &Path,
    line: usize,
    character: usize,
) -> Result<bool> {
    let test_file = std::fs::canonicalize(test_file)?;
    let root = test_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", test_file.display()))?;
    let uri = file_uri(&test_file)?;
    let mut server = start_lsp_server(server, root, &test.name)?;
    let reply = request_code_actions(&mut server, &uri, &test.code, line, character);
    // Shut down before looking at the reply, so a failed request still stops
    // the child.
    server.shutdown(3)?;
    check_code_action_reply(test, &uri, &reply?)
}

/// Open `code` as `uri` and send `textDocument/codeAction` (request id 2) for
/// an empty range at the position, returning the whole JSON-RPC response.
fn request_code_actions(
    server: &mut LspServer,
    uri: &str,
    code: &str,
    line: usize,
    character: usize,
) -> Result<serde_json::Value> {
    let did_open = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": { "uri": uri, "languageId": "rock", "version": 1, "text": code }
        }
    });
    server.stdin.write_all(&encode_jsonrpc(&did_open))?;
    let position = serde_json::json!({ "line": line, "character": character });
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/codeAction",
        "params": {
            "textDocument": { "uri": uri },
            "range": { "start": position, "end": position },
            "context": { "diagnostics": [] }
        }
    });
    server.stdin.write_all(&encode_jsonrpc(&request))?;
    server.stdin.flush()?;
    read_response_from_chan(&server.rx, 2)
}

/// Check a `textDocument/codeAction` response against the fixture: the action
/// titled `ACTION_TITLE:` must be offered, and applying its edit to the
/// fixture's code must give exactly the `RESULT:` text. A `NO_ACTION:` title
/// must not be offered at all.
fn check_code_action_reply(test: &LspTest, uri: &str, reply: &serde_json::Value) -> Result<bool> {
    let request = format!("code_action test '{}'", test.name);
    let title = test
        .action_title
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{request}: fixture has no ACTION_TITLE"))?;
    if let Some(error) = reply.get("error") {
        return Err(anyhow::anyhow!(
            "{request}: server answered with an error: {error}"
        ));
    }
    // `null` is the protocol's "no actions here".
    let actions = match reply.get("result") {
        Some(serde_json::Value::Array(actions)) => actions.as_slice(),
        Some(serde_json::Value::Null) => &[],
        _ => {
            return Err(anyhow::anyhow!(
                "{request}: reply has no `result` array; raw reply: {reply}"
            ));
        }
    };
    let found = actions.iter().find(|a| action_title(a) == Some(title));
    if test.action_absent {
        if found.is_some() {
            return Err(anyhow::anyhow!(
                "{request}: the server offered {title:?}, which must not be offered"
            ));
        }
        return Ok(true);
    }
    let expected = test
        .expected_code
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{request}: fixture has no RESULT"))?;
    let Some(action) = found else {
        let offered: Vec<&str> = actions.iter().filter_map(action_title).collect();
        return Err(anyhow::anyhow!(
            "{request}: no action titled {title:?}; the server offered {offered:?}"
        ));
    };
    let edits = code_action_edits(action, uri)
        .map_err(|e| anyhow::anyhow!("{request}: {e}; action: {action}"))?;
    let actual = apply_lsp_text_edits(&test.code, &edits)
        .map_err(|e| anyhow::anyhow!("{request}: {e}; action: {action}"))?;
    if actual.trim_end() != expected.trim_end() {
        return Err(anyhow::anyhow!(
            "{request}: result after {title:?} differs\n--- expected\n{expected}\n--- actual\n{actual}"
        ));
    }
    Ok(true)
}

fn action_title(action: &serde_json::Value) -> Option<&str> {
    action.get("title").and_then(|t| t.as_str())
}

/// The text edits a code action makes to `uri`, from its `edit`'s `changes`
/// map or `documentChanges` list. A single-file fixture has no other file to
/// edit, so an edit elsewhere, or an action with no edit, is an error.
fn code_action_edits<'a>(
    action: &'a serde_json::Value,
    uri: &str,
) -> Result<Vec<&'a serde_json::Value>> {
    let edit = action
        .get("edit")
        .ok_or_else(|| anyhow::anyhow!("action carries no workspace edit"))?;
    let mut edits = Vec::new();
    let check_uri = |target: &str| {
        if target == uri {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "action edits {target}, not the fixture file {uri}"
            ))
        }
    };
    if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (target, file_edits) in changes {
            check_uri(target)?;
            let file_edits = file_edits
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("edits for {target} are not an array"))?;
            edits.extend(file_edits);
        }
    }
    if let Some(changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for change in changes {
            let target = change
                .pointer("/textDocument/uri")
                .and_then(|u| u.as_str())
                .ok_or_else(|| anyhow::anyhow!("document change has no text document"))?;
            check_uri(target)?;
            let file_edits = change
                .get("edits")
                .and_then(|e| e.as_array())
                .ok_or_else(|| anyhow::anyhow!("document change for {target} has no edits"))?;
            edits.extend(file_edits);
        }
    }
    if edits.is_empty() {
        return Err(anyhow::anyhow!("action's workspace edit changes nothing"));
    }
    Ok(edits)
}

/// The byte offset of an LSP position: `character` counts UTF-16 code units
/// and, per the protocol, a value past the end of the line means its end.
fn lsp_offset(source: &str, line: u64, character: u64) -> Option<usize> {
    let mut start = 0;
    for _ in 0..line {
        start += source[start..].find('\n')? + 1;
    }
    let text = source[start..].split('\n').next().unwrap_or("");
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= character {
            return Some(start + index);
        }
        units += ch.len_utf16() as u64;
    }
    Some(start + text.len())
}

/// Apply LSP `TextEdit`s (`range` + `newText`) to `source`. The edits'
/// ranges refer to the original text, so they are applied from the end and
/// must not overlap.
fn apply_lsp_text_edits(source: &str, edits: &[&serde_json::Value]) -> Result<String> {
    let offset_at = |edit: &serde_json::Value, which: &str| -> Result<usize> {
        let pos = edit
            .get("range")
            .and_then(|r| r.get(which))
            .ok_or_else(|| anyhow::anyhow!("edit has no range {which}: {edit}"))?;
        let field = |name: &str| pos.get(name).and_then(|v| v.as_u64());
        let (Some(line), Some(character)) = (field("line"), field("character")) else {
            return Err(anyhow::anyhow!(
                "edit has a malformed range {which}: {edit}"
            ));
        };
        lsp_offset(source, line, character).ok_or_else(|| {
            anyhow::anyhow!("edit range {which} is past the end of the file: {edit}")
        })
    };
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let (start, end) = (offset_at(edit, "start")?, offset_at(edit, "end")?);
        let new_text = edit
            .get("newText")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("edit has no newText: {edit}"))?;
        if end < start {
            return Err(anyhow::anyhow!("edit range ends before it starts: {edit}"));
        }
        spans.push((start, end, new_text));
    }
    spans.sort_by_key(|&(start, end, _)| std::cmp::Reverse((start, end)));
    let mut result = source.to_string();
    let mut limit = source.len();
    for (start, end, new_text) in spans {
        if end > limit {
            return Err(anyhow::anyhow!("code action edits overlap"));
        }
        result.replace_range(start..end, new_text);
        limit = start;
    }
    Ok(result)
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
fn read_response_from_chan(
    rx: &std::sync::mpsc::Receiver<serde_json::Value>,
    expected_id: i64,
) -> Result<serde_json::Value> {
    loop {
        let msg = rx
            .recv()
            .map_err(|e| anyhow::anyhow!("LSP reader channel closed: {}", e))?;
        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
            if id == expected_id {
                return Ok(msg);
            }
        }
    }
}

/// Run a single edit sequence test against a persistent LSP server
fn file_uri(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not UTF-8: {}", path.display()))?;
    Ok(lsp_types::Uri::from_file_path(path)?.as_str().to_string())
}

/// The file every server this run spawns appends its stderr to. Unique per
/// run, so concurrent runs (or another user's) never share or clobber it;
/// named once on stderr so a hang or panic leaves a findable breadcrumb.
fn server_stderr_log() -> Result<PathBuf> {
    static LOG: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let mut log = LOG
        .lock()
        .map_err(|_| anyhow::anyhow!("LSP server stderr log lock poisoned"))?;
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
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot create the LSP server stderr log in {}: {e}",
                dir.display()
            )
        })?;
    let (_, path) = file
        .keep()
        .map_err(|e| anyhow::anyhow!("cannot keep the LSP server stderr log: {e}"))?;
    Ok(path)
}

/// A running `rock lsp serve` child that has answered `initialize`, with a
/// reader thread draining its stdout and a watchdog that kills it if the
/// test wedges.
struct LspServer {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<serde_json::Value>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl LspServer {
    /// Send `shutdown` (as request `id`) and `exit`, wait for the child, and
    /// stop the watchdog.
    fn shutdown(mut self, id: i64) -> Result<()> {
        let shutdown_id = id;
        let shutdown_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": shutdown_id,
            "method": "shutdown",
            "params": null
        });
        self.stdin.write_all(&encode_jsonrpc(&shutdown_request))?;
        self.stdin.flush()?;

        // Try to read shutdown response, but don't fail if the server is slow
        let _ = read_response_from_chan(&self.rx, shutdown_id);

        let exit_notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        });
        self.stdin.write_all(&encode_jsonrpc(&exit_notification))?;
        self.stdin.flush()?;

        // Wait for process to exit
        let _ = self.child.wait();

        // Stop the watchdog (we exited cleanly).
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// Spawn `rock lsp serve` rooted at `root_dir` and complete the
/// `initialize` handshake, using request id 1. `name` labels the watchdog.
fn start_lsp_server(server: LspServerCmd, root_dir: &Path, name: &str) -> Result<LspServer> {
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
    let mut child = server
        .command()
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_for()?)
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;

    // CRITICAL: drain stdout in a dedicated thread. The lsp-server crate uses
    // bounded(0) crossbeam channels for stdio, so its writer thread blocks on
    // `stdout.write_all` whenever the OS pipe is full — and the server's
    // compile path itself blocks on `connection.sender.send()` once the
    // writer is stuck. If this test client only drains stdout when it calls
    // `read_response`, a stress test that fires hundreds of `didChange`
    // notifications without an intervening request can fill the 64 KiB pipe
    // buffer (~5 KiB diagnostic notifications × 12+ unread keystrokes is
    // enough), wedge the server mid-`send`, and produce the
    // `edit_sequence_stress` non-determinism. The drainer below stuffs every
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
    // parallel runner would stall the whole suite indefinitely.
    let timeout_secs: u64 = std::env::var("ROCK_LSP_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let child_pid = child.id();
    let test_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_done = test_done.clone();
    let watchdog_name = name.to_string();
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

    // --- Initialize ---
    let init_id = 1;
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
            "rootUri": file_uri(&std::fs::canonicalize(root_dir)?)?,
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

    Ok(LspServer {
        child,
        stdin,
        rx: msg_rx,
        done: test_done,
    })
}

fn run_edit_sequence_test(
    test: &EditSequenceTest,
    server: LspServerCmd,
) -> Result<(usize, usize, Vec<String>)> {
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let mut passed = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    // Create temp file for the test
    let test_dir = PathBuf::from(".lsp-test-tmp");
    std::fs::create_dir_all(&test_dir)?;
    let test_file = test_dir.join(format!(
        "{}.rock",
        test.name
            .chars()
            .map(|c| if c == ' ' || c == '/' { '_' } else { c })
            .collect::<String>()
    ));
    let test_uri =
        file_uri(&std::fs::canonicalize(&test_dir)?.join(test_file.file_name().unwrap()))?;

    // Write initial content so the LSP can find it
    let initial_code = match &test.steps[0].mode {
        StepMode::FullReplace { code } => code.clone(),
        StepMode::TypeAt { .. } | StepMode::DeleteAt { .. } => {
            // First step must be FullReplace to establish the initial document
            return Err(anyhow::anyhow!(
                "First step in edit_sequence must be a full document (STEP N:), not a typing/delete step"
            ));
        }
    };
    std::fs::write(&test_file, &initial_code)?;

    let LspServer {
        child,
        mut stdin,
        rx: msg_rx,
        done: test_done,
    } = start_lsp_server(server, &test_dir, &test.name)?;
    // `start_lsp_server` used id 1 for `initialize`.
    let mut next_id: i64 = 2;
    let mut version: i64 = 1;

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
                        eprintln!(
                            "    Step {}: typing {} chars at {}:{}",
                            step_idx + 1,
                            text.len(),
                            line,
                            col
                        );
                    }

                    for ch in text.chars() {
                        version += 1;
                        if ch == '\n' {
                            // Simulate pressing Enter: split the line at cursor
                            let mut lines: Vec<String> =
                                current_doc.lines().map(|l| l.to_string()).collect();
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
                        eprintln!(
                            "    Step {}: deleting {} chars at {}:{}",
                            step_idx + 1,
                            count,
                            line,
                            col
                        );
                    }

                    for _ in 0..*count {
                        version += 1;
                        let (new_doc, new_line, new_col) =
                            delete_char_at(&current_doc, cursor_line, cursor_col);
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
                        eprintln!(
                            "    Step {}: diagnostics check (pass-through)",
                            step_idx + 1
                        );
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

    LspServer {
        child,
        stdin,
        rx: msg_rx,
        done: test_done,
    }
    .shutdown(next_id)?;

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
        eprintln!(
            "    Step {}: semantic_tokens count = {}",
            step_idx + 1,
            token_count
        );
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
                        step_idx + 1,
                        min_tokens,
                        token_count
                    );
                }
            }
        } else if exp == "non_zero" {
            if token_count == 0 {
                all_ok = false;
                if verbose {
                    eprintln!(
                        "    Step {}: FAIL - expected non-zero tokens, got 0",
                        step_idx + 1
                    );
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
        eprintln!(
            "    Step {}: completion labels = {:?}",
            step_idx + 1,
            labels
        );
    }

    let mut all_ok = true;
    for exp in expected {
        if !labels.iter().any(|l| l.contains(exp)) {
            all_ok = false;
            if verbose {
                eprintln!(
                    "    Step {}: FAIL - expected completion '{}' not found",
                    step_idx + 1,
                    exp
                );
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
            c.get("value")
                .and_then(|v| v.as_str())
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
                eprintln!(
                    "    Step {}: FAIL - expected '{}' in hover",
                    step_idx + 1,
                    exp
                );
            }
        }
    }
    all_ok
}

/// Run all tests (single-shot + edit_sequence) in a single .rock.lsp file.
/// Returns a `FileTestResult` for the shared parallel runner.
fn run_lsp_file(file: &Path, display_name: &str, server: LspServerCmd) -> FileTestResult {
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
        match run_lsp_test(test, server) {
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
        match run_edit_sequence_test(seq_test, server) {
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
    let bin = lsp_server_bin()?;

    let (total, passed, failed, ignored) =
        parallel_runner::run_files_parallel(lsp_files, None, move |file| {
            let display_name = display_name_for_lsp_file(file, &root_dir);
            let server = LspServerCmd { bin: &bin, backend };
            run_lsp_file(file, &display_name, server)
        });

    // Print section summary (matches formatter/error test pattern)
    if total > 0 {
        use colored::Colorize;
        let result_text = if failed == 0 {
            "ok".green()
        } else {
            "FAILED".red()
        };
        println!(
            "\nLSP tests: {}. {} passed; {} failed; {} ignored; finished in {:.2}s",
            result_text,
            passed,
            failed,
            ignored,
            start_time.elapsed().as_secs_f64()
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
mod runner_tests {
    use super::*;

    /// Records each spawn in `invocations` and prints `reply`, both next to
    /// the path it was run through.
    const STUB_SCRIPT: &str =
        "#!/bin/sh\ndir=\"${0%/*}\"\necho \"$@\" >> \"$dir/invocations\"\ncat \"$dir/reply\"\n";

    /// The one stub script every test links to, kept beside the test binary.
    /// macOS scans each new executable on its first run, which costs up to
    /// half a second, so a fresh script per fixture made these tests slow.
    fn shared_stub_script() -> &'static Path {
        static STUB: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        STUB.get_or_init(|| {
            let exe = std::env::current_exe().unwrap();
            let path = exe.parent().unwrap().join("rock-lsp-test-stub.sh");
            if std::fs::read_to_string(&path).ok().as_deref() != Some(STUB_SCRIPT) {
                #[cfg(unix)]
                test_utils::write_executable(&path, STUB_SCRIPT);
                #[cfg(not(unix))]
                std::fs::write(&path, STUB_SCRIPT).unwrap();
            }
            path
        })
    }

    /// A stub server in `dir` that records each spawn in `<dir>/invocations`
    /// and prints `reply`, so a fixture's verdict comes from the runner's
    /// reply handling rather than from the compiler.
    fn stub_server(dir: &Path, reply: &str) -> PathBuf {
        std::fs::write(dir.join("reply"), reply).unwrap();
        let path = dir.join("stub-rock");
        #[cfg(unix)]
        std::os::unix::fs::symlink(shared_stub_script(), &path).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(shared_stub_script(), &path).unwrap();
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
        let server = LspServerCmd {
            bin: &bin,
            backend: crate::Backend::Bone,
        };
        let result = run_lsp_file(&path, "fixture", server);
        let invocations = std::fs::read_to_string(&marker)
            .map(|log| log.lines().count())
            .unwrap_or(0);
        (result, invocations)
    }

    #[test]
    fn the_lsp_server_is_the_running_executable() {
        assert_eq!(
            lsp_server_bin().unwrap(),
            std::env::current_exe().unwrap(),
            "a fixture must run the executable that is running it"
        );
    }

    /// The stub records every spawn, so the fixture proves which binary ran.
    #[test]
    fn a_fixture_runs_on_the_given_binary() {
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
    // --- Reply strictness ---------------------------------------------------
    //
    // Every reply below either is malformed or lacks the array its request
    // answers with, yet contains the fixture's EXPECTED text verbatim. A
    // fixture over such a reply must fail; a substring fallback would pass it.

    const COMPLETION_FIXTURE: &str = "\
TEST: stub completion\n\
TYPE: completion\n\
---\n\
fn main() {\n\
    let count = 1;\n\
    let other = cou«cursor»nt;\n\
}\n\
---\n\
OPERATION: completion\n\
EXPECTED:\n\
- count\n\
===\n";

    const DEFINITION_FIXTURE: &str = "\
TEST: stub definition\n\
TYPE: goto_definition\n\
---\n\
fn main() {\n\
    let count = 1;\n\
    let other = cou«cursor»nt;\n\
}\n\
---\n\
OPERATION: goto_definition\n\
EXPECTED:\n\
line: 2\n\
character: 12\n\
===\n";

    const DEFINITION_NONE_FIXTURE: &str = "\
TEST: stub definition none\n\
TYPE: goto_definition\n\
---\n\
fn main() {\n\
    let other = cou«cursor»nt;\n\
}\n\
---\n\
OPERATION: goto_definition\n\
EXPECTED:\n\
none\n\
===\n";

    const REFERENCES_NONE_FIXTURE: &str = "\
TEST: stub references none\n\
TYPE: references\n\
---\n\
fn main() {\n\
    let count = 1;\n\
    let other = cou«cursor»nt;\n\
}\n\
---\n\
OPERATION: references\n\
EXPECTED:\n\
none\n\
===\n";

    const SIGNATURE_HELP_FIXTURE: &str = "\
TEST: stub signature help\n\
TYPE: signature_help\n\
---\n\
fn greet(name: string) {}\n\
fn main() {\n\
    gre«cursor»et(\n\
}\n\
---\n\
OPERATION: signature_help\n\
EXPECTED:\n\
greet(name: string)\n\
===\n";

    const SYMBOLS_FIXTURE: &str = "\
TEST: stub symbols\n\
TYPE: document_symbols\n\
---\n\
struct Widget {\n\
}\n\
---\n\
OPERATION: document_symbols\n\
EXPECTED:\n\
- Widget: struct\n\
===\n";

    const HOVER_FIXTURE: &str = "\
TEST: stub hover\n\
TYPE: hover\n\
---\n\
fn main() {\n\
    mai«cursor»n();\n\
}\n\
---\n\
OPERATION: hover\n\
EXPECTED:\n\
fn main()\n\
===\n";

    const INLAY_HINTS_FIXTURE: &str = "\
TEST: stub inlay hints\n\
TYPE: inlay_hints\n\
---\n\
fn main() {\n\
    let count = 1\n\
}\n\
---\n\
OPERATION: inlay_hints\n\
EXPECTED:\n\
fn main() {\n\
    let count«hint:: int» = 1\n\
}\n\
===\n";

    const RENAME_FIXTURE: &str = "\
TEST: stub rename\n\
TYPE: rename\n\
---\n\
fn main() {\n\
    let «start»count«end» = 1\n\
    print(count)\n\
}\n\
---\n\
OPERATION: rename\n\
NEW_NAME: total\n\
---\n\
EXPECTED:\n\
fn main() {\n\
    let total = 1\n\
    print(total)\n\
}\n\
===\n";

    /// `full_document` is the older spelling of a whole-file semantic-token
    /// request. It builds the semantic-token operation, so its reply has to
    /// decode as one whatever the fixture calls the type.
    const FULL_DOCUMENT_FIXTURE: &str = "\
TEST: stub full document\n\
TYPE: full_document\n\
---\n\
fn main() {\n\
    let count = 1;\n\
}\n\
---\n\
OPERATION: full_document\n\
EXPECTED:\n\
min_tokens: 5\n\
===\n";

    /// A fixture the parser must reject, with a type no decoder answers.
    const UNKNOWN_TYPE_FIXTURE: &str = "\
TEST: stub unknown type\n\
TYPE: teleport\n\
---\n\
fn main() {\n\
    let count = 1;\n\
}\n\
---\n\
OPERATION: teleport\n\
EXPECTED:\n\
- count\n\
===\n";

    const MISMATCHED_OPERATION_FIXTURE: &str = "\
TEST: stub mismatched operation\n\
TYPE: hover\n\
---\n\
fn main() {\n\
    mai«cursor»n();\n\
}\n\
---\n\
OPERATION: completion\n\
EXPECTED:\n\
- count\n\
===\n";

    const UNKNOWN_OPERATION_FIXTURE: &str = "\
TEST: stub unknown operation\n\
TYPE: completion\n\
---\n\
fn main() {\n\
    mai«cursor»n();\n\
}\n\
---\n\
OPERATION: teleport\n\
EXPECTED:\n\
- count\n\
===\n";

    /// Write `fixture` to a temporary `.rock.lsp` file and return the parse
    /// error it must produce.
    fn parse_error_of(fixture: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, fixture).unwrap();
        parse_lsp_test_file(&path)
            .expect_err("the fixture must be rejected when it is parsed")
            .to_string()
    }

    /// The fixture must fail, and the failure must name the request and quote
    /// the raw reply.
    fn assert_fixture_fails(fixture: &str, reply: &str, request: &str) {
        let (result, invocations) = run_against_stub(fixture, reply);
        assert_eq!(invocations, 1, "the stub server must have run exactly once");
        assert_eq!(
            (result.passed, result.failed),
            (0, 1),
            "a reply of {reply:?} must not satisfy the fixture; details: {:?}",
            result.failed_test_details
        );
        let detail = &result.failed_test_details[0].1;
        assert!(
            detail.contains(request),
            "failure {detail:?} does not name the request {request:?}"
        );
        assert!(
            detail.contains(reply),
            "failure {detail:?} does not quote the raw reply {reply:?}"
        );
    }

    #[test]
    fn a_malformed_completion_reply_fails_the_fixture() {
        assert_fixture_fails(
            COMPLETION_FIXTURE,
            r#"{"completions": [{"label": "count""#,
            "completion",
        );
    }

    #[test]
    fn a_malformed_definition_reply_fails_the_fixture() {
        assert_fixture_fails(
            DEFINITION_FIXTURE,
            "not json line: 2 character: 12",
            "goto_definition",
        );
    }

    #[test]
    fn a_definition_reply_without_the_array_fails_the_none_expectation() {
        assert_fixture_fails(
            DEFINITION_NONE_FIXTURE,
            r#"{"status": "ok"}"#,
            "goto_definition",
        );
    }

    #[test]
    fn a_malformed_references_reply_fails_the_none_expectation() {
        assert_fixture_fails(
            REFERENCES_NONE_FIXTURE,
            r#"{"references": [{"range": "#,
            "references",
        );
    }

    #[test]
    fn a_references_reply_without_the_array_fails_the_none_expectation() {
        assert_fixture_fails(REFERENCES_NONE_FIXTURE, r#"{"status": "ok"}"#, "references");
    }

    #[test]
    fn a_malformed_signature_help_reply_fails_the_fixture() {
        assert_fixture_fails(
            SIGNATURE_HELP_FIXTURE,
            "not json greet(name: string)",
            "signature_help",
        );
    }

    #[test]
    fn a_malformed_symbols_reply_fails_the_fixture() {
        assert_fixture_fails(
            SYMBOLS_FIXTURE,
            "not json Widget: struct",
            "document_symbols",
        );
    }

    #[test]
    fn a_malformed_full_document_reply_fails_the_fixture() {
        assert_fixture_fails(
            FULL_DOCUMENT_FIXTURE,
            "not json min_tokens: 5",
            "full_document",
        );
    }

    /// Every fixture type the parser accepts must reject a reply that only
    /// carries the expected text: none of them may fall back to a raw-output
    /// comparison, and none of them may reach a missing decoder.
    #[test]
    fn every_fixture_type_rejects_a_reply_that_only_quotes_the_expected_text() {
        let cases: [(&str, &str, &str); 9] = [
            (COMPLETION_FIXTURE, "count", "completion"),
            (DEFINITION_FIXTURE, "character: 12", "goto_definition"),
            (REFERENCES_NONE_FIXTURE, "none", "references"),
            (
                SIGNATURE_HELP_FIXTURE,
                "greet(name: string)",
                "signature_help",
            ),
            (SYMBOLS_FIXTURE, "Widget: struct", "document_symbols"),
            (HOVER_FIXTURE, "fn main()", "hover"),
            (INLAY_HINTS_FIXTURE, ": int", "inlay_hints"),
            (RENAME_FIXTURE, "let total = 1", "rename"),
            (FULL_DOCUMENT_FIXTURE, "min_tokens: 5", "full_document"),
        ];
        for (fixture, expected_text, request) in cases {
            let reply = format!("not json {expected_text}");
            let (result, invocations) = run_against_stub(fixture, &reply);
            assert_eq!(invocations, 1, "the stub server must have run exactly once");
            assert_eq!(
                (result.passed, result.failed),
                (0, 1),
                "a reply of {reply:?} must not satisfy a {request} fixture; details: {:?}",
                result.failed_test_details
            );
            let detail = &result.failed_test_details[0].1;
            assert!(
                detail.contains(request) && detail.contains(&reply),
                "failure {detail:?} must name the request {request:?} and quote {reply:?}"
            );
            assert!(
                !detail.contains("no reply decoder"),
                "{request} must have a decoder, not fall through: {detail:?}"
            );
        }
    }

    /// A type the harness cannot decode fails when the fixture is parsed, so it
    /// cannot be dropped or compared as text.
    #[test]
    fn an_unknown_fixture_type_is_rejected_at_parse_time() {
        let message = parse_error_of(UNKNOWN_TYPE_FIXTURE);
        assert!(
            message.contains("TYPE 'teleport'") && message.contains("stub unknown type"),
            "{message}"
        );
    }

    /// The type names the decoder and the operation names the request; a
    /// fixture whose two halves disagree would run one request through another
    /// type's validator.
    #[test]
    fn a_fixture_whose_type_and_operation_disagree_is_rejected_at_parse_time() {
        let message = parse_error_of(MISMATCHED_OPERATION_FIXTURE);
        assert!(
            message.contains("TYPE 'hover'")
                && message.contains("OPERATION 'completion'")
                && message.contains("stub mismatched operation"),
            "{message}"
        );
    }

    #[test]
    fn an_unknown_operation_is_rejected_at_parse_time() {
        let message = parse_error_of(UNKNOWN_OPERATION_FIXTURE);
        assert!(
            message.contains("OPERATION 'teleport'") && message.contains("stub unknown operation"),
            "{message}"
        );
    }

    /// The rename fixture whose code block opens with `--` instead of `---`.
    /// It used to vanish from the run without a word.
    const BAD_DELIMITER_FIXTURE: &str = "\
TEST: stub bad delimiter\n\
TYPE: rename\n\
--\n\
fn main() {\n\
    let «start»count«end» = 1\n\
}\n\
---\n\
OPERATION: rename\n\
NEW_NAME: total\n\
---\n\
EXPECTED:\n\
fn main() {\n\
    let total = 1\n\
}\n\
===\n";

    #[test]
    fn a_malformed_code_block_delimiter_is_a_parse_error_naming_the_fixture() {
        let message = parse_error_of(BAD_DELIMITER_FIXTURE);
        assert!(
            message.contains("stub bad delimiter") && message.contains("fixture.rock.lsp"),
            "{message}"
        );
    }

    /// Each reachable malformed shape, as (fixture, text the error must carry).
    /// Every one used to drop its fixture, or an expectation in it, silently.
    #[test]
    fn every_malformed_block_is_a_parse_error_naming_the_fixture() {
        let hover_with = |operation_and_expected: &str| {
            format!(
                "TEST: stub malformed\nTYPE: hover\n---\nfn main() {{\n    main()\n}}\n---\n{operation_and_expected}"
            )
        };
        let cases = [
            (
                "TEST: stub malformed\n---\nfn main() {}\n".to_string(),
                "expected `TYPE:`",
            ),
            ("TEST: stub malformed\n".to_string(), "before `TYPE:`"),
            (
                "TEST: stub malformed\nTYPE: hover\n---\nfn main() {}\n".to_string(),
                "closing `---`",
            ),
            (
                hover_with("EXPECTED:\nfn main()\n===\n"),
                "expected `OPERATION:`",
            ),
            (
                hover_with("OPERATION: hover\nEXPECTED:\nfn main()\n===\n"),
                "needs a `POSITION:` line or a cursor marker",
            ),
            (
                hover_with("OPERATION: hover\nPOSITION: line=1\nEXPECTED:\nfn main()\n===\n"),
                "Invalid position format",
            ),
            (
                hover_with("OPERATION: hover\nPOSITION: line=1, character=4\nfn main()\n===\n"),
                "expected `EXPECTED:`",
            ),
            (
                hover_with("OPERATION: hover\nPOSITION: line=1, character=4\nEXPECTED:\n\n===\n"),
                "EXPECTED block is empty",
            ),
            (
                hover_with(
                    "OPERATION: hover\nPOSITION: line=1, character=4\nEXPECTED:\nfn main()\n\nTEST: next\nTYPE: hover\n",
                ),
                "no closing `===`",
            ),
            (
                "TEST: stub malformed\nTYPE: completion\n---\nfn main() {\n    ma«cursor»\n}\n---\nOPERATION: completion\nEXPECTED:\nmain\n===\n"
                    .to_string(),
                "unrecognised `completion` expectation \"main\"",
            ),
            (
                "TEST: stub malformed\nTYPE: hover\nFILES:\nstray text\nFILE: a.rock\n---\nfn main() {}\n---\nOPERATION: hover\n"
                    .to_string(),
                "expected `FILE:` in a files block",
            ),
            (
                "TEST: stub malformed\nTYPE: edit_sequence\n---\nSTEP 1:\nfn main() {}\n---\n\nTEST: next\n"
                    .to_string(),
                "no closing `===`",
            ),
        ];
        for (fixture, needle) in cases {
            let message = parse_error_of(&fixture);
            assert!(
                message.contains("stub malformed") && message.contains(needle),
                "fixture {fixture:?}: error {message:?} must name the test and say {needle:?}"
            );
        }
    }

    /// A mangled `TEST:` header between blocks would take its block with it.
    /// Whatever the header looks like, the `TYPE:` line under it is caught.
    #[test]
    fn a_mangled_test_header_between_blocks_is_a_parse_error() {
        for header in [
            "test: stub mangled",
            "TEST stub mangled",
            "Tset: stub mangled",
        ] {
            let fixture = format!("{HOVER_FIXTURE}\n{header}\nTYPE: hover\n");
            let message = parse_error_of(&fixture);
            assert!(message.contains("expected `TEST:`"), "{header}: {message}");
        }
    }

    /// Fixtures run in parallel and their names repeat across files, so two
    /// runs of the same fixture must not share a project directory.
    #[test]
    fn fixtures_with_the_same_name_get_separate_project_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, HOVER_FIXTURE).unwrap();
        let test = parse_lsp_test_file(&path).unwrap().remove(0);
        let (file_a, proj_a) = write_fixture_project(&test).unwrap();
        let (file_b, proj_b) = write_fixture_project(&test).unwrap();
        let _ = std::fs::remove_dir_all(&proj_a);
        let _ = std::fs::remove_dir_all(&proj_b);
        assert_ne!(proj_a, proj_b);
        assert_eq!(file_a.file_name(), file_b.file_name());
    }

    /// Prose notes before and between blocks are documentation, not fixtures.
    #[test]
    fn prose_between_blocks_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        let fixture = format!(
            "Notes on this file.\n\n{HOVER_FIXTURE}\nPINNED BUG (sentinel): a note.\n\n{COMPLETION_FIXTURE}"
        );
        std::fs::write(&path, fixture).unwrap();
        let tests = parse_lsp_test_file(&path).unwrap();
        assert_eq!(tests.len(), 2);
    }

    /// The edit-sequence parser rejects malformed steps and checks instead of
    /// dropping them from the sequence.
    #[test]
    fn a_malformed_edit_sequence_step_or_check_is_a_parse_error() {
        let sequence =
            |body: &str| format!("TEST: stub sequence\nTYPE: edit_sequence\n---\n{body}===\n");
        let cases = [
            (
                sequence("STEP 1:\nfn main() {}\n---\nCHECK: hover(1)\n"),
                "malformed check `CHECK: hover(1)`",
            ),
            (
                sequence("STEP 1:\nfn main() {}\n---\nCHECK: teleport\n"),
                "malformed check `CHECK: teleport`",
            ),
            (
                sequence(
                    "STEP 1:\nfn main() {}\n---\nCHECK: semantic_tokens\n---\nSTEP 2 (type at 1):\nx\n---\n",
                ),
                "malformed step header",
            ),
            (
                sequence("STEP 1:\nfn main() {}\n---\nstray text\n"),
                "expected `CHECK:`",
            ),
            (sequence("stray text\n"), "expected `STEP`"),
            (sequence(""), "has no steps"),
        ];
        for (fixture, needle) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fixture.rock.lsp");
            std::fs::write(&path, &fixture).unwrap();
            let message = parse_edit_sequence_tests(&path)
                .expect_err("the sequence must be rejected when it is parsed")
                .to_string();
            assert!(
                message.contains("stub sequence") && message.contains(needle),
                "fixture {fixture:?}: error {message:?} must name the test and say {needle:?}"
            );
        }
    }

    /// `Name: Kind` lines without a `- ` used to be dropped, leaving the
    /// symbol fixtures with nothing to assert. The server sends numeric kinds.
    #[test]
    fn bare_symbol_lines_are_checked_against_numeric_kinds() {
        let fixture = "\
TEST: stub symbols\n\
TYPE: document_symbols\n\
---\n\
struct Widget {\n\
}\n\
---\n\
OPERATION: document_symbols\n\
EXPECTED:\n\
Widget: Struct\n\
===\n";
        let (result, _) =
            run_against_stub(fixture, r#"{"symbols": [{"name": "Widget", "kind": 23}]}"#);
        assert_eq!(
            (result.passed, result.failed),
            (1, 0),
            "{:?}",
            result.failed_test_details
        );
        let (result, _) =
            run_against_stub(fixture, r#"{"symbols": [{"name": "Widget", "kind": 12}]}"#);
        assert_eq!(
            (result.passed, result.failed),
            (0, 1),
            "a Function is not a Struct"
        );
        let (result, _) = run_against_stub(fixture, r#"{"symbols": []}"#);
        assert_eq!(
            (result.passed, result.failed),
            (0, 1),
            "an empty reply lacks Widget"
        );
    }

    /// Stubs that answer with the documented shape still pass, so the strict
    /// path does not reject well-formed, empty or partial replies.
    #[test]
    fn well_formed_replies_pass_the_fixtures() {
        let cases = [
            (
                COMPLETION_FIXTURE,
                r#"{"completions": [{"label": "count"}]}"#,
            ),
            (
                DEFINITION_FIXTURE,
                r#"{"definitions": [{"range": {"start": {"line": 2, "character": 12}}}]}"#,
            ),
            (DEFINITION_NONE_FIXTURE, r#"{"definitions": []}"#),
            (REFERENCES_NONE_FIXTURE, r#"{"references": []}"#),
            (
                SIGNATURE_HELP_FIXTURE,
                r#"{"signatures": [{"label": "greet(name: string)"}], "activeSignature": 0}"#,
            ),
            (
                SYMBOLS_FIXTURE,
                r#"{"symbols": [{"name": "Widget", "kind": "Struct"}]}"#,
            ),
            (HOVER_FIXTURE, r#"{"hover": {"contents": "fn main()"}}"#),
            (
                INLAY_HINTS_FIXTURE,
                r#"{"hints": [{"position": {"line": 1, "character": 12}, "label": ": int"}]}"#,
            ),
            (
                RENAME_FIXTURE,
                r#"{"changes": {"file:///fixture.rock": [{"range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 9}}, "new_text": "total"}, {"range": {"start": {"line": 2, "character": 6}, "end": {"line": 2, "character": 11}}, "new_text": "total"}]}}"#,
            ),
            (FULL_DOCUMENT_FIXTURE, r#"{"count": 7}"#),
        ];
        for (fixture, reply) in cases {
            let (result, invocations) = run_against_stub(fixture, reply);
            assert_eq!(invocations, 1);
            assert_eq!(
                (result.passed, result.failed),
                (1, 0),
                "a reply of {reply:?} must satisfy the fixture; details: {:?}",
                result.failed_test_details
            );
        }
    }

    // --- Code actions -----------------------------------------------------

    // No `\` line continuations: they would strip the indentation the
    // positions below depend on.
    const CODE_ACTION_FIXTURE: &str = "TEST: stub inline
TYPE: code_action
---
fn main() {
    let «start»x«end» = 1 + 2
    print(x * x)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `x`
RESULT:
fn main() {
    print((1 + 2) * (1 + 2))
}
===
";

    fn code_action_test() -> LspTest {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, CODE_ACTION_FIXTURE).unwrap();
        let mut tests = parse_lsp_test_file(&path).unwrap();
        assert_eq!(tests.len(), 1, "a code_action fixture must be admitted");
        tests.pop().unwrap()
    }

    const URI: &str = "file:///fixture.rock";

    /// The inline-local reply the server gives for `CODE_ACTION_FIXTURE`, with
    /// `uses` as the replacement text of each use.
    fn inline_reply(uses: &str) -> serde_json::Value {
        let edit = |l1: u32, c1: u32, l2: u32, c2: u32, text: &str| {
            serde_json::json!({
                "range": {
                    "start": {"line": l1, "character": c1},
                    "end": {"line": l2, "character": c2}
                },
                "newText": text
            })
        };
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": [
                {"title": "Fill struct fields", "edit": {"changes": {URI: []}}},
                {
                    "title": "Inline local `x`",
                    "edit": {"changes": {URI: [
                        edit(2, 10, 2, 11, uses),
                        edit(2, 14, 2, 15, uses),
                        edit(1, 0, 2, 0, ""),
                    ]}}
                }
            ]
        })
    }

    #[test]
    fn a_code_action_fixture_parses_its_title_and_result() {
        let test = code_action_test();
        assert!(matches!(
            test.operation,
            LspOperation::CodeAction {
                line: 1,
                character: 8
            }
        ));
        assert_eq!(test.action_title.as_deref(), Some("Inline local `x`"));
        assert_eq!(
            test.expected_code.as_deref(),
            Some("fn main() {\n    print((1 + 2) * (1 + 2))\n}")
        );
    }

    #[test]
    fn the_named_action_is_applied_and_its_result_compared() {
        let test = code_action_test();
        assert!(check_code_action_reply(&test, URI, &inline_reply("(1 + 2)")).unwrap());
        let wrong = check_code_action_reply(&test, URI, &inline_reply("1 + 2"))
            .unwrap_err()
            .to_string();
        assert!(
            wrong.contains("differs") && wrong.contains("print(1 + 2 * 1 + 2)"),
            "{wrong}"
        );
    }

    #[test]
    fn a_code_action_reply_without_the_action_fails_naming_what_was_offered() {
        let test = code_action_test();
        let none =
            check_code_action_reply(&test, URI, &serde_json::json!({"id": 2, "result": null}))
                .unwrap_err()
                .to_string();
        assert!(
            none.contains("no action titled") && none.contains("[]"),
            "{none}"
        );
        let mut reply = inline_reply("(1 + 2)");
        reply["result"][1]["title"] = "Inline local `y`".into();
        let other = check_code_action_reply(&test, URI, &reply)
            .unwrap_err()
            .to_string();
        assert!(other.contains("Inline local `y`"), "{other}");
    }

    const NO_ACTION_FIXTURE: &str = "TEST: stub no inline
TYPE: code_action
---
fn main() {
    let «start»x«end» = 1 + 2
    print(x * x)
}
---
OPERATION: code_action
---
EXPECTED:
NO_ACTION: Inline local `x`
===
";

    #[test]
    fn a_no_action_fixture_passes_only_when_the_action_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, NO_ACTION_FIXTURE).unwrap();
        let test = parse_lsp_test_file(&path).unwrap().pop().unwrap();
        assert!(test.action_absent);
        assert_eq!(test.action_title.as_deref(), Some("Inline local `x`"));
        assert!(
            check_code_action_reply(&test, URI, &serde_json::json!({"id": 2, "result": null}))
                .unwrap()
        );
        let offered = check_code_action_reply(&test, URI, &inline_reply("(1 + 2)"))
            .unwrap_err()
            .to_string();
        assert!(offered.contains("must not be offered"), "{offered}");

        let with_result = NO_ACTION_FIXTURE.replace("===", "RESULT:\nfn main() {}\n===");
        std::fs::write(&path, with_result).unwrap();
        let err = parse_lsp_test_file(&path).unwrap_err().to_string();
        assert!(err.contains("drop `RESULT:`"), "{err}");
    }

    #[test]
    fn a_refused_rename_fixture_needs_the_error_and_no_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        let (head, _) = RENAME_FIXTURE.split_once("EXPECTED:\n").unwrap();
        let refused = format!("{head}EXPECTED:\nREFUSED: opaque body\n===\n");
        std::fs::write(&path, &refused).unwrap();
        let test = parse_lsp_test_file(&path).unwrap().pop().unwrap();
        assert_eq!(test.rename_refused.as_deref(), Some("opaque body"));
        let edit = serde_json::json!([{"range": {}, "new_text": "total"}]);
        let check = |reply: serde_json::Value| check_rename_refusal(&test, "rename", &reply);
        check(serde_json::json!({"changes": {}, "error": "an opaque body names it"})).unwrap();
        let cases = [
            (serde_json::json!({"changes": {}}), "was not refused"),
            (
                serde_json::json!({"changes": {}, "error": "something else"}),
                "does not contain",
            ),
            (
                serde_json::json!({"changes": {"f": edit}, "error": "opaque body"}),
                "still returned edits",
            ),
        ];
        for (reply, want) in cases {
            let err = check(reply).unwrap_err().to_string();
            assert!(err.contains(want), "{err}");
        }

        std::fs::write(&path, RENAME_FIXTURE).unwrap();
        let plain = parse_lsp_test_file(&path).unwrap().pop().unwrap();
        let err = check_rename_refusal(&plain, "rename", &serde_json::json!({"error": "no"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("rename refused: no"), "{err}");

        let trailing = refused.replace("REFUSED: opaque body\n", "REFUSED: opaque body\nfn x\n");
        std::fs::write(&path, trailing).unwrap();
        let err = parse_lsp_test_file(&path).unwrap_err().to_string();
        assert!(err.contains("whole EXPECTED block"), "{err}");
    }

    #[test]
    fn a_malformed_code_action_reply_fails() {
        let test = code_action_test();
        let cases = [
            (
                serde_json::json!({"id": 2, "error": {"code": -32603}}),
                "error",
            ),
            (
                serde_json::json!({"id": 2, "result": {}}),
                "no `result` array",
            ),
            (
                serde_json::json!({"id": 2, "result": [{"title": "Inline local `x`"}]}),
                "no workspace edit",
            ),
            (
                serde_json::json!({"id": 2, "result": [{
                    "title": "Inline local `x`",
                    "edit": {"changes": {"file:///other.rock": []}}
                }]}),
                "not the fixture file",
            ),
        ];
        for (reply, needle) in cases {
            let message = check_code_action_reply(&test, URI, &reply)
                .unwrap_err()
                .to_string();
            assert!(message.contains(needle), "{reply}: {message}");
        }
    }

    #[test]
    fn lsp_text_edits_use_utf16_columns_and_reject_overlaps() {
        let edit = |c1: u32, c2: u32, text: &str| {
            serde_json::json!({
                "range": {"start": {"line": 0, "character": c1}, "end": {"line": 0, "character": c2}},
                "newText": text
            })
        };
        // `é` is one UTF-16 unit but two bytes.
        let (a, b) = (edit(1, 2, "X"), edit(9, 99, "!"));
        assert_eq!(apply_lsp_text_edits("éab", &[&a, &b]).unwrap(), "éXb!");
        let (a, b) = (edit(0, 2, "X"), edit(1, 3, "Y"));
        assert!(apply_lsp_text_edits("abc", &[&a, &b]).is_err());
    }

    #[test]
    fn a_code_action_fixture_without_a_title_or_result_is_a_parse_error() {
        let without_title = CODE_ACTION_FIXTURE.replace("ACTION_TITLE: Inline local `x`\n", "");
        let message = parse_error_of(&without_title);
        assert!(message.contains("no `ACTION_TITLE:`"), "{message}");
        let without_result = CODE_ACTION_FIXTURE.replace("RESULT:\n", "");
        let message = parse_error_of(&without_result);
        assert!(
            message.contains("expected `ACTION_TITLE:`, `NO_ACTION:` or `RESULT:`"),
            "{message}"
        );
        let title_only = CODE_ACTION_FIXTURE
            .split("RESULT:")
            .next()
            .unwrap()
            .to_string()
            + "===\n";
        let message = parse_error_of(&title_only);
        assert!(message.contains("no `RESULT:`"), "{message}");
    }
}
