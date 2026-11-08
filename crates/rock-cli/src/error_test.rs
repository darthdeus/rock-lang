use anyhow::{Result, anyhow};
use bone_core::prelude::Instant;
use colored::Colorize;
use fs_err as fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug)]
struct ErrorTest {
    test_cases: Vec<ErrorTestCase>,
    is_todo: bool,
}

#[derive(Debug)]
struct ErrorTestCase {
    name: String,
    /// Source files for this test case. Single-entry `[("test.rock", body)]` for the
    /// common case; multiple entries when the body uses `//--- FILE: path ---` markers
    /// to split across modules (for cross-module error scenarios).
    files: Vec<(String, String)>,
    expected_error_kind: String,
    /// `//! expect: <text>` lines: substrings the rejecting diagnostic must
    /// contain. Empty means "any stage may reject, for any reason" -- the
    /// original contract, kept for the cases that predate this.
    expected_reasons: Vec<String>,
}

#[derive(Debug)]
enum ErrorTestResult {
    Pass,
    Fail(String),
    Todo(String),
}

impl ErrorTest {
    fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let is_todo = content.lines().any(|l| l.contains("//! TODO:"));

        // Parse test cases from the .err file
        let test_cases = parse_error_test_cases(&content)?;

        Ok(ErrorTest {
            test_cases,
            is_todo,
        })
    }
}

fn parse_error_test_cases(content: &str) -> Result<Vec<ErrorTestCase>> {
    let mut test_cases = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        // Look for test case start: //=== TEST CASE: name_ExpectedErrorKind{params} ===
        if line.starts_with("//=== TEST CASE:") && line.ends_with(" ===") {
            let case_header = line
                .strip_prefix("//=== TEST CASE:")
                .unwrap()
                .strip_suffix(" ===")
                .unwrap()
                .trim();

            // Parse the expected error from the test case name. The `{params}`
            // suffix is validated but not checked against the diagnostic.
            let (case_name, expected_error_kind, _params) = parse_test_case_name(case_header)?;

            // Collect the input code until we find the next test case or end of file
            i += 1;
            let mut input_lines = Vec::new();

            while i < lines.len() {
                let current_line = lines[i];

                // Stop if we hit the next test case
                if current_line.trim().starts_with("//=== TEST CASE:") {
                    break;
                }

                // Stop if we hit a comment that looks like another delimiter
                if current_line.trim().starts_with("//===") {
                    break;
                }

                input_lines.push(current_line);
                i += 1;
            }

            // `//! expect:` is harness input, not source: strip it before the
            // body becomes files, so it cannot reach the parser.
            let mut expected_reasons = Vec::new();
            let body: Vec<&str> = input_lines
                .iter()
                .filter(|l| match l.trim().strip_prefix("//! expect:") {
                    Some(want) => {
                        expected_reasons.push(want.trim().to_string());
                        false
                    }
                    None => true,
                })
                .copied()
                .collect();
            let files = split_body_into_files(&body);

            test_cases.push(ErrorTestCase {
                name: case_name,
                files,
                expected_error_kind,
                expected_reasons,
            });
        } else {
            i += 1;
        }
    }

    if test_cases.is_empty() {
        return Err(anyhow!("No test cases found in error test file"));
    }

    Ok(test_cases)
}

fn parse_test_case_name(case_header: &str) -> Result<(String, String, Option<String>)> {
    // Parse: "simple_undefined_UndefinedSymbol{name=\"undefined_function\"}"
    // Returns: ("simple_undefined", "UndefinedSymbol", Some("name=\"undefined_function\""))

    if let Some(underscore_pos) = case_header.rfind('_') {
        let case_name = case_header[..underscore_pos].to_string();
        let error_part = &case_header[underscore_pos + 1..];

        // Check if there are parameters in braces
        if let Some(brace_pos) = error_part.find('{') {
            let error_kind = error_part[..brace_pos].to_string();
            let params_part = &error_part[brace_pos + 1..];

            // Remove trailing '}'
            let params = if params_part.ends_with('}') {
                Some(params_part[..params_part.len() - 1].to_string())
            } else {
                return Err(anyhow!(
                    "Invalid parameter format in test case: {}",
                    case_header
                ));
            };

            Ok((case_name, error_kind, params))
        } else {
            Ok((case_name, error_part.to_string(), None))
        }
    } else {
        Err(anyhow!("Invalid test case name format: {}", case_header))
    }
}

