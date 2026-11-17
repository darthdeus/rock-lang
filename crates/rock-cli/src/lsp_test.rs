use anyhow::Result;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
struct LspTest {
    name: String,
    test_type: String,
    reply_kind: ReplyKind,
    code: String,
    /// A multi-file fixture selects its primary file using the cursor marker.
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
    OneShot {
        subcommand: &'static str,
        position: Option<(usize, usize)>,
        argument: Option<String>,
    },
    CodeAction {
        line: usize,
        character: usize,
    },
}

#[derive(Debug, Clone)]
struct EditSequenceTest {
    name: String,
    steps: Vec<EditStep>,
}

#[derive(Debug, Clone)]
enum StepMode {
    FullReplace {
        code: String,
    },
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

#[derive(Debug, Clone)]
struct EditStep {
    mode: StepMode,
    checks: Vec<EditStepCheck>,
}

#[derive(Debug, Clone)]
struct EditStepCheck {
    operation: EditStepOperation,
    expected: Vec<String>,
}

#[derive(Debug, Clone)]
enum EditStepOperation {
    SemanticTokens,
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

fn fixture_error(path: &Path, name: &str, msg: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{}: test '{}': {}", path.display(), name, msg)
}

/// Consume a block's remaining lines through its `===` terminator (or the end
/// of the file). A `TEST:` line before `===` means the terminator is missing,
/// which would merge two fixtures.
fn skip_to_block_end<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    path: &Path,
    name: &str,
) -> Result<()> {
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
fn parse_files_block<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    stop_prefixes: &[&str],
    path: &Path,
    name: &str,
) -> Result<(
    Vec<(String, String)>,
    Option<(String, usize, usize)>, // (file_name, line, char)
)> {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut cursor: Option<(String, usize, usize)> = None;

    // Optional leading `---` after `FILES:` / `EXPECTED:`
    lines.next_if(|l| l.starts_with("---"));

    while let Some(line) = lines.peek() {
        if stop_prefixes.iter().any(|p| line.starts_with(p)) {
            break;
        }
        if let Some(rest) = line.strip_prefix("FILE:") {
            let file_name = rest.trim().to_string();
            lines.next();
            // consume leading `---`
            lines.next_if(|l| l.starts_with("---"));
            let (content, position) = source_lines(lines, |line| {
                line.starts_with("---") || stop_prefixes.iter().any(|p| line.starts_with(p))
            });
            lines.next_if(|line| line.starts_with("---"));
            if let Some((line, character)) = position {
                if cursor.is_none() {
                    cursor = Some((file_name.clone(), line, character));
                }
            }
            files.push((file_name, content));
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

fn source_lines<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    stop: impl Fn(&str) -> bool,
) -> (String, Option<(usize, usize)>) {
    let mut content = Vec::new();
    let mut cursor = None;
    while let Some(line) = lines.next_if(|line| !stop(line)) {
        let (clean, column) = strip_markers(line);
        if let Some(column) = column {
            cursor.get_or_insert((content.len(), column));
        }
        content.push(clean);
    }
    (content.join("\n"), cursor)
}

fn fixture_directive<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    path: &Path,
    name: &str,
    directive: &str,
    after: &str,
) -> Result<&'a str> {
    let line = lines
        .next()
        .ok_or_else(|| fixture_error(path, name, format!("file ends before `{directive}`")))?;
    line.strip_prefix(directive).map(str::trim).ok_or_else(|| {
        fixture_error(
            path,
            name,
            format!("expected `{directive}` after {after}, found {line:?}"),
        )
    })
}

/// The position a positional operation runs at: an explicit `POSITION:` line,
/// else the code block's cursor marker. A fixture with neither has nowhere to
/// send the request.
fn operation_position<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    cursor: Option<(usize, usize)>,
    path: &Path,
    name: &str,
    op_type: &str,
) -> Result<(usize, usize)> {
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

fn parse_lsp_test_file(path: &Path) -> Result<Vec<LspTest>> {
    let contents = fs_err::read_to_string(path)?;
    let mut tests: Vec<LspTest> = Vec::new();
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

        let test_type = fixture_directive(&mut lines, path, &name, "TYPE:", "`TEST:`")?;
        if test_type == "edit_sequence" {
            skip_to_block_end(&mut lines, path, &name)?;
            continue;
        }
        let Some((reply_kind, _, _)) = fixture_request(test_type) else {
            return Err(anyhow::anyhow!(
                "{}: test '{}' has TYPE '{}', which has no reply decoder",
                path.display(),
                name,
                test_type
            ));
        };

        // Multi-file mode: `FILES:` after `TYPE:` instead of a code block.
        let source = lines.next_if(|line| line.starts_with("SOURCE:"));
        let shared = source
            .map(|line| {
                let source = line.strip_prefix("SOURCE:").unwrap().trim();
                tests
                    .iter()
                    .find(|test| test.name == source)
                    .ok_or_else(|| {
                        fixture_error(path, &name, format!("unknown earlier SOURCE {source:?}"))
                    })
            })
            .transpose()?;
        let is_multi_file = shared.map_or_else(
            || lines.peek().is_some_and(|line| line.trim() == "FILES:"),
            |test| !test.files.is_empty(),
        );

        let mut files: Vec<(String, String)> = Vec::new();
        let mut primary_file = String::new();
        let mut cursor: Option<(usize, usize)> = None;
        let mut code = String::new();

        if let Some(test) = shared {
            code = test.code.clone();
            files = test.files.clone();
            primary_file = test.primary_file.clone();
            if !files.is_empty()
                && lines
                    .peek()
                    .is_some_and(|line| line.starts_with("REPLACE:"))
            {
                return Err(fixture_error(
                    path,
                    &name,
                    "source replacements require a single file",
                ));
            }
            while let Some(line) = lines.next_if(|line| line.starts_with("REPLACE:")) {
                replace_fixture_source(&mut code, line, path, &name)?;
            }
        } else if is_multi_file {
            lines.next(); // consume `FILES:`
            let (parsed_files, cur) = parse_files_block(
                &mut lines,
                &[
                    "OPERATION:",
                    "POSITION:",
                    "NEW_NAME:",
                    "QUERY:",
                    "EXPECTED:",
                ],
                path,
                &name,
            )?;
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

        let op_type = lines
            .next_if(|line| line.starts_with("OPERATION:"))
            .map_or(test_type, |line| {
                line.strip_prefix("OPERATION:").unwrap().trim()
            });
        let Some((op_reply, subcommand, args)) = fixture_request(op_type) else {
            return Err(anyhow::anyhow!(
                "{}: test '{}' has OPERATION '{}', which has no reply decoder",
                path.display(), name, op_type
            ));
        };
        let position = match args {
            RequestArgs::File | RequestArgs::Query => None,
            _ => Some(operation_position(
                &mut lines, cursor, path, &name, op_type,
            )?),
        };
        let operation = if matches!(args, RequestArgs::CodeAction) {
            let (line, character) = position.unwrap();
            LspOperation::CodeAction { line, character }
        } else {
            let argument = match args {
                RequestArgs::Query => Some(optional_directive(&mut lines, "QUERY:", "")),
                RequestArgs::Rename => Some(optional_directive(&mut lines, "NEW_NAME:", "renamed")),
                _ => None,
            };
            LspOperation::OneShot {
                subcommand,
                position,
                argument,
            }
        };
        if reply_kind != op_reply {
            return Err(anyhow::anyhow!(
                "{}: test '{}' has TYPE '{}' but OPERATION '{}' answers a {:?} reply",
                path.display(),
                name,
                test_type,
                op_type,
                op_reply
            ));
        }

        // Skip separator if present
        lines.next_if(|l| l.starts_with("---"));

        fixture_directive(&mut lines, path, &name, "EXPECTED:", "the operation")?;

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
                expected_code = Some(fixture_result(&code, &rename_lines, path, &name)?);
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
                (false, Some(result)) => {
                    expected_code = Some(fixture_result(&code, &result, path, &name)?)
                }
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
            test_type: test_type.to_string(),
            reply_kind,
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

fn fixture_result(source: &str, lines: &[String], path: &Path, name: &str) -> Result<String> {
    if !lines
        .first()
        .is_some_and(|line| line.starts_with("REPLACE:"))
    {
        return Ok(lines.join("\n"));
    }
    let mut result = source.to_string();
    for line in lines {
        replace_fixture_source(&mut result, line, path, name)?;
    }
    Ok(result)
}

fn replace_fixture_source(source: &mut String, line: &str, path: &Path, name: &str) -> Result<()> {
    let pair = line.strip_prefix("REPLACE:").ok_or_else(|| {
        fixture_error(
            path,
            name,
            "REPLACE expectations cannot mix with source text",
        )
    })?;
    let (old, new): (String, String) = serde_json::from_str(pair)
        .map_err(|error| fixture_error(path, name, format!("invalid REPLACE: {error}")))?;
    if old.is_empty() || !source.contains(&old) {
        return Err(fixture_error(
            path,
            name,
            "REPLACE source is empty or absent",
        ));
    }
    *source = source.replace(&old, &new);
    Ok(())
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
    if op == "semantic_tokens" {
        return Some(EditStepOperation::SemanticTokens);
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

        let mut steps: Vec<EditStep> = Vec::new();

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

            let mode = match mode {
                StepMode::FullReplace { code } if code.starts_with("SOURCE:") => {
                    let mut body = code.lines();
                    let source = body.next().unwrap().strip_prefix("SOURCE:").unwrap().trim();
                    let reference = source
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|index| steps.get(index));
                    let Some(EditStep {
                        mode: StepMode::FullReplace { code },
                        ..
                    }) = reference
                    else {
                        return Err(fixture_error(
                            path,
                            &name,
                            "SOURCE must name an earlier full-document step",
                        ));
                    };
                    let mut code = code.clone();
                    for replacement in body {
                        replace_fixture_source(&mut code, replacement, path, &name)?;
                    }
                    StepMode::FullReplace { code }
                }
                mode => mode,
            };

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
        let mut stripped_offset: usize = 0; // UTF-16 column in the stripped line
        while let Some(open_rel) = rest.find(OPEN) {
            // Text before the marker contributes to the stripped column.
            let before = &rest[..open_rel];
            stripped_offset += before.encode_utf16().count();
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

/// The server binary a fixture talks to. Tests pass a stub binary here.
#[derive(Clone, Copy)]
struct LspServerCmd<'a> {
    bin: &'a Path,
}

impl LspServerCmd<'_> {
    /// `<bin> lsp`. The one-shot handlers and the interactive server both live
    /// under `rock lsp`; there is no separate `rock-lsp` binary.
    fn command(&self) -> Command {
        let mut cmd = Command::new(self.bin);
        cmd.arg("lsp");
        cmd
    }
}

/// The shape a fixture's reply must have, one variant per decoder.
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

enum RequestArgs {
    File,
    Position,
    Rename,
    Query,
    CodeAction,
}

fn fixture_request(name: &str) -> Option<(ReplyKind, &'static str, RequestArgs)> {
    use ReplyKind::*;
    Some(match name {
        "completion" => (Completion, "completion", RequestArgs::Position),
        "goto_definition" => (Definition, "definition", RequestArgs::Position),
        "goto_type_definition" => (Definition, "type-definition", RequestArgs::Position),
        "references" => (References, "references", RequestArgs::Position),
        "semantic_tokens" => (SemanticTokens, "semantic-tokens", RequestArgs::File),
        "signature_help" => (SignatureHelp, "signature-help", RequestArgs::Position),
        "inlay_hints" => (InlayHints, "inlay-hints", RequestArgs::File),
        "hover" => (Hover, "hover", RequestArgs::Position),
        "document_symbols" => (Symbols, "document-symbols", RequestArgs::File),
        "workspace_symbols" => (Symbols, "workspace-symbols", RequestArgs::Query),
        "rename" => (Rename, "rename", RequestArgs::Rename),
        "code_action" => (CodeAction, "", RequestArgs::CodeAction),
        _ => return None,
    })
}

fn optional_directive<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    prefix: &str,
    default: &str,
) -> String {
    lines
        .next_if(|line| line.starts_with(prefix))
        .map_or(default, |line| line[prefix.len()..].trim())
        .to_string()
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

fn decode_reply(request: &str, raw: &str) -> Result<serde_json::Value> {
    serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("{request}: reply is not JSON ({e}); raw reply: {raw}"))
}

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

fn all_expected_substrings<'a>(
    values: impl Iterator<Item = &'a str> + Clone,
    expected: &[String],
) -> bool {
    expected
        .iter()
        .all(|item| values.clone().any(|value| value.contains(item.as_str())))
}

fn reference_locations_match(actual: &[(u64, u64)], expected: &[(u64, u64)]) -> bool {
    let mut actual = actual.to_vec();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    actual == expected
}

fn minimum_token_counts_match(count: usize, expected: &[String]) -> bool {
    expected.iter().all(|item| {
        item.strip_prefix("min_tokens:")
            .is_none_or(|minimum| count >= minimum.trim().parse::<usize>().unwrap_or(0))
    })
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

    let (subcommand, position, argument) = match &test.operation {
        LspOperation::OneShot {
            subcommand,
            position,
            argument,
        } => (subcommand, position, argument),
        LspOperation::CodeAction { line, character } => {
            let result = run_code_action_test(test, server, &test_file, *line, *character);
            let _ = fs_err::remove_dir_all(&project_dir);
            return result;
        }
    };

    let target = match position {
        Some((line, character)) => {
            format!("{}:{}:{}", test_file.display(), line + 1, character + 1)
        }
        None => test_file.display().to_string(),
    };
    let output = server
        .command()
        .arg(subcommand)
        .arg(target)
        .args(argument)
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let _ = fs_err::remove_dir_all(&project_dir);

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

    let request = format!("{} test '{}'", test.test_type, test.name);

    let all_found = match test.reply_kind {
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

    if std::env::var("ROCK_TEST_VERBOSE").is_ok() && !all_found {
        eprintln!("    Actual output: {}", stdout);
    }

    Ok(all_found)
}

/// Write the fixture's files into a fresh project directory under
/// `.lsp-test-tmp/` and return the primary file and that directory.
fn write_fixture_project(test: &LspTest) -> Result<(PathBuf, PathBuf)> {
    let (safe_name, proj) = new_fixture_project(&test.name, !test.files.is_empty())?;

    // Multi-file tests get a named package manifest and every `FILE:` entry.
    // The primary file (the one with the cursor marker) is the path passed to
    // the rock-lsp CLI.
    if !test.files.is_empty() {
        for (name, content) in &test.files {
            // The harness writes its own manifest below.
            if name == "stick.toml" {
                continue;
            }
            fs_err::write(proj.join(name), content)?;
        }
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
        // Isolate each file so requests that load its project never see parallel fixtures.
        let tf = proj.join(format!("{}.rock", safe_name));
        fs_err::write(&tf, &test.code)?;
        Ok((tf, proj))
    }
}

/// Create an isolated temporary directory for one fixture, optionally as a
/// named package for multi-file tests.
fn new_fixture_project(name: &str, with_manifest: bool) -> Result<(String, PathBuf)> {
    let test_dir = PathBuf::from(".lsp-test-tmp");
    fs_err::create_dir_all(&test_dir)?;

    let safe_name: String = name
        .chars()
        .map(|c| if c == ' ' || c == '/' { '_' } else { c })
        .collect();
    static NEXT_PROJECT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT_PROJECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let project_dir = test_dir.join(format!("{safe_name}-{}-{n}", std::process::id()));
    fs_err::create_dir_all(&project_dir)?;
    if with_manifest {
        fs_err::write(
            project_dir.join("stick.toml"),
            "[package]\nname = \"lsp_test\"\nversion = \"0.1.0\"\n",
        )?;
    }
    Ok((safe_name, project_dir))
}

fn check_completion_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let json = decode_reply(request, stdout)?;
    let labels: Vec<String> = reply_array(request, stdout, &json, "completions")?
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .map(str::to_string)
        .collect();
    if std::env::var("ROCK_TEST_VERBOSE").is_ok() {
        eprintln!("    Checking completions for test '{}'", test.name);
        eprintln!("    Available completions: {:?}", labels);
        eprintln!("    Expected items: {:?}", test.expected);
    }
    Ok(all_expected_substrings(
        labels.iter().map(String::as_str),
        &test.expected,
    ))
}

fn check_definition_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let json = decode_reply(request, stdout)?;
    let definitions = reply_array(request, stdout, &json, "definitions")?;
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    if expects_no_locations(&test.expected) {
        if verbose && !definitions.is_empty() {
            eprintln!("    Expected no definitions, got {:?}", definitions);
        }
        return Ok(definitions.is_empty());
    }
    let Some(definition) = definitions.first() else {
        if verbose {
            eprintln!("    No definitions found in output");
            eprintln!("    Actual output: {}", stdout);
        }
        return Ok(false);
    };
    let mut matches = true;
    for item in &test.expected {
        let Some((key @ ("line" | "character"), value)) = item.split_once(':') else {
            continue;
        };
        let expected = value.trim().parse::<usize>().unwrap_or(0) as u64;
        match definition["range"]["start"][key].as_u64() {
            Some(actual) if actual == expected => {}
            Some(actual) => {
                matches = false;
                if verbose {
                    eprintln!("    Expected {key} {expected}, got {actual}");
                }
            }
            None => {
                matches = false;
                if verbose {
                    eprintln!("    No {key} field found in definition range");
                }
            }
        }
    }
    Ok(matches)
}

