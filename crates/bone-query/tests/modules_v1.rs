//! The LSP over a modules-v1 package: a file entering the
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
    let width = box_width(b)
    b.w + c.w + shapes::LIMIT + width
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

pub fn box_width(b: Box) -> int {
    b.w
}

pub fn width_value(w: int) -> int {
    w
}

fn hidden_width(b: Box) -> int {
    b.w
}

pub struct Box {
    pub w: int,
    private secret: int,
}

pub enum Kind {
    Round,
    Square,
}

pub global LIMIT: int = 10

pub global BOX: Box = Box { w: 2, secret: 3 }

global SECRET_BOX: Box = Box { w: 4, secret: 5 }

pub fn box_area(b: Box) -> int {
    b.w * b.w
}

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
        let errors: Vec<_> = diags
            .iter()
            .flat_map(|(p, d)| d.iter().map(move |d| (p, d)))
            .collect();
        assert!(
            errors
                .iter()
                .all(|(_, d)| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
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
    let path = loc.uri.to_file_path().unwrap();
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
        lines[r.start.line as usize]
            .chars()
            .skip(r.start.character as usize)
            .collect()
    };
    (path, covered)
}

fn file_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
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
    assert_eq!(
        errors_in(&diags, &extra),
        Vec::<String>::new(),
        "a v1 file of a v1 package must build as v1"
    );

    let loc = bone
        .definition(&extra, pos_of(extra_text, "area", 0, 1))
        .expect("a call in the new fragment resolves");
    let (path, covered) = located_text(&loc);
    assert_eq!(file_name(&path), "shapes.rock");
    assert!(covered.contains("area"), "landed on {covered:?}");
}

// -- go-to-definition ---------------------------------------------------------

