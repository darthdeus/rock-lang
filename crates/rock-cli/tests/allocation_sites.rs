use crate::support::{invoke, success, write_files};

#[test]
fn check_reports_allocations_with_locations_reasons_and_site_ids() {
    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[(
            "main.rock",
            "fn values() -> [int] { [1, 2] }\nfn main() {}\n",
        )],
    );
    let report = || {
        success(invoke(
            root.path(),
            &["check", "--alloc-sites", ".", "values"],
        ))
    };
    let output = report();
    let row = output.trim_end();
    let fields: Vec<_> = row.split('\t').collect();
    assert_eq!(fields.len(), 4, "{output}");
    assert!(fields[0].ends_with("main.rock:1"), "{row}");
    assert_eq!(fields[1], "values");
    assert_eq!(fields[2], "constructs a Vec");
    assert_eq!(fields[3].len(), 16);
    assert!(u64::from_str_radix(fields[3], 16).is_ok());
    assert_eq!(report(), output);
}

#[test]
fn published_site_ids_match_the_runtime_manifest() {
    use crate::support::rock;

    let root = tempfile::tempdir().unwrap();
    write_files(
        root.path(),
        &[(
            "main.rock",
            r#"
@inline fn make<T>(item: T) -> [T] { [item] }
fn values() -> [string] { make("hello") }
fn main() { print("{values().len()}") }
"#,
        )],
    );
    let manifest = root.path().join("sites.txt");
    success(
        rock(root.path())
            .args(["compile", ".", "--output", "out.lua"])
            .env("SKULL_ALLOCATION_REPORT", &manifest)
            .output()
            .unwrap(),
    );
    let manifest = std::fs::read_to_string(manifest).unwrap();
    let output = success(invoke(root.path(), &["check", "--alloc-sites", "."]));
    let ids: Vec<_> = output
        .lines()
        .filter(|row| row.contains("constructs a Vec"))
        .filter_map(|row| row.rsplit_once('\t').map(|(_, id)| id))
        .collect();
    assert!(ids.iter().any(|id| manifest.contains(id)), "{output}");
}

#[test]
fn simulator_keeps_site_attribution_as_its_allocation_report() {
    let out = success(invoke(&crate::support::repo_root(), &["sim", "--help"]));
    assert!(out.contains("--allocation-sites"));
    assert!(!out.contains("--allocation-census"));
}