/// Split a test-case body into one or more (path, contents) pairs.
///
/// If the body contains lines of the form `//--- FILE: <path> ---`, those act as
/// file separators and each trailing block becomes a separate source file. With no
/// markers the whole body is returned as a single `test.rock` file (legacy behavior).
fn split_body_into_files(body_lines: &[&str]) -> Vec<(String, String)> {
    let is_marker = |line: &str| {
        let t = line.trim();
        t.starts_with("//--- FILE:") && t.ends_with("---")
    };

    if !body_lines.iter().any(|l| is_marker(l)) {
        let input = body_lines.join("\n").trim().to_string();
        return vec![("test.rock".to_string(), input)];
    }

    let mut files: Vec<(String, String)> = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    let flush = |name: &Option<String>, lines: &[&str], out: &mut Vec<(String, String)>| {
        if let Some(n) = name {
            let content = lines.join("\n").trim().to_string();
            out.push((n.clone(), content));
        }
    };

    for line in body_lines {
        if is_marker(line) {
            flush(&current_name, &current_lines, &mut files);
            current_lines.clear();
            let t = line.trim();
            let inner = t
                .strip_prefix("//--- FILE:")
                .and_then(|s| s.strip_suffix("---"))
                .unwrap_or("")
                .trim();
            current_name = Some(inner.to_string());
        } else {
            current_lines.push(line);
        }
    }
    flush(&current_name, &current_lines, &mut files);

    files
}

// ───────────────────────── bone/skull front-end suite ─────────────────────────
//
// Error-tests assert REJECTION, which happens in the front end before codegen --
// so bone and skull (skull's front end IS bone) verify them identically. This
// runs each `.rock.err` case through bone's shared front end; the `test`
// command for either backend folds it in (like the LSP suite).

/// Bone's verdict on a (multi-file) case. `None` == bone rejected it (a parse or
/// type error), the expected outcome for a front-end error-test. `Some(ws)` ==
/// bone ACCEPTED; the workspace is returned so skull emission can run on it
/// (below) WITHOUT a second semantic pass.
fn bone_check_case(
    files: &[(String, String)],
) -> Result<bone_semantic::inference::Workspace, String> {
    let mut user = Vec::with_capacity(files.len());
    for (name, code) in files {
        // A case may declare `//! modules: v1` to exercise the v1 grammar.
        let dialect = match rock_host::source_files::parse_modules_directive(code) {
            Ok(d) => d,
            // anyhow error: an unknown `//! modules:` value is a load error.
            Err(e) => return Err(e.to_string()),
        };
        match skull_parser::parse_module_with_dialect(
            code,
            ustr::Ustr::from(name.as_str()),
            dialect,
        ) {
            Ok(m) => user.push(m),
            Err(e) => return Err(error_text(&[e])), // hard tokenizer failure
        }
    }
    // Total parse: a recoverable syntax error returns Ok with `recovery_errors`,
    // so the rejection is the downstream gate in `check_with_stdlib`
    // (`skull::pipeline::syntax_errors`), not the parse `match` above.
    crate::bone_backend::check_with_stdlib(user).map_err(|errs| error_text(&errs))
}

/// Every message a rejection carries -- primary, labels, and the siblings a
/// single-error return folded in. `//! expect:` matches against this text, so a
/// case can pin the reason a stage rejected for, not merely that it rejected.
fn error_text(errs: &[rock_core::errors::CompilerError]) -> String {
    fn push(out: &mut Vec<String>, e: &rock_core::errors::CompilerError) {
        out.push(e.message.clone());
        out.extend(e.labels.iter().map(|l| l.message.clone()));
        for sib in &e.siblings {
            push(out, sib);
        }
    }
    let mut out = Vec::new();
    for e in errs {
        push(&mut out, e);
    }
    out.join("\n")
}

