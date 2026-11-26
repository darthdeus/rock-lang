use bone_query::BoneWorkspace;
use bone_query::FileUriExt;
use lsp_types::{Position, Range};

#[path = "support/failure_log.rs"]
mod failure_log;
use failure_log::Recording;

fn all() -> Range {
    Range::new(Position::new(0, 0), Position::new(100, 0))
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
fn semantic_recovery_reports_real_cause_and_repaired_request_succeeds() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rock").to_string_lossy().into_owned();
    let broken = "struct Clash {}\nstruct Clash {}\nfn main() { let count = 1 }\n";
    let ast = skull_parser::parse_module_clean(broken, ustr::Ustr::from(path.as_str()))
        .expect("semantic fixture must parse cleanly");
    let cause = bone_semantic::semantic_passes::semantic_pass(&mut [ast])
        .expect_err("fixture must refuse the semantic pass")
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
fn batch_recovery_reports_rejected_source() {
    let capture = Recording::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"), "[package]\nname = \"cw7\"\n").unwrap();
    let path = dir.path().join("main.rock");
    let path = bone_query::canonicalize_lenient(&path.to_string_lossy());
    let broken = "struct Clash {}\nstruct Clash {}\n";
    std::fs::write(&path, broken).unwrap();
    let cause = "duplicate definition `Clash` in module `cw7`";
    let mut bone = BoneWorkspace::new();
    bone.load_project(&path);
    let records = capture.take();
    assert!(
        records.iter().any(|record| record.contains("load_project")
            && record.contains(&path)
            && record.contains(cause)),
        "rejected source diagnostics must not be discarded: {records:?}"
    );

    let good = "fn main() { let count = 1 }\n";
    std::fs::write(&path, good).unwrap();
    bone.update(&path, good.into());
    assert!(!bone.inlay_hints(&path, all()).is_empty());
    assert!(capture.take().is_empty());
}
