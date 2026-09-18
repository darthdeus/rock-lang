//! The LSP over a modules-v1 package (plan M5, spec 15.3): a file entering the
//! session after the batch load keeps its manifest facts, and go-to-definition
//! follows what the resolver recorded -- module paths, aliases, globs and
//! re-exports -- to the defining item.

use bone_query::BoneWorkspace;
use lsp_types::Position;

/// A v1 `bin` package `app` on disk. The root fragment reaches the child
/// module `shapes` by a qualified path, an aliased import, a glob and a
/// re-export through `facade`.
pub struct V1Project {
    pub dir: tempfile::TempDir,
}

pub const MAIN: &str = "\
use shapes::area as surface;
use shapes::*;

fn main() {
    let a = shapes::inner::depth()
    let b = surface(2)
    let c = perimeter(3)
    let d = facade::depth()
    print(\"{a} {b} {c} {d}\")
}
";

pub const SHAPES: &str = "\
pub mod shapes;

pub fn area(w: int) -> int {
    w * w
}

pub fn perimeter(w: int) -> int {
    w * 4
}
";

pub const INNER: &str = "\
pub mod shapes::inner;

pub fn depth() -> int {
    3
}
";

pub const FACADE: &str = "\
pub mod facade;

pub use super::shapes::inner::depth;
";

impl V1Project {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
        )
        .unwrap();
        for (name, text) in [
            ("main.rock", MAIN),
            ("shapes.rock", SHAPES),
            ("shapes_inner.rock", INNER),
            ("facade.rock", FACADE),
        ] {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        Self { dir }
    }

    pub fn path(&self, name: &str) -> String {
        bone_query::canonicalize_lenient(&self.dir.path().join(name).to_string_lossy())
    }

    /// A workspace with the package loaded through the editor route
    /// (`update` on the root fragment, as didOpen does), asserting the
    /// package builds clean -- a test below that sees a diagnostic reads a
    /// real regression, not a broken fixture.
    pub fn open(&self) -> BoneWorkspace {
        let mut bone = BoneWorkspace::new();
        let main = self.path("main.rock");
        let diags = bone.update(&main, MAIN.to_string());
        let errors: Vec<_> = diags.iter().flat_map(|(p, d)| d.iter().map(move |d| (p, d))).collect();
        assert!(
            errors.iter().all(|(_, d)| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
            "the fixture package must build clean: {errors:#?}"
        );
        bone
    }
}

/// The LSP position of the `nth` (0-based) occurrence of `needle` in `text`,
/// `into` characters into it.
pub fn pos_of(text: &str, needle: &str, nth: usize, into: usize) -> Position {
    let offset = text
        .match_indices(needle)
        .nth(nth)
        .unwrap_or_else(|| panic!("`{needle}` #{nth} not in text"))
        .0
        + into;
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let col = before.rsplit('\n').next().unwrap().chars().count() as u32;
    Position::new(line, col)
}

/// The text a location's range covers, read from disk.
fn located_text(loc: &lsp_types::Location) -> (String, String) {
    let path = loc.uri.as_str().strip_prefix("file://").unwrap().to_string();
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.split('\n').collect();
    let r = loc.range;
    let covered = if r.start.line == r.end.line {
        lines[r.start.line as usize]
            .chars()
            .skip(r.start.character as usize)
            .take((r.end.character - r.start.character) as usize)
            .collect()
    } else {
        lines[r.start.line as usize].chars().skip(r.start.character as usize).collect()
    };
    (path, covered)
}

fn file_name(path: &str) -> &str {
    std::path::Path::new(path).file_name().unwrap().to_str().unwrap()
}

fn errors_in(diags: &[(String, Vec<lsp_types::Diagnostic>)], path: &str) -> Vec<String> {
    diags
        .iter()
        .filter(|(p, _)| p == path)
        .flat_map(|(_, d)| d.iter())
        .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
        .map(|d| d.message.clone())
        .collect()
}

// -- a file entering the session after the batch load -------------------------

/// Opening a file the batch load did not see (created after the project was
/// indexed) must read it as its package's dialect, with its package, exactly
/// as the batch load reads its siblings. It used to enter through
/// `Session::update_file` with no facts and parse as legacy and unowned.
#[test]
fn a_file_created_after_the_load_is_read_as_its_package_reads_it() {
    let project = V1Project::new();
    let mut bone = project.open();
    let extra_text = "pub mod extra;\n\npub fn more() -> int {\n    super::shapes::area(2)\n}\n";
    std::fs::write(project.dir.path().join("extra.rock"), extra_text).unwrap();
    let extra = project.path("extra.rock");
    let diags = bone.update(&extra, extra_text.to_string());
    assert_eq!(errors_in(&diags, &extra), Vec::<String>::new(), "a v1 file of a v1 package must build as v1");

    let loc = bone
        .definition(&extra, pos_of(extra_text, "area", 0, 1))
        .expect("a call in the new fragment resolves");
    let (path, covered) = located_text(&loc);
    assert_eq!(file_name(&path), "shapes.rock");
    assert!(covered.contains("area"), "landed on {covered:?}");
}

// -- go-to-definition ---------------------------------------------------------

fn definition_in_main(bone: &BoneWorkspace, project: &V1Project, needle: &str, nth: usize, into: usize) -> (String, String) {
    let main = project.path("main.rock");
    let loc = bone
        .definition(&main, pos_of(MAIN, needle, nth, into))
        .unwrap_or_else(|| panic!("no definition for `{needle}` #{nth}"));
    let (path, covered) = located_text(&loc);
    (file_name(&path).to_string(), covered)
}

#[test]
fn definition_of_a_qualified_path_lands_on_the_function() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "depth()", 0, 1);
    assert_eq!(file, "shapes_inner.rock");
    assert!(covered.starts_with("pub fn depth") || covered == "depth", "landed on {covered:?}");
}

#[test]
fn definition_of_an_aliased_import_lands_on_the_original_function() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "surface(2)", 0, 2);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("area"), "landed on {covered:?}");
}

#[test]
fn definition_of_a_glob_imported_name_lands_on_the_function() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "perimeter(3)", 0, 2);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("perimeter"), "landed on {covered:?}");
}

#[test]
fn definition_of_a_re_exported_name_lands_on_the_defining_function() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "depth()", 1, 1);
    assert_eq!(file, "shapes_inner.rock", "a `pub use` is not the definition");
    assert!(covered.contains("depth"), "landed on {covered:?}");
}

// -- the std prelude ----------------------------------------------------------

/// A v1 fragment sees the std prelude (plan 2.5) in the editor as it does in
/// a compile: `print` and `*` resolve.
#[test]
fn the_std_prelude_resolves_in_a_v1_file() {
    let project = V1Project::new();
    let mut bone = BoneWorkspace::new();
    let shapes = project.path("shapes.rock");
    let diags = bone.update(&shapes, SHAPES.to_string());
    assert_eq!(errors_in(&diags, &shapes), Vec::<String>::new());
    assert_eq!(errors_in(&diags, &project.path("main.rock")), Vec::<String>::new());
}