fn expects_no_locations(expected: &[String]) -> bool {
    expected
        .iter()
        .any(|line| line.trim().eq_ignore_ascii_case("none"))
}

fn reference_start(location: &serde_json::Value) -> Option<(u64, u64)> {
    let start = &location["range"]["start"];
    Some((start["line"].as_u64()?, start["character"].as_u64()?))
}

fn check_references_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let json = decode_reply(request, stdout)?;
    let actual: Vec<(u64, u64)> = reply_array(request, stdout, &json, "references")?
        .iter()
        .filter_map(reference_start)
        .collect();
    let mut expected = Vec::new();
    if !expects_no_locations(&test.expected) {
        let mut line = None;
        for item in &test.expected {
            let item = item.trim();
            if item.starts_with("file:") {
                line = None;
            } else if let Some(value) = item.strip_prefix("line:") {
                line = value.trim().parse().ok();
            } else if let Some(value) = item.strip_prefix("character:") {
                if let (Some(line), Ok(character)) = (line, value.trim().parse::<u64>()) {
                    expected.push((line, character));
                }
            }
        }
    }
    let matches = reference_locations_match(&actual, &expected);
    if !matches && std::env::var("ROCK_TEST_VERBOSE").is_ok() {
        eprintln!(
            "    Reference mismatch: got {:?}, expected {:?}",
            actual, expected
        );
    }
    Ok(matches)
}

