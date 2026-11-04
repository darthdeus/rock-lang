use bone_query::BoneWorkspace;
use lsp_types::{Position, Range};

#[path = "support/failure_log.rs"]
mod failure_log;
use failure_log::Recording;

fn all() -> Range {
    Range::new(Position::new(0, 0), Position::new(100, 0))
}

#[test]
fn semantic_token_failure_reports_cause_once_and_recovers() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock").to_string_lossy().into_owned();
    let good = "fn main() { let count = 1 }";
    let broken = "fn main() { let text = \"unterminated";
    let cause = rock_tokenizer::Tokenizer::new(broken, ustr::Ustr::from(path.as_str()))
        .tokenize()
        .unwrap_err()
        .message;
    let mut bone = BoneWorkspace::new();
    bone.update(&path, good.into());
    assert!(!bone.semantic_tokens(&path).data.is_empty());
    bone.update(&path, broken.into());
    capture.take();

    assert!(bone.semantic_tokens(&path).data.is_empty());
    let records = capture.take();
    assert_eq!(
        records.len(),
        1,
        "tokenizer failure must be visible: {records:?}"
    );
    assert!(
        records[0].contains("LSP")
            && records[0].contains("semantic tokens")
            && records[0].contains(&path)
            && records[0].contains(&cause)
            && records[0].contains("empty token response"),
        "lost context: {records:?}"
    );
    assert!(bone.semantic_tokens_range(&path, all()).data.is_empty());
    assert!(
        capture.take().is_empty(),
        "full and range requests share failure suppression"
    );

    bone.update(&path, good.into());
    assert!(!bone.semantic_tokens(&path).data.is_empty());
    assert!(capture.take().is_empty());
    bone.update(&path, broken.into());
    capture.take();
    bone.semantic_tokens(&path);
    assert_eq!(capture.take().len(), 1, "repair must release suppression");
}

#[test]
fn workspace_symbol_uri_failure_reports_cause_once_and_keeps_other_files() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    // A relative path is the one spelling a file: URI cannot carry.
    let path = "relative-symbol.rock".to_string();
    let good_path = dir.path().join("good.rock").to_string_lossy().into_owned();
    let cause = lsp_types::Uri::from_file_path(&path)
        .unwrap_err()
        .to_string();
    let mut bone = BoneWorkspace::new();
    bone.update(&path, "fn omitted_symbol() {}".into());
    bone.update(&good_path, "fn retained_symbol() {}".into());
    capture.take();

    let symbols = bone.workspace_symbols("_symbol");
    assert!(symbols.iter().any(|s| s.name == "retained_symbol"));
    assert!(symbols.iter().all(|s| s.name != "omitted_symbol"));
    let records = capture.take();
    assert_eq!(
        records.len(),
        1,
        "invalid symbol URI must be visible: {records:?}"
    );
    assert!(
        records[0].contains("workspace symbols")
            && records[0].contains(&path)
            && records[0].contains(&cause)
            && records[0].contains("omit symbols from this file"),
        "lost context: {records:?}"
    );
    bone.workspace_symbols("_symbol");
    assert!(capture.take().is_empty());
    bone.remove(&path);
    assert!(
        bone.workspace_symbols("_symbol")
            .iter()
            .any(|s| s.name == "retained_symbol")
    );
    assert!(capture.take().is_empty());
}

#[test]
fn ordinary_empty_queries_do_not_report_failures() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock").to_string_lossy().into_owned();
    let mut bone = BoneWorkspace::new();
    bone.update(&path, "\nfn main() {}".into());
    capture.take();
    assert!(bone.hover(&path, Position::new(0, 0)).is_none());
    assert!(bone.workspace_symbols("absent_symbol").is_empty());
    assert!(bone.inlay_hints(&path, all()).is_empty());
    assert!(capture.take().is_empty());
}