fn definition_in_main(
    bone: &BoneWorkspace,
    project: &V1Project,
    needle: &str,
    nth: usize,
    into: usize,
) -> (String, String) {
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
fn references_of_a_qualified_call_cover_only_the_callable_segment() {
    let project = V1Project::new();
    let bone = project.open();
    let main = project.path("main.rock");
    let references = bone.references(&main, pos_of(MAIN, "depth()", 0, 1));
    let covered: Vec<_> = references
        .iter()
        .filter_map(|location| {
            let (path, text) = located_text(location);
            (file_name(&path) == "main.rock").then_some(text)
        })
        .collect();

    assert!(
        covered.iter().any(|text| text == "depth"),
        "references: {covered:?}"
    );
    assert!(
        covered.iter().all(|text| text != "shapes::inner::depth"),
        "a qualified reference must not cover its path: {covered:?}"
    );
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
    assert_eq!(
        file, "shapes_inner.rock",
        "a `pub use` is not the definition"
    );
    assert!(covered.contains("depth"), "landed on {covered:?}");
}

// -- the std prelude ----------------------------------------------------------

/// A v1 fragment sees the std prelude in the editor as it does in
/// a compile: `print` and `*` resolve.
#[test]
fn the_std_prelude_resolves_in_a_v1_file() {
    let project = V1Project::new();
    let mut bone = BoneWorkspace::new();
    let shapes = project.path("shapes.rock");
    let diags = bone.update(&shapes, SHAPES.to_string());
    assert_eq!(errors_in(&diags, &shapes), Vec::<String>::new());
    assert_eq!(
        errors_in(&diags, &project.path("main.rock")),
        Vec::<String>::new()
    );
}

// -- go-to-definition on each path segment ------------------------------------

/// The line a definition landed on, with its file.
fn definition_line(
    bone: &BoneWorkspace,
    path: &str,
    text: &str,
    needle: &str,
    nth: usize,
    into: usize,
) -> (String, u32) {
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
    assert_eq!(
        definition_line(&bone, &main, MAIN, "shapes::inner::depth", 0, 1),
        ("shapes.rock".into(), 0)
    );
    assert_eq!(
        definition_line(&bone, &main, MAIN, "inner::depth", 0, 1),
        ("shapes_inner.rock".into(), 0)
    );
    assert_eq!(
        definition_line(&bone, &main, MAIN, "facade::depth", 0, 1),
        ("facade.rock".into(), 0)
    );
}

#[test]
fn a_use_item_segment_lands_on_what_it_names() {
    let project = V1Project::new();
    let bone = project.open();
    let (file, covered) = definition_in_main(&bone, &project, "area as surface", 0, 1);
    assert_eq!(file, "shapes.rock");
    assert!(covered.contains("area"), "landed on {covered:?}");
    assert_eq!(
        definition_line(
            &bone,
            &project.path("main.rock"),
            MAIN,
            "shapes::area as",
            0,
            1
        ),
        ("shapes.rock".into(), 0)
    );
}

#[test]
fn an_inline_module_use_item_records_each_path_segment() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let text = format!(
        "{MAIN}\n\
         pub mod inline {{\n\
             use ::shapes::area as local_area;\n\
             pub fn run() -> int {{ local_area(2) }}\n\
         }}\n"
    );
    bone.update(&main, text.clone());
    let loc = bone
        .definition(&main, pos_of(&text, "::shapes::area as", 0, 11))
        .expect("inline use path resolves");
    let (path, covered) = located_text(&loc);
    let file = file_name(&path).to_string();
    assert_eq!(file, "shapes.rock");
    assert!(
        covered.contains("area"),
        "inline import landed on {covered:?}"
    );
    let loc = bone
        .definition(&main, pos_of(&text, "local_area;", 0, 1))
        .expect("inline use alias resolves");
    let (path, covered) = located_text(&loc);
    let file = file_name(&path).to_string();
    assert_eq!(file, "shapes.rock");
    assert!(
        covered.contains("area"),
        "inline alias landed on {covered:?}"
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
fn hover_on_v1_signature_types_records_each_nominal_spelling() {
    let project = V1Project::new();
    let text = format!(
        "{}\nfn signature(first: Crate) -> shapes::Box {{\n    first\n}}\n",
        MAIN
    );
    std::fs::write(project.path("main.rock"), &text).unwrap();

    let mut bone = BoneWorkspace::new();
    let main = project.path("main.rock");
    let diagnostics = bone.update(&main, text.clone());
    assert_eq!(errors_in(&diagnostics, &main), Vec::<String>::new());

    let hover_at = |position| {
        bone.hover(&main, position)
            .unwrap_or_else(|| panic!("no hover at {position:?}"))
    };

    let alias_hover = hover_at(pos_of(&text, "first: Crate", 0, 8));
    let qualified_hover = hover_at(pos_of(&text, "-> shapes::Box", 0, 11));

    let alias_text = match alias_hover.contents {
        lsp_types::HoverContents::Markup(markup) => markup.value,
        contents => panic!("unexpected aliased hover contents: {contents:?}"),
    };
    let qualified_text = match qualified_hover.contents {
        lsp_types::HoverContents::Markup(markup) => markup.value,
        contents => panic!("unexpected qualified hover contents: {contents:?}"),
    };

    assert!(
        alias_text.contains("struct app::shapes::Box"),
        "aliased hover: {alias_text}"
    );
    assert!(
        qualified_text.contains("struct app::shapes::Box"),
        "qualified hover: {qualified_text}"
    );
    assert_ne!(alias_hover.range, qualified_hover.range);
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
/// `area` through the compatibility projection.
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
    std::fs::write(
        geo.join("lib.rock"),
        "pub fn area(w: int) -> int {\n    w * w\n}\n",
    )
    .unwrap();
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
    let loc = bone
        .definition(&main, pos_of(text, "area", 0, 1))
        .expect("a projected call resolves");
    let (path, covered) = located_text(&loc);
    assert!(
        std::path::Path::new(&path).ends_with("geo/lib.rock"),
        "landed in {path}"
    );
    assert!(covered.contains("area"), "landed on {covered:?}");
}

/// A body edit keeps the other bodies' and the signatures' recorded
/// segments, shifted with the text under them.
#[test]
fn segments_after_a_body_edit_still_navigate() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let edited = MAIN.replace(
        "    let a = shapes::inner::depth()\n",
        "    let a = shapes::inner::depth()\n    let z = 1\n",
    );
    bone.update(&main, edited.clone());
    let at = |needle: &str, into: usize| {
        let loc = bone
            .definition(&main, pos_of(&edited, needle, 0, into))
            .unwrap_or_else(|| panic!("no definition for `{needle}`"));
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
    let prefix: String = typed
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let violations = bone_query::invariants::completion_response_violations(&items, &prefix);
    assert!(
        violations.is_empty(),
        "response invariants violated after `{typed}`: {violations:?}"
    );
    items
}

fn complete_in_sizes(typed: &str) -> Vec<lsp_types::CompletionItem> {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let anchor = "    let width = box_width(b)\n";
    let text = MAIN.replace(anchor, &format!("{anchor}    let q = {typed}\n"));
    bone.update(&main, text.clone());
    let marker = format!("let q = {typed}");
    let pos = pos_of(&text, &marker, 0, marker.len());
    bone.completion(&main, pos)
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
    assert_eq!(
        labels(&items),
        [
            "BOX",
            "Box",
            "Kind",
            "LIMIT",
            "area",
            "box_area",
            "box_width",
            "inner",
            "perimeter",
            "width_value"
        ]
    );
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
        assert!(
            got.iter().any(|l| l == bound),
            "`{bound}` is bound in main.rock: {got:?}"
        );
        if let Some(key) = got
            .iter()
            .find(|l| l.contains("::") && !l.contains("Kind::"))
        {
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
        .unwrap_or_else(|| {
            panic!(
                "`depth` is accessible and offered with an import: {:?}",
                labels(&items)
            )
        });
    let edits = depth
        .additional_text_edits
        .as_ref()
        .expect("an unimported name carries its import");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "use shapes::inner::depth;\n");
    // After the file's last `use` item, at the start of the next line.
    assert_eq!(edits[0].range.start, lsp_types::Position::new(3, 0));
    assert_eq!(edits[0].range.end, lsp_types::Position::new(3, 0));
}

#[test]
fn bare_completion_of_a_bound_name_carries_no_import_edit() {
    let items = complete_in_main("let q = perim");
    let perimeter = items
        .iter()
        .find(|c| c.label == "perimeter")
        .expect("globbed name offered");
    assert!(
        perimeter.additional_text_edits.is_none(),
        "a glob already binds it"
    );
}

#[test]
fn v1_call_through_uses_bound_spelling_and_hides_private_globals() {
    let items = complete_in_main("let q = boxwidthb");
    let labels = labels(&items);
    assert!(
        labels.iter().any(|label| label == "box_width(BOX)"),
        "public v1 call-through: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("SECRET_BOX")),
        "private global leaked: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("app::shapes::")),
        "DefKey leaked: {labels:?}"
    );
}

#[test]
fn v1_reach_completion_uses_bound_spelling_and_field_visibility() {
    let items = complete_in_sizes("widthvaluebw");
    let labels = labels(&items);
    assert!(
        labels.iter().any(|label| label == "width_value(BOX.w)"),
        "public field reach path: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("b.secret")),
        "private field leaked: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("app::shapes::")),
        "DefKey leaked: {labels:?}"
    );
}

#[test]
fn v1_call_through_filters_private_fields_on_public_global_roots() {
    let items = complete_in_sizes("widthvalues");
    let labels = labels(&items);
    assert!(
        !labels
            .iter()
            .any(|label| label == "width_value(BOX.secret)"),
        "private global field leaked: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("app::shapes::")),
        "DefKey leaked: {labels:?}"
    );
}

#[test]
fn v1_call_through_hides_inaccessible_callable_members() {
    let items = complete_in_main("let q = hiddenwidthb");
    let labels = labels(&items);
    assert!(
        !labels.iter().any(|label| label == "hidden_width(BOX)"),
        "private callable leaked: {labels:?}"
    );
}

#[test]
fn changing_v1_visibility_invalidates_synthesis_cache() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let shapes = project.path("shapes.rock");
    let anchor = "    let a = shapes::inner::depth()\n";
    let text = MAIN.replace(anchor, &format!("{anchor}    let q = boxwidthb\n"));
    bone.update(&main, text.clone());
    let pos = pos_of(&text, "let q = boxwidthb", 0, "let q = boxwidthb".len());
    let before = labels(&bone.completion(&main, pos));
    assert!(
        before.iter().any(|label| label == "box_width(BOX)"),
        "fixture did not seed cache: {before:?}"
    );

    let changed = SHAPES.replace("pub global BOX", "global BOX");
    bone.update(&shapes, changed);
    let after = labels(&bone.completion(&main, pos));
    assert!(
        !after.iter().any(|label| label == "box_width(BOX)"),
        "private global survived synthesis cache: {after:?}"
    );
}

#[test]
fn changing_v1_visibility_invalidates_bound_completion_cache() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let shapes = project.path("shapes.rock");
    let anchor = "    let a = shapes::inner::depth()\n";
    let text = MAIN.replace(anchor, &format!("{anchor}    let q = perim\n"));
    bone.update(&main, text.clone());
    let pos = pos_of(&text, "let q = perim", 0, "let q = perim".len());
    let before = labels(&bone.completion(&main, pos));
    assert!(
        before.iter().any(|label| label == "perimeter"),
        "fixture did not seed cache: {before:?}"
    );

    let changed = SHAPES.replace("pub fn perimeter", "fn perimeter");
    bone.update(&shapes, changed);
    let after = labels(&bone.completion(&main, pos));
    assert!(
        !after.iter().any(|label| label == "perimeter"),
        "private glob member survived bound cache: {after:?}"
    );
}