fn check_semantic_tokens_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let json = decode_reply(request, stdout)?;
    let token_count = if let Some(count) = json.get("count").and_then(|value| value.as_u64()) {
        count as usize
    } else if let Some(tokens) = json.get("tokens").and_then(|value| value.as_array()) {
        tokens.len()
    } else if let Some(data) = json.get("data").and_then(|value| value.as_array()) {
        data.len() / 5
    } else {
        return Err(anyhow::anyhow!(
            "{request}: reply has no `count`, `tokens` or `data` field; raw reply: {stdout}"
        ));
    };
    let matches = token_count > 0 && minimum_token_counts_match(token_count, &test.expected);
    if !matches && std::env::var("ROCK_TEST_VERBOSE").is_ok() {
        eprintln!(
            "    Expected semantic tokens {:?}, got {}",
            test.expected, token_count
        );
    }
    Ok(matches)
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

    // Type and parameter hints disagree about the padding column.
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
    let json = decode_reply(request, stdout)?;
    let contents = json
        .get("hover")
        .and_then(|hover| hover.get("contents"))
        .and_then(|contents| {
            contents
                .as_str()
                .or_else(|| contents.get("value").and_then(|value| value.as_str()))
        })
        .ok_or_else(|| {
            anyhow::anyhow!("{request}: reply has no `hover.contents`; raw reply: {stdout}")
        })?;
    let expected = test.expected.join("\n");
    let matches = contents.trim() == expected.trim();
    if !matches && std::env::var("ROCK_TEST_VERBOSE").is_ok() {
        eprintln!("    Hover content mismatch");
        eprintln!("    Expected:\n{}", expected.trim());
        eprintln!("    Actual:\n{}", contents.trim());
    }
    Ok(matches)
}

