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
use shapes::Box as Crate;

fn main() {
    let a = shapes::inner::depth()
    let b = surface(2)
    let c = perimeter(3)
    let d = facade::depth()
    print(\"{a} {b} {c} {d}\")
}

fn sizes(b: shapes::Box, c: Crate) -> int {
    let k = shapes::Kind::Round
    b.w + c.w + shapes::LIMIT
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

pub struct Box {
    pub w: int,
}

pub enum Kind {
    Round,
    Square,
}

pub global LIMIT: int = 10

fn hidden_helper() -> int {
    1
}
";

pub const INNER: &str = "\
pub mod shapes::inner;

pub fn depth() -> int {
    3
}

fn inner_secret() -> int {
    4
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
    assert_eq!(covered, "depth");
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

// -- go-to-definition on each path segment ------------------------------------

/// The line a definition landed on, with its file.
fn definition_line(bone: &BoneWorkspace, path: &str, text: &str, needle: &str, nth: usize, into: usize) -> (String, u32) {
    let loc = bone
        .definition(path, pos_of(text, needle, nth, into))
        .unwrap_or_else(|| panic!("no definition for `{needle}` #{nth}"));
    let (file, _) = located_text(&loc);
    (file_name(&file).to_string(), loc.range.start.line)
}

#[test]
fn a_module_segment_lands_on_the_module_header() {
    let project = V1Project::new();
    let bone = project.open();
    let main = project.path("main.rock");
    assert_eq!(definition_line(&bone, &main, MAIN, "shapes::inner::depth", 0, 1), ("shapes.rock".into(), 0));
    assert_eq!(definition_line(&bone, &main, MAIN, "inner::depth", 0, 1), ("shapes_inner.rock".into(), 0));
    assert_eq!(definition_line(&bone, &main, MAIN, "facade::depth", 0, 1), ("facade.rock".into(), 0));
}

#[test]
fn a_use_item_segment_lands_on_what_it_names() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "area as surface", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("area"), "landed on {covered:?}");
    assert_eq!(
        definition_line(&bone, &project.path("main.rock"), MAIN, "shapes::area as", 0, 1),
        ("shapes.rock".into(), 0)
    );
}

#[test]
fn a_qualified_type_lands_on_the_type() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "Box, c", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("Box"), "landed on {covered:?}");
}

#[test]
fn an_aliased_type_lands_on_the_original_type() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "Crate)", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("Box"), "landed on {covered:?}");
}

#[test]
fn a_qualified_global_lands_on_the_global() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "LIMIT", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("LIMIT"), "landed on {covered:?}");
}

#[test]
fn a_qualified_variant_lands_on_the_variant_and_its_enum() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "Round", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("Round"), "landed on {covered:?}");
    let (file, covered) = definition_in_main(&bone, &project, "Kind::Round", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("Kind"), "landed on {covered:?}");
}

/// A legacy package depending on the v1 package `geo`: the legacy file calls
/// `area` through the compatibility projection (spec 3.2).
fn legacy_consumer() -> (tempfile::TempDir, String, &'static str) {
    let dir = tempfile::tempdir().unwrap();
    let geo = dir.path().join("geo");
    let app = dir.path().join("app");
    std::fs::create_dir_all(&geo).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        geo.join("stick.toml"),
        "[package]\nname = \"geo\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n[language]\nmodules = \"v1\"\n",
    )
    .unwrap();
    std::fs::write(geo.join("lib.rock"), "pub fn area(w: int) -> int {\n    w * w\n}\n").unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[dependencies]\ngeo = { path = \"../geo\" }\n",
    )
    .unwrap();
    let main_text = "fn main() {\n    print(\"{area(2)}\")\n}\n";
    std::fs::write(app.join("main.rock"), main_text).unwrap();
    let main = bone_query::canonicalize_lenient(&app.join("main.rock").to_string_lossy());
    (dir, main, main_text)
}

#[test]
fn a_legacy_call_of_a_projected_name_lands_on_the_v1_definition() {
    let (_dir, main, text) = legacy_consumer();
    let mut bone = BoneWorkspace::new();
    let diags = bone.update(&main, text.to_string());
    assert_eq!(errors_in(&diags, &main), Vec::<String>::new());
    let loc = bone.definition(&main, pos_of(text, "area", 0, 1)).expect("a projected call resolves");
    let (path, covered) = located_text(&loc);
    assert!(path.ends_with("geo/lib.rock"), "landed in {path}");
    assert!(covered.contains("area"), "landed on {covered:?}");
}