#[test]
fn changing_v1_import_alias_invalidates_bound_completion_cache() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let anchor = "    let a = shapes::inner::depth()\n";
    let text = MAIN.replace(anchor, &format!("{anchor}    let q = surf\n"));
    bone.update(&main, text.clone());
    let pos = pos_of(&text, "let q = surf", 0, "let q = surf".len());
    let before = bone.completion(&main, pos);
    assert!(
        before.iter().any(|item| item.label == "surface"),
        "fixture did not seed alias cache"
    );

    let body_changed = text.replace("surface(2)", "surface(3)");
    bone.update(&main, body_changed.clone());
    let after_body = bone.completion(
        &main,
        pos_of(&body_changed, "let q = surf", 0, "let q = surf".len()),
    );
    assert_eq!(
        after_body, before,
        "body-only edit changed bound completion output"
    );

    let changed = body_changed
        .replace("use shapes::area as surface;", "use shapes::area as surf;")
        .replace("surface(3)", "surf(3)");
    bone.update(&main, changed.clone());
    let after = bone.completion(
        &main,
        pos_of(&changed, "let q = surf", 0, "let q = surf".len()),
    );
    assert!(
        !after.iter().any(|item| item.label == "surface"),
        "old alias survived bound cache"
    );
    let surf = after
        .iter()
        .find(|item| item.label == "surf")
        .expect("new alias is offered");
    assert_eq!(surf.detail.as_deref(), Some("fn surf(w: int) -> int"));
}