/// Full-pipeline extension: does skull EMISSION reject this bone-accepted case?
/// Emit-time rejections (a reserved body-local name `r<N>`/`c<N>`/`__rock_env`,
/// or a compiler-minted synthetic-name collision) never reach the bone front
/// end, so a `.rock.err` for them is invisible to `bone_check_case`. Running
/// skull's MIR emit on each user module and treating a codegen `CompilerError`
/// as the case's rejection makes the suite STAGE-AGNOSTIC: a `.rock.err` says
/// "this must be rejected", and either bone or skull-emit may be the stage that
/// raises it. Only reached when bone already accepted, so it reuses `ws`.
fn skull_emit_rejects(
    ws: &bone_semantic::inference::Workspace,
    files: &[(String, String)],
) -> Option<String> {
    files.iter().find_map(|(name, _)| {
        let mid = *ws.module_ids.get(&ustr::Ustr::from(name.as_str()))?;
        skull::mir_emit::emit_workspace_mir(ws, mid)
            .err()
            .map(|e| error_text(&[e]))
    })
}

/// Third gate, same principle as [`skull_emit_rejects`]: an `Error`-severity
/// lint finding fails a real build (`rock lint`, and the pass every
/// compile/run/test drives), so a `.rock.err` may name it as the rejecting
/// stage. Soft tiers are ignored -- a `Warning` prints and builds, so it is not
/// a rejection. Sources come from `files`, which is also where
/// `// rock: allow(...)` suppression is read from, so a case can pin that an
/// allow comment does NOT rescue an unsuppressable lint.
///
/// The DEFAULT tier is what runs, matching a build: an opt-in lint
/// (`bone_lints::OPT_IN_LINTS`) is unreachable from any build, so a `.rock.err`
/// must not be able to name one as its rejector.
fn lint_rejects(
    ws: &bone_semantic::inference::Workspace,
    files: &[(String, String)],
) -> Option<String> {
    let sources: rustc_hash::FxHashMap<ustr::Ustr, &str> = files
        .iter()
        .map(|(n, c)| (ustr::Ustr::from(n.as_str()), c.as_str()))
        .collect();
    // The build pass's rule: everything the case did not author is stdlib,
    // and stdlib is scanned but never reported on.
    let warm = skull::pipeline::warm_module_ids(ws, &sources);
    bone_lints::lint_workspace(ws, &warm, true, &sources, &crate::registry::lint_options())
        .iter()
        .find(|f| f.level() == rock_core::errors::CompilerErrorLevel::Error)
        .map(|f| error_text(std::slice::from_ref(&f.error)))
}