/// A body edit keeps the other bodies' and the signatures' recorded
/// segments, shifted with the text under them.
#[test]
fn segments_after_a_body_edit_still_navigate() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let edited = MAIN.replace("    let a = shapes::inner::depth()\n", "    let a = shapes::inner::depth()\n    let z = 1\n");
    bone.update(&main, edited.clone());
    let at = |needle: &str, into: usize| {
        let loc = bone.definition(&main, pos_of(&edited, needle, 0, into)).unwrap_or_else(|| panic!("no definition for `{needle}`"));
        let (path, covered) = located_text(&loc);
        (file_name(&path).to_string(), covered)
    };
    let (file, covered) = at("Kind::Round", 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("Kind"), "landed on {covered:?}");
    let (file, covered) = at("Box, c", 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("Box"), "landed on {covered:?}");
}

// -- completion ---------------------------------------------------------------

/// Completion items at the end of `typed`, typed as a new line inside
/// `main`'s body (the line after `let a = ...`).
fn complete_in_main(typed: &str) -> Vec<lsp_types::CompletionItem> {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let anchor = "    let a = shapes::inner::depth()\n";
    let text = MAIN.replace(anchor, &format!("{anchor}    {typed}\n"));
    bone.update(&main, text.clone());
    let pos = pos_of(&text, typed, 0, typed.len());
    let items = bone.completion(&main, pos);
    let prefix: String = typed.chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
    let violations = bone_query::invariants::completion_response_violations(&items, &prefix);
    assert!(violations.is_empty(), "response invariants violated after `{typed}`: {violations:?}");
    items
}

fn labels(items: &[lsp_types::CompletionItem]) -> Vec<String> {
    let mut out: Vec<String> = items.iter().map(|c| c.label.clone()).collect();
    out.sort();
    out.dedup();
    out
}

#[test]
fn completion_after_a_module_path_offers_its_accessible_members() {
    let items = complete_in_main("shapes::");
    assert_eq!(labels(&items), ["Box", "Kind", "LIMIT", "area", "inner", "perimeter"]);
}

#[test]
fn completion_after_a_nested_module_path_offers_its_members() {
    assert_eq!(labels(&complete_in_main("shapes::inner::")), ["depth"]);
}

#[test]
fn completion_after_a_facade_offers_its_re_exports() {
    assert_eq!(labels(&complete_in_main("facade::")), ["depth"]);
}

#[test]
fn completion_after_a_module_path_filters_by_the_typed_prefix() {
    assert_eq!(labels(&complete_in_main("shapes::per")), ["perimeter"]);
}

#[test]
fn bare_completion_offers_what_the_tiers_bind_by_their_bound_names() {
    // Each tier: an aliased import, a glob member (a function, a type, a
    // global), an explicit type import by its alias, the std prelude, and
    // the module's own items.
    for (typed, bound) in [
        ("surf", "surface"),
        ("perim", "perimeter"),
        ("are", "area"),
        ("Crat", "Crate"),
        ("Bo", "Box"),
        ("LIM", "LIMIT"),
        ("prin", "print"),
        ("siz", "sizes"),
    ] {
        let got = labels(&complete_in_main(&format!("let q = {typed}")));
        assert!(got.iter().any(|l| l == bound), "`{bound}` is bound in main.rock: {got:?}");
        for key in got.iter().filter(|l| l.contains("::") && !l.contains("Kind::")) {
            panic!("a DefKey leaked into completion: {key}");
        }
    }
    // Private members of other modules are not callable API, bound through
    // a glob or offered with an import.
    let got = labels(&complete_in_main("let q = hidden"));
    assert!(!got.iter().any(|l| l == "hidden_helper"), "{got:?}");
    let got = labels(&complete_in_main("let q = inner_sec"));
    assert!(!got.iter().any(|l| l == "inner_secret"), "{got:?}");
}