#[test]
fn aliased_callable_completion_carries_signature_detail() {
    let items = complete_in_main("let q = surf");
    let surface = items
        .iter()
        .find(|item| item.label == "surface")
        .expect("the aliased callable is offered");
    let detail = surface.detail.as_deref().unwrap_or("");
    assert!(
        detail.starts_with("fn "),
        "aliased callable detail: {detail:?}"
    );
    assert!(
        detail.contains("(w: int) -> int"),
        "aliased callable detail: {detail:?}"
    );
}

#[test]
fn v1_global_completion_detail_survives_body_updates() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let anchor = "    let a = shapes::inner::depth()\n";
    let text = MAIN.replace(anchor, &format!("{anchor}    let q = LIM\n"));
    bone.update(&main, text.clone());
    let marker = "let q = LIM";
    let before = bone.completion(&main, pos_of(&text, marker, 0, marker.len()));
    let limit = before
        .iter()
        .find(|item| item.label == "LIMIT")
        .expect("the public global is offered");
    assert_eq!(limit.detail.as_deref(), Some("int"));
    assert_eq!(
        limit
            .label_details
            .as_ref()
            .and_then(|details| details.detail.as_deref()),
        Some(": int")
    );

    let body_changed = text.replace("surface(2)", "surface(3)");
    bone.update(&main, body_changed.clone());
    let after = bone.completion(&main, pos_of(&body_changed, marker, 0, marker.len()));
    assert_eq!(
        after, before,
        "body-only edit changed global completion details"
    );
    let limit = after
        .iter()
        .find(|item| item.label == "LIMIT")
        .expect("the public global remains offered");
    assert_eq!(limit.detail.as_deref(), Some("int"));
    assert_eq!(
        limit
            .label_details
            .as_ref()
            .and_then(|details| details.detail.as_deref()),
        Some(": int")
    );
}

