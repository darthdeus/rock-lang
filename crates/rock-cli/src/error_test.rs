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
    Todo,
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

        // Look for test case start: //=== TEST CASE: name_ExpectedErrorKind ===
        if line.starts_with("//=== TEST CASE:") && line.ends_with(" ===") {
            let case_header = line
                .strip_prefix("//=== TEST CASE:")
                .unwrap()
                .strip_suffix(" ===")
                .unwrap()
                .trim();

            let (case_name, expected_error_kind) = parse_test_case_name(case_header)?;

            // Collect the input code until we find the next test case or end of file
            i += 1;
            let mut input_lines = Vec::new();

            while i < lines.len() {
                let current_line = lines[i];

                // The next case header, or any other `//===` delimiter.
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

/// Split `simple_undefined_UndefinedSymbol` into the case name and the
/// expected error kind after its last underscore.
fn parse_test_case_name(case_header: &str) -> Result<(String, String)> {
    match case_header.rsplit_once('_') {
        Some((case_name, error_kind))
            if !error_kind.is_empty() && error_kind.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            Ok((case_name.to_string(), error_kind.to_string()))
        }
        _ => Err(anyhow!("Invalid test case name format: {}", case_header)),
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
        // Reject obsolete or unknown module directives before parsing.
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
            Ok(mut m) => {
                if files.len() > 1 {
                    m.package = Some(skull_parser::PackageTag {
                        id: 1,
                        self_name: "error_test".to_string(),
                        identity_storage: Default::default(),
                        root: String::new(),
                    });
                }
                user.push(m);
            }
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
    bone_lints::lint_workspace(
        ws,
        &warm,
        true,
        &sources,
        &bone_lints::LintOptions::default(),
    )
    .iter()
    .find(|f| f.level() == rock_core::errors::CompilerErrorLevel::Error)
    .map(|f| error_text(std::slice::from_ref(&f.error)))
}

/// bone, then skull emission, then an Error-severity lint: any of the three
/// may be the rejecting stage.
fn run_case(tc: &ErrorTestCase) -> ErrorTestResult {
    let rejection = match bone_check_case(&tc.files) {
        Err(msg) => Some(msg),
        Ok(ws) => skull_emit_rejects(&ws, &tc.files).or_else(|| lint_rejects(&ws, &tc.files)),
    };
    let Some(msg) = rejection else {
        return ErrorTestResult::Fail(format!(
            "expected bone or skull-emit to reject ({}), but both accepted",
            tc.expected_error_kind
        ));
    };
    match tc
        .expected_reasons
        .iter()
        .find(|w| !msg.contains(w.as_str()))
    {
        // Rejected, but the rule that fired is not the rule the case claims
        // to pin.
        Some(missing) => ErrorTestResult::Fail(format!(
            "rejected, but not for the expected reason `{missing}`; got: {}",
            msg.replace('\n', " | ")
        )),
        None => ErrorTestResult::Pass,
    }
}

/// Run the error-test suite through bone's front end (the skull/bone `test`
/// path). `backend` is informational -- bone is the shared front end, so both
/// verify rejection identically. The error-KIND suffix is rock-specific (bone
/// diagnostics mostly carry no kind), so this asserts REJECTION only.
pub(crate) fn run_error_suite(path: &Path, verbose: bool) -> Result<usize> {
    let Some(dir) = find_error_test_dir(path) else {
        return Ok(0);
    };
    println!(
        "\nRunning error tests in {} (bone front end)...",
        dir.display()
    );

    let start = Instant::now();
    let (mut passed, mut failed, mut todos) = (0usize, 0usize, 0usize);
    let mut failures: Vec<(PathBuf, String)> = Vec::new();

    // Fixture files in walk order; a file that does not parse is a failure.
    let mut fixtures: Vec<(PathBuf, Result<ErrorTest>)> = Vec::new();
    for entry in WalkDir::new(&dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if is_error_test_file(p) {
            fixtures.push((p.to_owned(), ErrorTest::from_file(p)));
        }
    }
    let cases: Vec<(&ErrorTestCase, bool)> = fixtures
        .iter()
        .filter_map(|(_, test)| test.as_ref().ok())
        .flat_map(|test| test.test_cases.iter().map(|tc| (tc, test.is_todo)))
        .collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut results = crate::parallel_runner::map_in_order(&cases, threads, |(tc, is_todo)| {
        if *is_todo {
            ErrorTestResult::Todo
        } else {
            run_case(tc)
        }
    })
    .into_iter();

    for (p, error_test) in &fixtures {
        let error_test = match error_test {
            Ok(t) => t,
            Err(e) => {
                failures.push((p.to_owned(), format!("Failed to parse test file: {}", e)));
                failed += 1;
                continue;
            }
        };
        for tc in &error_test.test_cases {
            match results.next().expect("one result per case") {
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
                ErrorTestResult::Todo => {
                    todos += 1;
                    if verbose {
                        println!("{} ... {} (TODO)", tc.name, "TODO".yellow());
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

    Ok(failed)
}

fn is_error_test_file(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.ends_with(".rock.err"))
}

fn contains_error_tests(dir: &Path) -> bool {
    WalkDir::new(dir)
        .into_iter()
        .flatten()
        .any(|e| is_error_test_file(e.path()))
}

/// The `.rock.err` directory `path` selects: an `errors` directory, the
/// `errors` directory of a fixture or fixture subdirectory inside it, a
/// directory's `errors` subdirectory, or a fixture's own directory.
fn find_error_test_dir(path: &Path) -> Option<PathBuf> {
    if path.ends_with("errors") && path.is_dir() {
        return Some(path.to_path_buf());
    }
    if let Some(parent) = path
        .parent()
        .filter(|p| p.ends_with("errors") && p.is_dir())
    {
        let selects_fixtures = is_error_test_file(path)
            || path.join("errors").exists()
            || (path.is_dir() && contains_error_tests(path));
        return selects_fixtures.then(|| parent.to_path_buf());
    }
    let errors_subdir = path.join("errors");
    if errors_subdir.is_dir() {
        return Some(errors_subdir);
    }
    if is_error_test_file(path) {
        return path.parent().map(|p| p.to_path_buf());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_case_name_ends_in_a_bare_error_kind() {
        assert_eq!(
            parse_test_case_name("simple_undefined_UndefinedSymbol").unwrap(),
            (
                "simple_undefined".to_string(),
                "UndefinedSymbol".to_string()
            )
        );
        let error = parse_test_case_name("simple_undefined_UndefinedSymbol{name=\"f\"}")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Invalid test case name format"), "{error}");
    }
}