#[test]
fn bare_completion_of_an_unimported_accessible_name_carries_an_import_edit() {
    let items = complete_in_main("let q = dept");
    let depth = items
        .iter()
        .find(|c| c.label == "depth")
        .unwrap_or_else(|| panic!("`depth` is accessible and offered with an import: {:?}", labels(&items)));
    let edits = depth.additional_text_edits.as_ref().expect("an unimported name carries its import");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "use shapes::inner::depth;\n");
    // After the file's last `use` item, at the start of the next line.
    assert_eq!(edits[0].range.start, lsp_types::Position::new(3, 0));
    assert_eq!(edits[0].range.end, lsp_types::Position::new(3, 0));
}

#[test]
fn bare_completion_of_a_bound_name_carries_no_import_edit() {
    let items = complete_in_main("let q = perim");
    let perimeter = items.iter().find(|c| c.label == "perimeter").expect("globbed name offered");
    assert!(perimeter.additional_text_edits.is_none(), "a glob already binds it");
}

// -- rename -------------------------------------------------------------------

/// Every edit of a rename, as `(file name, line, covered old text, new text)`,
/// sorted.
fn rename_edits(bone: &BoneWorkspace, path: &str, pos: Position, new_name: &str) -> Vec<(String, u32, String, String)> {
    let edit = bone.rename(path, pos, new_name).expect("renameable");
    let mut out = Vec::new();
    for (uri, edits) in edit.changes.unwrap_or_default() {
        let file = uri.as_str().strip_prefix("file://").unwrap().to_string();
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        for e in edits {
            let r = e.range;
            assert_eq!(r.start.line, r.end.line);
            let covered: String = lines[r.start.line as usize]
                .chars()
                .skip(r.start.character as usize)
                .take((r.end.character - r.start.character) as usize)
                .collect();
            out.push((file_name(&file).to_string(), r.start.line, covered, e.new_text));
        }
    }
    out.sort();
    out
}

#[test]
fn renaming_a_declaration_renames_its_paths_and_use_items_but_not_an_alias() {
    let project = V1Project::new();
    let bone = project.open();
    let shapes = project.path("shapes.rock");
    let edits = rename_edits(&bone, &shapes, pos_of(SHAPES, "area", 0, 1), "size");
    let want: Vec<(String, u32, String, String)> = vec![
        ("main.rock".into(), 0, "area".into(), "size".into()),
        ("shapes.rock".into(), 2, "area".into(), "size".into()),
    ];
    assert_eq!(edits, want, "`surface` is the alias's own name and stays");
}

#[test]
fn renaming_a_declaration_renames_qualified_paths_and_re_exports() {
    let project = V1Project::new();
    let bone = project.open();
    let inner = project.path("shapes_inner.rock");
    let edits = rename_edits(&bone, &inner, pos_of(INNER, "depth", 0, 1), "level");
    let want: Vec<(String, u32, String, String)> = vec![
        ("facade.rock".into(), 2, "depth".into(), "level".into()),
        ("main.rock".into(), 5, "depth".into(), "level".into()),
        ("main.rock".into(), 8, "depth".into(), "level".into()),
        ("shapes_inner.rock".into(), 2, "depth".into(), "level".into()),
    ];
    assert_eq!(edits, want);
}

#[test]
fn renaming_a_glob_imported_declaration_renames_its_bare_uses() {
    let project = V1Project::new();
    let bone = project.open();
    let shapes = project.path("shapes.rock");
    let edits = rename_edits(&bone, &shapes, pos_of(SHAPES, "perimeter", 0, 1), "outline");
    let want: Vec<(String, u32, String, String)> = vec![
        ("main.rock".into(), 7, "perimeter".into(), "outline".into()),
        ("shapes.rock".into(), 6, "perimeter".into(), "outline".into()),
    ];
    assert_eq!(edits, want);
}

#[test]
fn renaming_an_alias_renames_only_that_files_uses_of_it() {
    let project = V1Project::new();
    let bone = project.open();
    let main = project.path("main.rock");
    for (needle, into) in [("surface;", 1), ("surface(2)", 1)] {
        let edits = rename_edits(&bone, &main, pos_of(MAIN, needle, 0, into), "surf");
        let want: Vec<(String, u32, String, String)> = vec![
            ("main.rock".into(), 0, "surface".into(), "surf".into()),
            ("main.rock".into(), 6, "surface".into(), "surf".into()),
        ];
        assert_eq!(edits, want, "renaming at `{needle}`");
        let range = bone.prepare_rename(&main, pos_of(MAIN, needle, 0, into)).expect("an alias is renameable");
        assert_eq!(range.end.character - range.start.character, "surface".len() as u32);
    }
}
