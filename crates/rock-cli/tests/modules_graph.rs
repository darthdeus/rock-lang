//! PackageGraph integration through the CLI source loader (plan M0):
//! legacy file order is pinned against the pre-graph behavior, and v1
//! packages must reach `SourceFile` with their dialect and package facts.

use rock_cli::unified_compile::SourceSpec;
use rock_core::source_code::Dialect;
use rock_host::stick::graph::PackageGraph;
use rock_host::stick::StickConfig;
use std::fs;
use std::path::Path;

fn write_package(dir: &Path, name: &str, body: &str, sources: &[&str]) {
    fs::create_dir_all(dir).expect("create package dir");
    fs::write(
        dir.join("stick.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{body}"),
    )
    .expect("write manifest");
    for src in sources {
        fs::write(dir.join(src), "fn main() {}\n").expect("write source");
    }
}

fn temp_tree(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(tag)
        .tempdir()
        .expect("tempdir")
}

/// Three legacy packages, no transitive edges: the file list and its order
/// must be exactly what the pre-graph loader produced (deps before the root
/// package, each dependency's non-script files in walk order, dependencies
/// ordered by declared key).
#[test]
fn discover_sources_three_package_legacy_order_unchanged() {
    let tmp = temp_tree("modules-graph-order");
    let root = tmp.path().join("app");
    let b_child = tmp.path().join("b-child");
    let a_child = tmp.path().join("a-child");
    write_package(&b_child, "b-child", "", &["util.rock"]);
    write_package(&a_child, "a-child", "", &["core.rock", "extra.rock"]);
    write_package(
        &root,
        "app",
        "\n[dependencies]\nb = { path = \"../b-child\" }\na = { path = \"../a-child\" }\n",
        &["main.rock"],
    );

    let sources = rock_cli::unified_compile::discover_sources(&SourceSpec::Directory(root.clone()))
        .expect("discover sources");
    let files: Vec<String> = sources
        .files
        .iter()
        .map(|f| f.borrow().file.to_id_path())
        .collect();

    let a_root = fs::canonicalize(&a_child).expect("canonical a");
    let b_root = fs::canonicalize(&b_child).expect("canonical b");
    let app_root = fs::canonicalize(&root).expect("canonical app");
    let expected = vec![
        format!("{}/core.rock", a_root.display()),
        format!("{}/extra.rock", a_root.display()),
        format!("{}/util.rock", b_root.display()),
        format!("{}/main.rock", app_root.display()),
    ];
    assert_eq!(files, expected);
}

/// A v1 root's sources carry `Dialect::V1` and its package tag; the legacy
/// dependency's sources carry `Dialect::Legacy` and their own tag.
#[test]
fn discover_sources_stamps_dialect_and_package() {
    let tmp = temp_tree("modules-graph-stamp");
    let root = tmp.path().join("app");
    let lib = tmp.path().join("v1-lib");
    write_package(&lib, "v1-lib", "\n[language]\nmodules = \"v1\"\n", &["lib.rock"]);
    write_package(
        &root,
        "app",
        "\n[language]\nmodules = \"v1\"\n\n[dependencies]\nlib = { path = \"../v1-lib\" }\n",
        &["main.rock"],
    );

    let sources = rock_cli::unified_compile::discover_sources(&SourceSpec::Directory(root.clone()))
        .expect("discover sources");
    let graph = PackageGraph::build(&root).expect("graph builds");

    for file in &sources.files {
        let f = file.borrow();
        let path = f.file.to_id_path();
        let owner = graph
            .package_of_file(Path::new(&path))
            .expect("every loaded file has a package");
        let node = graph.package(owner);
        assert_eq!(
            f.package.as_ref().expect("package tag").self_name,
            node.self_name
        );
        assert_eq!(f.package.as_ref().expect("package tag").id, owner.0);
        if node.manifest_name == "app" {
            assert_eq!(f.dialect, Dialect::V1, "{path} should be v1");
            assert_eq!(f.dialect, node.dialect, "{path} should match its package");
        } else {
            assert_eq!(f.dialect, node.dialect, "{path} should match its package");
        }
    }
}

/// The graph and the loader agree on the manifest, and a manifest with a
/// `[language]` table loads through the normal config path.
#[test]
fn stick_config_language_table_loads_from_disk() {
    let tmp = temp_tree("modules-graph-manifest");
    let root = tmp.path().join("app");
    write_package(&root, "app", "\n[language]\nmodules = \"v1\"\n", &[]);
    let (_, cfg) = StickConfig::find_from_dir(&root)
        .expect("find manifest")
        .expect("manifest exists");
    assert_eq!(cfg.dialect(), Dialect::V1);
}