fn check_symbols_reply(test: &LspTest, request: &str, stdout: &str) -> Result<bool> {
    let json = decode_reply(request, stdout)?;
    let symbols = json["symbols"]
        .as_array()
        .or_else(|| json.as_array())
        .ok_or_else(|| {
            anyhow::anyhow!("{request}: reply has no `symbols` array; raw reply: {stdout}")
        })?;
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let mut all_found = true;
    for item in &test.expected {
        let (name, kind) = match item.split_once(':') {
            Some((name, kind)) if !kind.contains(':') => (name.trim(), Some(kind.trim())),
            _ => (item.as_str(), None),
        };
        let found = symbols.iter().any(|symbol| {
            symbol["name"].as_str() == Some(name)
                && kind.is_none_or(|kind| {
                    symbol_kind_name(&symbol["kind"])
                        .is_some_and(|actual| actual.to_lowercase().contains(&kind.to_lowercase()))
                })
        });
        if !found {
            all_found = false;
            if verbose {
                match kind {
                    Some(kind) => {
                        eprintln!("    Expected symbol '{name}' of kind '{kind}' not found")
                    }
                    None => eprintln!("    Expected symbol '{name}' not found"),
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
                // The renamer never touches the manifest.
                if fname == "stick.toml" {
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

/// Apply a single-file rename's `changes` to `code`.
fn apply_rename_changes(
    code: &str,
    changes: &serde_json::Map<String, serde_json::Value>,
) -> String {
    changes
        .values()
        .filter_map(|edits| edits.as_array())
        .fold(code.to_string(), |code, edits| {
            apply_text_edits(&code, &edits.iter().collect::<Vec<_>>())
        })
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
    server.shutdown()?;
    check_code_action_reply(test, &uri, &reply?)
}

/// Open `code` as `uri` and send `textDocument/codeAction` for an empty range
/// at the position, returning the whole JSON-RPC response.
fn request_code_actions(
    server: &mut LspServer,
    uri: &str,
    code: &str,
    line: usize,
    character: usize,
) -> Result<serde_json::Value> {
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": { "uri": uri, "languageId": "rock", "version": 1, "text": code }
        }),
    )?;
    let position = serde_json::json!({ "line": line, "character": character });
    server.request(
        "textDocument/codeAction",
        serde_json::json!({
            "textDocument": { "uri": uri },
            "range": { "start": position, "end": position },
            "context": { "diagnostics": [] }
        }),
    )
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
    next_id: i64,
}

impl LspServer {
    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.stdin.write_all(&encode_jsonrpc(&message))?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Send a request under the next id and wait for its response.
    fn request(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.stdin.write_all(&encode_jsonrpc(&message))?;
        self.stdin.flush()?;
        read_response_from_chan(&self.rx, id)
    }

    /// Send `shutdown` and `exit`, wait for the child, and stop the watchdog.
    fn shutdown(mut self) -> Result<()> {
        // A slow server may not answer `shutdown`; `exit` still ends it.
        let _ = self.request("shutdown", serde_json::Value::Null);
        self.notify("exit", serde_json::Value::Null)?;
        let _ = self.child.wait();
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

    let stdin = child
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
    let timeout_secs: u64 = 60;
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

    let mut server = LspServer {
        child,
        stdin,
        rx: msg_rx,
        done: test_done,
        next_id: 1,
    };
    server.request(
        "initialize",
        serde_json::json!({
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
        }),
    )?;
    server.notify("initialized", serde_json::json!({}))?;
    Ok(server)
}

/// Insert `ch` at (line, col) of `doc`.
fn insert_char_at(doc: &str, line: usize, col: usize, ch: char) -> String {
    let mut lines: Vec<String> = doc.lines().map(|l| l.to_string()).collect();
    while lines.len() <= line {
        lines.push(String::new());
    }
    let target_line = &mut lines[line];
    let col = col.min(target_line.len());
    target_line.insert(col, ch);
    lines.join("\n")
}

/// Backspace at (line, col) of `doc`; returns the document and new cursor.
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

/// Drive one edit sequence against a persistent server. Returns a label for
/// every failed check.
fn run_edit_sequence_test(test: &EditSequenceTest, server: LspServerCmd) -> Result<Vec<String>> {
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let mut failures = Vec::new();

    let StepMode::FullReplace { code: initial_code } = &test.steps[0].mode else {
        return Err(anyhow::anyhow!(
            "First step in edit_sequence must be a full document (STEP N:), not a typing/delete step"
        ));
    };
    let (safe_name, project_dir) = new_fixture_project(&test.name, false)?;
    let test_file = project_dir.join(format!("{safe_name}.rock"));
    std::fs::write(&test_file, initial_code)?;
    let test_uri = file_uri(&std::fs::canonicalize(&test_file)?)?;

    let mut server = start_lsp_server(server, &project_dir, &test.name)?;
    let mut version: i64 = 1;
    let mut current_doc = initial_code.clone();
    server.notify(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": test_uri,
                "languageId": "rock",
                "version": version,
                "text": current_doc
            }
        }),
    )?;

    // The server compiles synchronously inside didOpen/didChange, so the
    // next request's response is the sync point; no sleeps are needed.
    let did_change = |server: &mut LspServer, version: i64, text: &str| {
        server.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": test_uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
    };

    for (step_idx, step) in test.steps.iter().enumerate() {
        if step_idx > 0 {
            match &step.mode {
                StepMode::FullReplace { code } => {
                    version += 1;
                    current_doc = code.clone();
                    std::fs::write(&test_file, &current_doc)?;
                    did_change(&mut server, version, &current_doc)?;
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
                        did_change(&mut server, version, &current_doc)?;
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
                        did_change(&mut server, version, &current_doc)?;
                    }
                    std::fs::write(&test_file, &current_doc)?;
                }
            }
        }

        for check in &step.checks {
            let at = |line: &usize, character: &usize| {
                serde_json::json!({
                    "textDocument": { "uri": test_uri },
                    "position": { "line": line, "character": character }
                })
            };
            let check_passed = match &check.operation {
                EditStepOperation::SemanticTokens => {
                    let response = server.request(
                        "textDocument/semanticTokens/full",
                        serde_json::json!({ "textDocument": { "uri": test_uri } }),
                    )?;
                    check_semantic_tokens_response(&response, &check.expected, step_idx, verbose)
                }
                EditStepOperation::Completion { line, character } => {
                    let response =
                        server.request("textDocument/completion", at(line, character))?;
                    check_completion_response(&response, &check.expected, step_idx, verbose)
                }
                EditStepOperation::Hover { line, character } => {
                    let response = server.request("textDocument/hover", at(line, character))?;
                    check_hover_response(&response, &check.expected, step_idx, verbose)
                }
                EditStepOperation::References { line, character } => {
                    let mut params = at(line, character);
                    params["context"] = serde_json::json!({ "includeDeclaration": true });
                    let response = server.request("textDocument/references", params)?;
                    check_references_response(&response, &check.expected, step_idx, verbose)
                }
            };
            if !check_passed {
                failures.push(format!("step {} {:?}", step_idx + 1, check.operation));
            }
        }
    }

    server.shutdown()?;
    let _ = fs_err::remove_dir_all(&project_dir);
    Ok(failures)
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

    let all_ok = minimum_token_counts_match(token_count, expected)
        && (!expected.iter().any(|item| item == "non_zero") || token_count > 0);
    if !all_ok && verbose {
        eprintln!(
            "    Step {}: FAIL - semantic token expectations not met",
            step_idx + 1
        );
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

    let all_ok = all_expected_substrings(labels.iter().map(String::as_str), expected);
    if !all_ok && verbose {
        eprintln!(
            "    Step {}: FAIL - expected completion not found",
            step_idx + 1
        );
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
    let got: Vec<(u64, u64)> = response
        .get("result")
        .and_then(|r| r.as_array())
        .map(|locs| locs.iter().filter_map(reference_start).collect())
        .unwrap_or_default();
    let want: Vec<(u64, u64)> = expected
        .iter()
        .filter_map(|l| {
            let (a, b) = l.trim().split_once(':')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .collect();
    if verbose {
        eprintln!("    Step {}: references = {:?}", step_idx + 1, got);
    }
    let ok = reference_locations_match(&got, &want);
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

    let all_ok = all_expected_substrings(std::iter::once(hover_text), expected);
    if !all_ok && verbose {
        eprintln!(
            "    Step {}: FAIL - expected hover content not found",
            step_idx + 1
        );
    }
    all_ok
}

/// Result of running every fixture in one `.rock.lsp` file.
#[derive(Debug, Clone)]
struct FileTestResult {
    file_name: String,
    passed: usize,
    failed: usize,
    duration: std::time::Duration,
    /// (fixture label, error detail) for each failed fixture.
    failed_test_details: Vec<(String, String)>,
}

impl FileTestResult {
    fn record(&mut self, name: &str, kind: &str, failure: Option<String>) {
        if let Some(detail) = failure {
            self.failed += 1;
            self.failed_test_details
                .push((format!("{name} ({kind})"), detail));
        } else {
            self.passed += 1;
        }
    }
}

/// Print one file's result as `name ...... ok (10/12 tests, 45ms)`, then its
/// failures.
fn print_file_result(r: &FileTestResult) {
    use colored::Colorize;
    let total = r.passed + r.failed;
    let total_ms = r.duration.as_secs_f64() * 1000.0;
    let dots = ".".repeat(60_usize.saturating_sub(r.file_name.len().min(50)));
    let status = if r.failed == 0 {
        "ok".green()
    } else {
        "FAILED".red()
    };
    println!(
        "{} {} {} ({}/{} tests, {:.0}ms)",
        r.file_name, dots, status, r.passed, total, total_ms,
    );
    for (test_name, error) in &r.failed_test_details {
        println!("  {} ... {}", test_name, "FAILED".red());
        for line in error.lines() {
            println!("    {}", line);
        }
    }
}

/// Run all tests (single-shot + edit_sequence) in a single .rock.lsp file.
fn run_lsp_file(file: &Path, display_name: &str, server: LspServerCmd) -> FileTestResult {
    let start_time = bone_core::prelude::Instant::now();
    let mut result = FileTestResult {
        file_name: display_name.to_string(),
        passed: 0,
        failed: 0,
        duration: std::time::Duration::ZERO,
        failed_test_details: Vec::new(),
    };
    let parsed =
        parse_lsp_test_file(file).and_then(|tests| Ok((tests, parse_edit_sequence_tests(file)?)));
    let (tests, edit_seq_tests) = match parsed {
        Ok(parsed) => parsed,
        Err(e) => {
            result.failed = 1;
            result
                .failed_test_details
                .push(("parse".to_string(), e.to_string()));
            result.duration = start_time.elapsed();
            return result;
        }
    };

    for test in &tests {
        let detail = match run_lsp_test(test, server) {
            Ok(true) => None,
            Ok(false) => Some(String::new()),
            Err(e) => Some(e.to_string()),
        };
        result.record(&test.name, &test.test_type, detail);
    }

    // Each edit sequence drives its own persistent server.
    for seq_test in &edit_seq_tests {
        let detail = match run_edit_sequence_test(seq_test, server) {
            Ok(failures) if failures.is_empty() => None,
            Ok(failures) => Some(failures.join(", ")),
            Err(e) => Some(e.to_string()),
        };
        result.record(&seq_test.name, "edit_sequence", detail);
    }

    result.duration = start_time.elapsed();
    result
}

/// The fixture root `path` selects, if any: a `.lsp` file, a `tests/lsp`
/// directory or a direct child of one (which selects the whole directory), or
/// a directory whose `lsp` subdirectory holds fixtures.
fn lsp_suite_root(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return (path.extension().and_then(|s| s.to_str()) == Some("lsp"))
            .then(|| path.to_path_buf());
    }
    if path.ends_with("tests/lsp") && path.is_dir() {
        return Some(path.to_path_buf());
    }
    if let Some(parent) = path.parent().filter(|p| p.ends_with("tests/lsp")) {
        return parent.is_dir().then(|| parent.to_path_buf());
    }
    let subdir = path.join("lsp");
    (subdir.is_dir() && !lsp_files_under(&subdir).is_empty()).then_some(subdir)
}

fn lsp_files_under(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| {
            e.file_type().is_file() && e.path().extension().and_then(|s| s.to_str()) == Some("lsp")
        })
        .map(|e| e.into_path())
        .collect()
}

/// Run the `.rock.lsp` suite under `path` (a file or a tree containing
/// `tests/lsp`) if present. Returns the number of failed fixtures.
pub(crate) fn run_lsp_suite(path: &Path) -> Result<usize> {
    let Some(root) = lsp_suite_root(path) else {
        return Ok(0);
    };
    let (mut lsp_files, display_root) = if root.is_file() {
        println!("\nRunning LSP test file {}...", root.display());
        let parent = root.parent().unwrap_or(&root).to_path_buf();
        (vec![root], parent)
    } else {
        println!("\nRunning LSP tests in {}...", root.display());
        (lsp_files_under(&root), root)
    };

    // `*_stress.rock.lsp` files are slow; they run only on request.
    if std::env::var("ROCK_LSP_RUN_STRESS").is_err() {
        let before = lsp_files.len();
        lsp_files.retain(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.ends_with("_stress.rock.lsp"))
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
        return Ok(0);
    }

    let start_time = bone_core::prelude::Instant::now();
    // Fixtures run the executable running this suite, so they always test the
    // build under test.
    let bin = std::env::current_exe().map_err(|e| {
        anyhow::anyhow!("cannot find the running executable to use as the LSP server: {e}")
    })?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let results = crate::parallel_runner::map_in_order(&lsp_files, threads, |file| {
        // Relative to the suite root, minus `.lsp`: `completion/basic.rock`.
        let relative = file.strip_prefix(&display_root).unwrap_or(file);
        let relative = relative.to_string_lossy();
        let display_name = relative.strip_suffix(".lsp").unwrap_or(&relative);
        run_lsp_file(file, display_name, LspServerCmd { bin: &bin })
    });
    let (mut passed, mut failed) = (0, 0);
    for r in &results {
        passed += r.passed;
        failed += r.failed;
        if r.passed + r.failed > 0 {
            print_file_result(r);
        }
    }
    let total = passed + failed;

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
            0,
            start_time.elapsed().as_secs_f64()
        );
    }

    Ok(failed)
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
mod tests {
    use super::*;

    const HOVER: &str = "TEST: broken\nTYPE: hover\n---\nfn main() {\n    ma\u{ab}cursor\u{bb}in()\n}\n---\nOPERATION: hover\nEXPECTED:\nfn main()\n===\n";

    fn fixture_error(fixture: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, fixture).unwrap();
        parse_lsp_test_file(&path).unwrap_err().to_string()
    }

    #[test]
    fn malformed_fixtures_keep_their_name_and_reason() {
        let cases = [
            (
                HOVER.replace("OPERATION: hover", "OPERATION: completion"),
                "TYPE 'hover'",
            ),
            (
                HOVER.replace("---\nOPERATION:", "OPERATION:"),
                "closing \x60---\x60",
            ),
            (
                HOVER.replace("EXPECTED:\nfn main()\n===\n", "EXPECTED:\n\n===\n"),
                "EXPECTED block is empty",
            ),
        ];
        for (fixture, reason) in cases {
            let error = fixture_error(&fixture);
            assert!(
                error.contains("broken") && error.contains(reason),
                "{error}"
            );
        }
    }

    #[test]
    fn malformed_edit_sequence_checks_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(
            &path,
            "TEST: broken\nTYPE: edit_sequence\n---\nSTEP 1:\nfn main() {}\n---\nCHECK: teleport\n===\n",
        )
        .unwrap();
        let error = parse_edit_sequence_tests(&path).unwrap_err().to_string();
        assert!(
            error.contains("broken") && error.contains("malformed check"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replies_are_checked_against_the_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("stub-rock");
        test_utils::write_executable(&bin, "#!/bin/sh\ncat \"${0%/*}/reply\"\n");
        let path = dir.path().join("fixture.rock.lsp");
        let cases = [
            (HOVER, "not json fn main()", "reply is not JSON"),
            (HOVER, "{}", "reply has no `hover.contents`"),
            (HOVER, r#"{"hover":{"contents":"other"}}"#, ""),
        ];
        for (fixture, reply, fragment) in cases {
            std::fs::write(&path, fixture).unwrap();
            std::fs::write(dir.path().join("reply"), reply).unwrap();
            let result = run_lsp_file(&path, "fixture", LspServerCmd { bin: &bin });
            assert_eq!((result.passed, result.failed), (0, 1), "{reply}");
            let detail = &result.failed_test_details[0].1;
            assert!(detail.contains(fragment), "{detail}");
            if !fragment.is_empty() {
                assert!(
                    detail.contains("broken") && detail.contains(reply),
                    "{detail}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_lsp_process_fails_the_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("stub-rock");
        test_utils::write_executable(&bin, "#!/bin/sh\nexit 7\n");
        let path = dir.path().join("fixture.rock.lsp");
        std::fs::write(&path, HOVER).unwrap();
        let tests = parse_lsp_test_file(&path).unwrap();
        assert!(!run_lsp_test(&tests[0], LspServerCmd { bin: &bin }).unwrap());
    }

    #[test]
    fn text_edits_use_utf16_columns_and_reject_overlaps() {
        let edit = |start, end, text| {
            serde_json::json!({
                "range": {"start": {"line": 0, "character": start}, "end": {"line": 0, "character": end}},
                "newText": text
            })
        };
        assert_eq!(
            apply_lsp_text_edits("\u{e9}ab", &[&edit(1, 2, "X"), &edit(9, 99, "!")]).unwrap(),
            "\u{e9}Xb!"
        );
        assert!(apply_lsp_text_edits("abc", &[&edit(0, 2, "X"), &edit(1, 3, "Y")]).is_err());
    }
}
