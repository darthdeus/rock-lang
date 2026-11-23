use anyhow::{Result, anyhow};
use bone_core::prelude::Instant;
use colored::Colorize;
use fs_err as fs;
use rock_core::source_code::STANDALONE_PACKAGE_PREFIX;
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

        let test_cases = parse_error_test_cases(&content)?;

        Ok(ErrorTest {
            test_cases,
            is_todo,
        })
    }
}

fn parse_error_test_cases(content: &str) -> Result<Vec<ErrorTestCase>> {
    let mut test_cases = Vec::new();
    let mut prologues = rustc_hash::FxHashMap::default();
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(name) = line
            .trim()
            .strip_prefix("//=== PROLOGUE: ")
            .and_then(|header| header.strip_suffix(" ==="))
        {
            let mut source = String::new();
            while let Some(line) = lines.next_if(|line| !line.trim_start().starts_with("//===")) {
                source.push_str(line);
                source.push('\n');
            }
            if name.is_empty()
                || source.trim().is_empty()
                || prologues.insert(name, source).is_some()
            {
                return Err(anyhow!("Invalid or duplicate fixture prologue: {name}"));
            }
            continue;
        }
        let Some(header) = line
            .trim()
            .strip_prefix("//=== TEST CASE:")
            .and_then(|header| header.strip_suffix(" ==="))
        else {
            continue;
        };
        let (name, expected_error_kind) = parse_test_case_name(header.trim())?;
        let mut expected_reasons = Vec::new();
        let mut files = Vec::new();
        let mut file = None;
        let mut code = String::new();
        while let Some(line) = lines.next_if(|line| !line.trim_start().starts_with("//===")) {
            let trimmed = line.trim();
            if let Some(want) = trimmed.strip_prefix("//! expect:") {
                expected_reasons.push(want.trim().to_owned());
            } else if let Some(next) = trimmed
                .strip_prefix("//--- FILE:")
                .and_then(|header| header.strip_suffix("---"))
            {
                if let Some(file) = file.replace(next.trim()) {
                    files.push((file.to_owned(), code.trim().to_owned()));
                }
                // Once explicit files are present, text before the first header
                // belongs to the fixture, not to any source file.
                code.clear();
            } else if let Some(name) = trimmed
                .strip_prefix("//--- USE: ")
                .and_then(|header| header.strip_suffix(" ---"))
            {
                let source = prologues
                    .get(name)
                    .ok_or_else(|| anyhow!("Unknown fixture prologue: {name}"))?;
                code.push_str(source);
            } else {
                code.push_str(line);
                code.push('\n');
            }
        }
        files.push((
            file.unwrap_or("test.rock").to_owned(),
            code.trim().to_owned(),
        ));
        test_cases.push(ErrorTestCase {
            name,
            files,
            expected_error_kind,
            expected_reasons,
        });
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

fn bone_check_case(
    files: &[(String, String)],
) -> Result<bone_semantic::inference::Workspace, String> {
    let mut user = Vec::with_capacity(files.len());
    for (name, code) in files {
        match skull_parser::parse_module(code, ustr::Ustr::from(name.as_str())) {
            Ok(mut m) => {
                if files.len() > 1 {
                    m.package = Some(skull_parser::PackageTag {
                        id: 1,
                        self_name: format!("{STANDALONE_PACKAGE_PREFIX}error-test"),
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
    // so the rejection is the downstream gate in `check_test_modules`
    // (`skull::pipeline::syntax_errors`), not the parse `match` above.
    skull::pipeline::with_stdlib(user)
        .map_err(|error| vec![error])
        .and_then(crate::test_unit::check_test_modules)
        .map_err(|errs| error_text(&errs))
}

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

/// A Bone-accepted case can still fail during Skull emission.
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

/// Error-severity build lints also reject programs the type checker accepted.
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
    let Some(dir) = find_error_test_dir(path)? else {
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
    for entry in WalkDir::new(&dir).follow_links(true) {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type().is_file() && has_error_fixture_suffix(p) {
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

fn has_error_fixture_suffix(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.ends_with(".rock.err"))
}

fn optional_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn is_dir(path: &Path) -> Result<bool> {
    Ok(optional_metadata(path)?.is_some_and(|metadata| metadata.is_dir()))
}

fn is_error_test_file(path: &Path) -> Result<bool> {
    Ok(has_error_fixture_suffix(path)
        && optional_metadata(path)?.is_some_and(|metadata| metadata.is_file()))
}

fn contains_error_tests(dir: &Path) -> Result<bool> {
    for entry in WalkDir::new(dir) {
        let entry = entry?;
        if is_error_test_file(entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn find_error_test_dir(path: &Path) -> Result<Option<PathBuf>> {
    if path.ends_with("errors") && is_dir(path)? {
        return Ok(Some(path.to_path_buf()));
    }
    if let Some(parent) = path.parent().filter(|p| p.ends_with("errors"))
        && is_dir(parent)?
    {
        let selects_fixtures = is_error_test_file(path)?
            || optional_metadata(&path.join("errors"))?.is_some()
            || (is_dir(path)? && contains_error_tests(path)?);
        return Ok(selects_fixtures.then(|| parent.to_path_buf()));
    }
    let errors_subdir = path.join("errors");
    if is_dir(&errors_subdir)? {
        return Ok(Some(errors_subdir));
    }
    Ok(if is_error_test_file(path)? {
        path.parent().map(Path::to_path_buf)
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn fixture_walk_reports_a_symlink_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let errors = tmp.path().join("errors");
        fs::create_dir(&errors).unwrap();
        let fixture = errors.join("loop.rock.err");
        std::os::unix::fs::symlink(&fixture, &fixture).unwrap();

        let error = run_error_suite(&errors, false).unwrap_err();
        assert!(format!("{error:#}").contains("loop.rock.err"), "{error:#}");
    }

    #[test]
    fn fixture_sections_keep_explicit_files_and_diagnostic_expectations() {
        let cases = parse_error_test_cases(
            "//=== TEST CASE: named_UndefinedSymbol ===
discarded preamble
//! expect: missing name
//--- FILE: first.rock ---
fn first() { missing() }
//--- FILE: second.rock ---
fn second() { first() }
//=== NOTES ===
discarded notes
//=== TEST CASE: implicit_UndefinedSymbol ===
fn implicit() { missing() }",
        )
        .unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].expected_reasons, ["missing name"]);
        assert_eq!(
            cases[0].files,
            [
                ("first.rock".into(), "fn first() { missing() }".into()),
                ("second.rock".into(), "fn second() { first() }".into()),
            ]
        );
        assert_eq!(
            cases[1].files,
            [("test.rock".into(), "fn implicit() { missing() }".into())]
        );
    }

    #[test]
    fn fixture_prologues_preserve_whitespace_and_require_a_definition() {
        let source = "//=== PROLOGUE: shared ===
fn setup() {
    marker()
}
//=== TEST CASE: implicit_UndefinedSymbol ===
//--- USE: shared ---

fn main() { setup() }";
        let cases = parse_error_test_cases(source).unwrap();
        assert_eq!(
            cases[0].files,
            [(
                "test.rock".into(),
                "fn setup() {\n    marker()\n}\n\nfn main() { setup() }".into()
            )]
        );
        let unknown = source.replace("USE: shared", "USE: missing");
        let error = parse_error_test_cases(&unknown).unwrap_err();
        assert_eq!(error.to_string(), "Unknown fixture prologue: missing");
    }

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