/// Run the error-test suite through bone's front end (the skull/bone `test`
/// path). `backend` is informational -- bone is the shared front end, so both
/// verify rejection identically. The error-KIND suffix is rock-specific (bone
/// diagnostics mostly carry no kind), so this asserts REJECTION only.
pub fn run_error_suite(
    path: &Path,
    _backend: crate::Backend,
    verbose: bool,
) -> Result<(usize, usize, usize, usize)> {
    if !has_error_tests(path) {
        return Ok((0, 0, 0, 0));
    }
    let Some(dir) = find_error_test_dir(path) else {
        return Ok((0, 0, 0, 0));
    };
    println!(
        "\nRunning error tests in {} (bone front end)...",
        dir.display()
    );

    let start = Instant::now();
    let (mut total, mut passed, mut failed, mut todos) = (0usize, 0usize, 0usize, 0usize);
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    for entry in WalkDir::new(&dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        let is_err_file = p.is_file()
            && p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.ends_with(".rock.err"));
        if !is_err_file {
            continue;
        }

        let error_test = match ErrorTest::from_file(p) {
            Ok(t) => t,
            Err(e) => {
                failures.push((p.to_owned(), format!("Failed to parse test file: {}", e)));
                failed += 1;
                continue;
            }
        };

        for tc in &error_test.test_cases {
            total += 1;
            let result = if error_test.is_todo {
                ErrorTestResult::Todo("TODO".to_string())
            } else {
                // bone, then skull emission, then an Error-severity lint: any
                // of the three may be the rejecting stage.
                let rejection = match bone_check_case(&tc.files) {
                    Err(msg) => Some(msg),
                    Ok(ws) => {
                        skull_emit_rejects(&ws, &tc.files).or_else(|| lint_rejects(&ws, &tc.files))
                    }
                };
                match rejection {
                    Some(msg) => {
                        match tc
                            .expected_reasons
                            .iter()
                            .find(|w| !msg.contains(w.as_str()))
                        {
                            // Rejected, but the rule that fired is not the rule
                            // the case claims to pin.
                            Some(missing) => ErrorTestResult::Fail(format!(
                                "rejected, but not for the expected reason `{missing}`; got: {}",
                                msg.replace('\n', " | ")
                            )),
                            None => ErrorTestResult::Pass,
                        }
                    }
                    None => ErrorTestResult::Fail(format!(
                        "expected bone or skull-emit to reject ({}), but both accepted",
                        tc.expected_error_kind
                    )),
                }
            };

            match result {
                ErrorTestResult::Pass => {
                    passed += 1;
                    if verbose {
                        println!("{} ... {}", tc.name, "ok".green());
                    }
                }
                ErrorTestResult::Fail(msg) => {
                    failed += 1;
                    failures.push((p.to_owned(), format!("{}: {}", tc.name, msg)));
                    println!("{} ... {}", tc.name, "FAILED".red());
                }
                ErrorTestResult::Todo(msg) => {
                    todos += 1;
                    if verbose {
                        println!("{} ... {} ({})", tc.name, "TODO".yellow(), msg);
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    if failed == 0 {
        println!(
            "Error tests (bone): {}. {} passed; {} failed; {} todo; finished in {:.2}s",
            "ok".green(),
            passed,
            failed,
            todos,
            elapsed
        );
    } else {
        println!(
            "Error tests (bone): {}. {} passed; {} failed; {} todo",
            "FAILED".red(),
            passed,
            failed,
            todos
        );
        for (path, msg) in &failures {
            println!("  {} -- {}", path.display(), msg);
        }
    }

    Ok((total, passed, failed, todos))
}

/// Check if a path contains error tests
pub fn has_error_tests(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    // Check if this is the errors test directory or contains it
    if path.ends_with("errors") || path.join("errors").exists() {
        return true;
    }

    // Check for any error test files
    if path.is_file() {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        return name.ends_with(".rock.err");
    }

    // For directories, check if they contain error test files
    if path.is_dir() {
        for entry in fs::read_dir(path)
            .unwrap_or_else(|_| panic!("Failed to read directory: {:?}", path))
            .flatten()
        {
            if has_error_tests(&entry.path()) {
                return true;
            }
        }
    }

    false
}

/// Find the error test directory from a given path
pub fn find_error_test_dir(path: &Path) -> Option<PathBuf> {
    if path.ends_with("errors") && path.is_dir() {
        return Some(path.to_path_buf());
    }

    // If path is tests/errors/something, return tests/errors
    if let Some(parent) = path.parent() {
        if parent.ends_with("errors") && parent.is_dir() {
            return Some(parent.to_path_buf());
        }
    }

    // If path is tests/, check for tests/errors
    let errors_subdir = path.join("errors");
    if errors_subdir.exists() && errors_subdir.is_dir() {
        return Some(errors_subdir);
    }

    // If this is an error test file, use its parent directory
    if path.is_file() {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".rock.err") {
            return path.parent().map(|p| p.to_path_buf());
        }
    }

    None
}