#[test]
fn parse_fallback_reports_cause_once_and_recovers_after_repair() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock").to_string_lossy().into_owned();
    let good = "fn main() {\n let count = 1\n}\n";
    let broken = "fn main() {\n let count = 1\n let text = \"unterminated\n}\n";
    let cause = skull_parser::parse_module(broken, ustr::Ustr::from(path.as_str()))
        .err()
        .expect("fixture must force a hard parse failure")
        .message;
    let mut bone = BoneWorkspace::new();
    bone.update(&path, good.into());
    assert!(!bone.inlay_hints(&path, all()).is_empty());
    capture.take();

    bone.update(&path, broken.into());
    capture.take();
    let partial = bone.inlay_hints(&path, all());
    assert!(
        !partial.is_empty(),
        "last good inference still supplies partial hints"
    );
    let records = capture.take();
    assert!(
        records.iter().any(|record| {
            record.contains("inlay")
                && record.contains(&path)
                && record.contains(&cause)
                && record.contains("receiver")
        }),
        "missing parse cause and intentional fallback: {records:?}"
    );

    bone.inlay_hints(&path, all());
    bone.update(&path, broken.into());
    bone.inlay_hints(&path, all());
    assert!(
        capture.take().is_empty(),
        "unchanged failure must not repeat logs"
    );

    bone.update(&path, good.into());
    assert!(
        bone.diagnostics_for(&path)
            .iter()
            .all(|d| { d.severity != Some(lsp_types::DiagnosticSeverity::ERROR) })
    );
    assert!(!bone.inlay_hints(&path, all()).is_empty());
    assert!(capture.take().is_empty());

    bone.update(&path, broken.into());
    bone.inlay_hints(&path, all());
    assert!(
        capture.take().iter().any(|record| record.contains(&cause)),
        "a new failure after repair must be reported again"
    );
}

#[test]
fn semantic_recovery_reports_real_cause_and_repaired_request_succeeds() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock").to_string_lossy().into_owned();
    let broken = "struct Clash {}\nstruct Clash {}\nfn main() { let count = 1 }\n";
    let ast = skull_parser::parse_module_clean(broken, ustr::Ustr::from(path.as_str()))
        .expect("semantic fixture must parse cleanly");
    let cause = bone_semantic::semantic_passes::semantic_pass(&mut [ast])
        .err()
        .expect("fixture must refuse the semantic pass")
        .message;
    let mut bone = BoneWorkspace::new();
    bone.update(&path, broken.into());
    capture.take();
    bone.inlay_hints(&path, all());
    let records = capture.take();
    assert!(
        records.iter().any(|record| {
            record.contains("inlay") && record.contains(&path) && record.contains(&cause)
        }),
        "recovery must retain its semantic failure cause: {records:?}"
    );
    bone.inlay_hints(&path, all());
    assert!(capture.take().is_empty());

    bone.update(&path, "fn main() { let count = 1 }\n".into());
    assert!(!bone.inlay_hints(&path, all()).is_empty());
    assert!(capture.take().is_empty());
}

#[test]
fn recoverable_syntax_keeps_partial_hints_without_fallback_warnings() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock").to_string_lossy().into_owned();
    let src = "fn main() {\n let count = 1\n}\nfn unfinished(\n";
    let ast = skull_parser::parse_module(src, ustr::Ustr::from(path.as_str())).unwrap();
    assert!(
        !ast.recovery_errors.is_empty(),
        "fixture must use parser recovery"
    );
    let mut bone = BoneWorkspace::new();
    bone.update(&path, src.into());
    assert!(!bone.inlay_hints(&path, all()).is_empty());
    assert!(
        capture.take().is_empty(),
        "syntax diagnostics already report ordinary typing errors"
    );
}

#[test]
fn log_capture_is_released_on_unwind() {
    let failed = std::panic::catch_unwind(|| {
        let _capture = Recording::start();
        panic!("intentional cleanup witness");
    });
    assert!(failed.is_err());
    let capture = Recording::start();
    log::warn!("capture restored");
    assert_eq!(capture.take().len(), 1);
}

#[test]
fn batch_fallback_reports_failed_per_file_update() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"cw7\"\n").unwrap();
    let path = dir.path().join("main.rock");
    let path = bone_query::canonicalize_lenient(&path.to_string_lossy());
    let broken = "struct Clash {}\nstruct Clash {}\n";
    std::fs::write(&path, broken).unwrap();
    let ast = skull_parser::parse_module_clean(broken, ustr::Ustr::from(path.as_str())).unwrap();
    let cause = bone_semantic::semantic_passes::semantic_pass(&mut [ast])
        .err()
        .expect("semantic failure fixture")
        .message;
    let mut bone = BoneWorkspace::new();
    bone.load_project(&path);
    let records = capture.take();
    assert!(
        records.iter().any(|record| record.contains("load_project")
            && record.contains(&path)
            && record.contains(&cause)),
        "per-file indexing diagnostics must not be discarded: {records:?}"
    );

    let good = "fn main() { let count = 1 }\n";
    std::fs::write(&path, good).unwrap();
    bone.update(&path, good.into());
    assert!(!bone.inlay_hints(&path, all()).is_empty());
    assert!(capture.take().is_empty());
}