#[test]
fn callable_completion_uses_the_accessible_overload_not_the_family_first() {
    let project = V1Project::new();
    let public_overload = "pub fn family(n: int, scale: int) -> int {\n    n * scale\n}\n";
    let shapes_text =
        format!("{SHAPES}\nfn family(s: string) -> string {{\n    s\n}}\n\n{public_overload}");
    std::fs::write(project.dir.path().join("shapes.rock"), &shapes_text).unwrap();

    let mut bone = project.open();
    let main = project.path("main.rock");
    let shapes_path = project.path("shapes.rock");
    let anchor = "    let a = shapes::inner::depth()\n";
    let text = MAIN
        .replace(
            "use shapes::*;",
            "use shapes::*;\nuse shapes::family as fam;",
        )
        .replace(anchor, &format!("{anchor}    let q = fam\n"));
    bone.update(&main, text.clone());
    let marker = "let q = fam";
    let completion = |bone: &mut BoneWorkspace, text: &str| {
        bone.completion(&main, pos_of(text, marker, 0, marker.len()))
            .into_iter()
            .find(|item| item.label == "fam")
            .expect("the aliased accessible overload is offered")
    };
    let before = completion(&mut bone, &text);
    assert_eq!(
        before.detail.as_deref(),
        Some("fn fam(n: int, scale: int) -> int")
    );
    assert_eq!(before.insert_text.as_deref(), Some("fam($0)"));
    assert_eq!(
        before.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET)
    );

    let body_changed = shapes_text.replace("    n * scale\n", "    n * scale + 1\n");
    bone.update(&shapes_path, body_changed.clone());
    let after_body = completion(&mut bone, &text);
    assert_eq!(
        after_body, before,
        "body-only edit changed aliased overload completion"
    );

    let signature_changed = body_changed.replace(
        "pub fn family(n: int, scale: int) -> int {\n    n * scale + 1\n}\n",
        "pub fn family() -> int {\n    0\n}\n",
    );
    assert_ne!(
        signature_changed, body_changed,
        "the signature edit must match the selected overload"
    );
    bone.update(&shapes_path, signature_changed);
    let after_signature = completion(&mut bone, &text);
    assert_ne!(
        after_signature, before,
        "signature edit left cached overload metadata in place"
    );
    assert_eq!(after_signature.detail.as_deref(), Some("fn fam() -> int"));
    assert_eq!(after_signature.insert_text.as_deref(), Some("fam()"));
    assert_eq!(after_signature.insert_text_format, None);
}

#[test]
fn body_only_module_rebuild_refreshes_cached_callable_metadata_across_fragments() {
    let project = V1Project::new();
    // Both files contribute to the same v1 module and both overloads are
    // public. The first file's zero-arg declaration is initially first in the
    // module's callable family; rebuilding its multiple item bodies moves the
    // one-arg overload from the second file to the front.
    let first = "pub mod shapes;\n\npub fn family() -> int {\n    0\n}\n\npub fn first_helper() -> int {\n    1\n}\n";
    let second = "pub mod shapes;\n\npub fn family(n: int) -> int {\n    n\n}\n";
    std::fs::write(project.dir.path().join("a.rock"), first).unwrap();
    std::fs::write(project.dir.path().join("b.rock"), second).unwrap();

    let mut bone = project.open();
    let main = project.path("main.rock");
    let text = "use shapes::family as fam;\n\nfn main() {\n    let q = fam\n}\n";
    bone.update(&main, text.to_string());
    let marker = "let q = fam";
    let completion = |bone: &mut BoneWorkspace| {
        bone.completion(&main, pos_of(text, marker, 0, marker.len()))
            .into_iter()
            .find(|item| item.label == "fam")
            .expect("the accessible aliased family is offered")
    };

    let before = completion(&mut bone);
    assert_eq!(before.detail.as_deref(), Some("fn fam() -> int"));
    assert_eq!(before.insert_text.as_deref(), Some("fam()"));
    assert_eq!(before.insert_text_format, None);

    // Change two bodies while keeping the module's item/signature layout.
    // This takes the session's BodyOnly module-rebuild path.
    let first_body_edit = first
        .replace("    0\n", "    10\n")
        .replace("    1\n", "    2\n");
    bone.update(&project.path("a.rock"), first_body_edit);

    let after = completion(&mut bone);
    assert_eq!(after.detail.as_deref(), Some("fn fam(n: int) -> int"));
    assert_eq!(after.insert_text.as_deref(), Some("fam($0)"));
    assert_eq!(
        after.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET)
    );
}

