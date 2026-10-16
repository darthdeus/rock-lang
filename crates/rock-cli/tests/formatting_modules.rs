use std::path::{Path};

fn run_fmt(cwd: &Path, args: &[&str]) -> std::process::Output {
    crate::command::rock_bin()
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn rock fmt")
}

fn manifest(name: &str, dialect: Option<&str>) -> String {
    let language = dialect
        .map(|value| format!("\n[language]\nmodules = \"{value}\"\n"))
        .unwrap_or_default();
    format!(
        "[package]\nname = \"{name}\"\ntype = \"bin\"\n{language}"
    )
}

#[test]
fn fmt_formats_v1_files_from_the_manifest_and_checks_without_writing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let v1 = dir.path().join("v1");
    std::fs::create_dir_all(&v1).expect("v1 dir");
    std::fs::write(v1.join("stick.toml"), manifest("fmt_v1", Some("v1")))
        .expect("manifest");
    let source = "// header\nuse   std::print; // import\n\npub   fn   main( )->() {\n    print(\"ok\")\n} // tail\n";
    let file = v1.join("main.rock");
    std::fs::write(&file, source).expect("source");

    let first = run_fmt(dir.path(), &["fmt", file.to_str().unwrap()]);
    assert!(
        first.status.success(),
        "v1 fmt failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let formatted = std::fs::read_to_string(&file).expect("formatted source");
    assert_ne!(
        formatted,
        source,
        "v1 fmt must visibly change unformatted input\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(formatted.contains("// header"), "header comment was lost: {formatted}");
    assert!(formatted.contains("use   std::print;"), "import was lost: {formatted}");
    assert!(formatted.contains("// import"), "trailing comment was lost: {formatted}");
    assert!(formatted.contains("// tail"), "tail comment was lost: {formatted}");
    skull_parser::parse_module_with_dialect(
        &formatted,
        ustr::Ustr::from("formatted.rock"),
        rock_core::source_code::Dialect::V1,
    )
    .expect("formatted source must parse as v1");

    let second = run_fmt(dir.path(), &["fmt", file.to_str().unwrap()]);
    assert!(second.status.success(), "second fmt failed");
    assert_eq!(
        std::fs::read_to_string(&file).expect("second formatted source"),
        formatted,
        "v1 formatting must be idempotent"
    );

    std::fs::write(&file, source).expect("restore unformatted source");
    let before_check = std::fs::read(&file).expect("source before check");
    let check = run_fmt(
        dir.path(),
        &["fmt", "--check", file.to_str().unwrap()],
    );
    assert!(!check.status.success(), "--check must reject unformatted v1 input");
    assert_eq!(
        std::fs::read(&file).expect("source after check"),
        before_check,
        "--check must not write the source"
    );
}

#[test]
fn fmt_directory_selects_each_package_file_dialect_and_reports_invalid_v1() {
    let dir = tempfile::tempdir().expect("tempdir");
    let v1 = dir.path().join("v1");
    let legacy = dir.path().join("legacy");
    std::fs::create_dir_all(&v1).expect("v1 dir");
    std::fs::create_dir_all(&legacy).expect("legacy dir");
    std::fs::write(v1.join("stick.toml"), manifest("fmt_v1_dir", Some("v1")))
        .expect("v1 manifest");
    std::fs::write(legacy.join("stick.toml"), manifest("fmt_legacy_dir", None))
        .expect("legacy manifest");

    let v1_file = v1.join("main.rock");
    let legacy_file = legacy.join("main.rock");
    std::fs::write(&v1_file, "pub   fn   main( )->(){ }\n").expect("v1 source");
    std::fs::write(&legacy_file, "fn   main( ){ }\n").expect("legacy source");

    let out = run_fmt(dir.path(), &["fmt", dir.path().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "mixed fmt failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(
        std::fs::read_to_string(&v1_file).unwrap(),
        "pub   fn   main( )->(){ }\n",
        "directory formatting must use the v1 package dialect"
    );
    assert_ne!(
        std::fs::read_to_string(&legacy_file).unwrap(),
        "fn   main( ){ }\n",
        "directory formatting must retain the legacy path"
    );

    let invalid = v1.join("invalid.rock");
    let invalid_source = "pub fn broken( {\n";
    std::fs::write(&invalid, invalid_source).expect("invalid source");
    let before = std::fs::read(&invalid).expect("invalid bytes");
    let failed = run_fmt(dir.path(), &["fmt", invalid.to_str().unwrap()]);
    assert!(!failed.status.success(), "invalid v1 formatting must fail");
    assert_eq!(
        std::fs::read(&invalid).expect("invalid bytes after fmt"),
        before,
        "a failed formatting operation must not corrupt input"
    );
}