// -- rename -------------------------------------------------------------------

/// Every edit of a rename, as `(file name, line, covered old text, new text)`,
/// sorted.
fn rename_edits(
    bone: &BoneWorkspace,
    path: &str,
    pos: Position,
    new_name: &str,
) -> Vec<(String, u32, String, String)> {
    let edit = bone.rename(path, pos, new_name).expect("renameable");
    let mut out = Vec::new();
    for (uri, edits) in edit.changes.unwrap_or_default() {
        let file = uri.to_file_path().unwrap();
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
            out.push((
                file_name(&file).to_string(),
                r.start.line,
                covered,
                e.new_text,
            ));
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
        (
            "shapes_inner.rock".into(),
            2,
            "depth".into(),
            "level".into(),
        ),
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
        (
            "shapes.rock".into(),
            6,
            "perimeter".into(),
            "outline".into(),
        ),
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
        let range = bone
            .prepare_rename(&main, pos_of(MAIN, needle, 0, into))
            .expect("an alias is renameable");
        assert_eq!(
            range.end.character - range.start.character,
            "surface".len() as u32
        );
    }
}

/// Type-directed synthesized items (call-throughs such as `f(pos)`, reach
/// paths such as `pos.f()`) spell workspace function names too; in a v1
/// file none may spell a DefKey. Pinned on a local chess-core fixture, where
/// the prefix `ch_t` inside `ch_type(p)` can expose a `chess_core::` spelling.
#[test]
fn no_completion_in_a_v1_file_spells_a_def_key() {
    let project =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chess_completion");
    let source = project.join("chess_moves.rock");
    let file = bone_query::canonicalize_lenient(&source.to_string_lossy());
    let text = std::fs::read_to_string(&source).expect("chess completion fixture is readable");
    let mut bone = BoneWorkspace::new();
    let diags = bone.update(&file, text.clone());
    let errors: Vec<_> = diags
        .iter()
        .flat_map(|(_, ds)| ds)
        .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
        .collect();
    assert!(
        errors.is_empty(),
        "fixture package must build clean: {errors:#?}"
    );
    let at = pos_of(&text, "ch_type(p)", 0, 4);
    let items = bone.completion(&file, at);
    assert!(!items.is_empty());
    for item in items {
        let insert = item.insert_text.clone().unwrap_or_default();
        assert!(
            !item.label.contains("chess_core::") && !insert.contains("chess_core::"),
            "{} / {insert}",
            item.label
        );
    }
}

/// A module alias is a path anchor: the alias segment of a path
/// through it lands on the aliased module's header, and the member after it
/// on the member's definition.
#[test]
fn a_module_alias_segment_lands_on_the_aliased_module() {
    let project = V1Project::new();
    let main_text = format!(
        "use shapes::inner as deep;\n\n{MAIN}\nfn deeper() -> int {{\n    deep::depth()\n}}\n"
    );
    let main = project.path("main.rock");
    std::fs::write(&main, &main_text).unwrap();
    let mut bone = BoneWorkspace::new();
    let diags = bone.update(&main, main_text.clone());
    let errors: Vec<_> = diags
        .iter()
        .flat_map(|(_, d)| d.iter())
        .filter(|d| d.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(
        definition_line(&bone, &main, &main_text, "deep::depth", 0, 1),
        ("shapes_inner.rock".into(), 0)
    );
    assert_eq!(
        definition_line(&bone, &main, &main_text, "as deep", 0, 4),
        ("shapes_inner.rock".into(), 0)
    );
    assert_eq!(
        definition_line(&bone, &main, &main_text, "deep::depth", 0, 7),
        ("shapes_inner.rock".into(), 2)
    );
}

#[test]
fn an_import_edit_goes_in_sorted_position_among_the_uses() {
    let project = V1Project::new();
    let mut bone = project.open();
    let main = project.path("main.rock");
    let head = "use shapes::area as surface;\nuse shapes::*;\nuse shapes::Box as Crate;\n";
    let sorted = "use shapes::*;\nuse shapes::Box as Crate;\nuse shapes::area as surface;\nuse shapes::perimeter;\n";
    let anchor = "    let a = shapes::inner::depth()\n";
    assert!(MAIN.contains(head));
    let text = MAIN
        .replacen(head, sorted, 1)
        .replace(anchor, &format!("{anchor}    let q = dept\n"));
    bone.update(&main, text.clone());
    let items = bone.completion(
        &main,
        pos_of(&text, "let q = dept", 0, "let q = dept".len()),
    );
    let depth = items
        .iter()
        .find(|c| c.label == "depth")
        .expect("`depth` is offered with an import");
    let edits = depth
        .additional_text_edits
        .as_ref()
        .expect("an unimported name carries its import");
    assert_eq!(edits[0].new_text, "use shapes::inner::depth;\n");
    // Before `use shapes::perimeter;`, the first `use` that sorts after it.
    assert_eq!(edits[0].range.start, Position::new(3, 0));
}

/// A definition in the view of an `allow_legacy` dependency is an
/// import candidate like a v1 dependency's.
#[test]
fn an_allow_legacy_providers_definition_is_offered_with_an_import() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("x_lib");
    let app = dir.path().join("app");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        lib.join("stick.toml"),
        "[package]\nname = \"x_lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("lib.rock"),
        "fn wander(n: int) -> int {\n    n\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("stick.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n\n\
         [dependencies]\nx_lib = { path = \"../x_lib\", allow_legacy = true }\n",
    )
    .unwrap();
    let text = "fn main() {\n    let q = wand\n}\n";
    std::fs::write(app.join("main.rock"), text).unwrap();
    let main = bone_query::canonicalize_lenient(&app.join("main.rock").to_string_lossy());
    let mut bone = BoneWorkspace::new();
    bone.update(&main, text.to_string());
    let items = bone.completion(&main, pos_of(text, "let q = wand", 0, "let q = wand".len()));
    let wander = items
        .iter()
        .find(|c| c.label == "wander")
        .unwrap_or_else(|| panic!("`wander` is offered: {:?}", labels(&items)));
    let edits = wander
        .additional_text_edits
        .as_ref()
        .expect("it carries its import");
    assert_eq!(edits[0].new_text, "use x_lib::wander;\n");
    assert_eq!(edits[0].range.start, Position::new(0, 0));
}

#[test]
fn qualified_nullable_annotations_are_not_expression_holes() {
    let project = V1Project::new();
    std::fs::write(
        project.dir.path().join("shapes.rock"),
        "pub mod shapes;\npub struct Cell<T> { pub value: T }\n",
    )
    .unwrap();
    let main = project.path("main.rock");
    let source = "fn main() {\n\
        let absent: shapes::Cell<int>? = null\n\
        let unit: shapes::Cell<()>? = null\n\
        let actual: shapes::Cell<int>? = ?\n\
        }\n";
    std::fs::write(&main, source).unwrap();
    let mut bone = BoneWorkspace::new();
    let updated = bone.update(&main, source.to_string());
    let diagnostics = bone.diagnostics_for(&main);
    assert!(
        diagnostics
            .iter()
            .all(|d| d.severity != Some(lsp_types::DiagnosticSeverity::ERROR)),
        "{diagnostics:?}"
    );
    let actual = pos_of(source, "= ?", 0, 2);
    for diagnostics in [
        diagnostics,
        updated
            .into_iter()
            .find(|(path, _)| path == &main)
            .unwrap()
            .1,
    ] {
        let holes: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("expression hole"))
            .collect();
        assert_eq!(
            holes.len(),
            1,
            "only the unfinished initializer is a hole: {holes:?}"
        );
        assert_eq!(holes[0].range.start, actual);
    }
    for occurrence in 0..3 {
        let nullable = pos_of(source, ">?", occurrence, 1);
        assert!(
            bone.hole_info(&main, nullable).is_none(),
            "nullable annotation #{occurrence} is not a hole"
        );
    }
    assert!(
        bone.hole_info(&main, actual)
            .unwrap()
            .starts_with("Expression hole expecting:")
    );
    bone.update(&main, source.replace("= ?", "= null"));
    assert!(
        !bone
            .diagnostics_for(&main)
            .iter()
            .any(|d| d.message.contains("hole"))
    );
}
