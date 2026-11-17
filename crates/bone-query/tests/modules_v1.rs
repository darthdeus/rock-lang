//! The LSP over a modules-v1 package: a file entering the
//! session after the batch load keeps its manifest facts, and go-to-definition
//! follows what the resolver recorded -- module paths, aliases, globs and
//! re-exports -- to the defining item.

use crate::completion::{assert_invariants, assert_labels, find, labels_of};
use crate::fixture::{Project, assert_no_errors, covered, position_of_offset};
use crate::label_asserts::{assert_lacks, assert_offers};
use bone_query::BoneWorkspace;
use lsp_types::Position;

/// A v1 `bin` package `app` on disk. The root fragment reaches the child
/// module `shapes` by a qualified path, an aliased import, a glob and a
/// re-export through `facade`.
pub struct V1Project(Project);

impl std::ops::Deref for V1Project {
    type Target = Project;

    fn deref(&self) -> &Project {
        &self.0
    }
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
        Self(Project::new(&[
            (
                "stick.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
            ),
            ("main.rock", MAIN),
            ("shapes.rock", SHAPES),
            ("shapes_inner.rock", INNER),
            ("facade.rock", FACADE),
        ]))
    }

    /// A workspace with the package loaded through the editor route
    /// (`update` on the root fragment, as didOpen does), asserting the
    /// package builds clean -- a test below that sees a diagnostic reads a
    /// real regression, not a broken fixture.
    pub fn open(&self) -> BoneWorkspace {
        let (bone, diags) = self.0.open("main.rock");
        assert_no_errors(&diags);
        bone
    }
}

fn opened() -> (V1Project, BoneWorkspace, String) {
    let project = V1Project::new();
    let bone = project.open();
    let main = project.path("main.rock");
    (project, bone, main)
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
    position_of_offset(text, offset)
}

/// The text a location's range covers, read from disk.
fn located_text(loc: &lsp_types::Location) -> (String, String) {
    let path = loc.uri.to_file_path().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    (path, covered(&text, loc.range).to_string())
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
/// `Session::update_file` with no facts and parse as standalone v1.
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

fn definition_at(
    bone: &BoneWorkspace,
    path: &str,
    text: &str,
    needle: &str,
    into: usize,
) -> (String, String) {
    let loc = bone
        .definition(path, pos_of(text, needle, 0, into))
        .unwrap_or_else(|| panic!("no definition for `{needle}`"));
    let (path, covered) = located_text(&loc);
    (file_name(&path).to_string(), covered)
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

    assert_offers(&covered, &["depth"]);
    assert!(
        covered.iter().all(|text| text != "shapes::inner::depth"),
        "a qualified reference must not cover its path: {covered:?}"
    );
}

#[test]
fn definition_cases() {
    let project = V1Project::new();
    let bone = project.open();

    let (file, covered) = definition_at(&bone, &project.path("main.rock"), MAIN, "depth()", 1);
    assert_eq!(file, "shapes_inner.rock");
    assert_eq!(covered, "depth");

    let items: &[(&str, usize, &str, &str)] = &[
        ("Round", 1, "shapes.rock", "Round"),
        ("Kind::Round", 1, "shapes.rock", "Kind"),
        ("Box, c", 1, "shapes.rock", "Box"),
        ("Crate)", 1, "shapes.rock", "Box"),
        ("LIMIT", 1, "shapes.rock", "LIMIT"),
        ("area as surface", 1, "shapes.rock", "area"),
    ];
    for (needle, into, expected_file, item) in items {
        let (file, covered) = definition_at(&bone, &project.path("main.rock"), MAIN, needle, *into);
        assert_eq!(file, *expected_file, "{needle}");
        assert!(covered.contains(item), "{needle} landed on {covered:?}");
    }

    let main = project.path("main.rock");
    let segments = [
        ("shapes::inner::depth", "shapes.rock", 0),
        ("inner::depth", "shapes_inner.rock", 0),
        ("facade::depth", "facade.rock", 0),
        ("shapes::area as", "shapes.rock", 0),
    ];
    for (needle, expected_file, expected_line) in segments {
        assert_eq!(
            definition_line(&bone, &main, MAIN, needle, 0, 1),
            (expected_file.into(), expected_line),
            "{needle}"
        );
    }
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
fn an_inline_module_use_item_records_each_path_segment() {
    let (_project, mut bone, main) = opened();
    let text = format!(
        "{MAIN}\n\
         pub mod inline {{\n\
             use ::shapes::area as local_area;\n\
             pub fn run() -> int {{ local_area(2) }}\n\
         }}\n"
    );
    bone.update(&main, text.clone());
    for (needle, into) in [("::shapes::area as", 11), ("local_area;", 1)] {
        let (file, covered) = definition_at(&bone, &main, &text, needle, into);
        assert_eq!(file, "shapes.rock");
        assert!(covered.contains("area"), "{needle} landed on {covered:?}");
    }
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

    for hover in [&alias_hover, &qualified_hover] {
        let lsp_types::HoverContents::Markup(markup) = &hover.contents;
        assert!(
            markup.value.contains("struct app::shapes::Box"),
            "{markup:?}"
        );
    }
    assert_ne!(alias_hover.range, qualified_hover.range);
}

/// A body edit keeps the other bodies' and the signatures' recorded
/// segments, shifted with the text under them.
#[test]
fn segments_after_a_body_edit_still_navigate() {
    let (_project, mut bone, main) = opened();
    let edited = MAIN.replace(
        "    let a = shapes::inner::depth()\n",
        "    let a = shapes::inner::depth()\n    let z = 1\n",
    );
    bone.update(&main, edited.clone());
    for (needle, item) in [("Kind::Round", "Kind"), ("Box, c", "Box")] {
        let (file, covered) = definition_at(&bone, &main, &edited, needle, 1);
        assert_eq!(file, "shapes.rock");
        assert!(covered.contains(item), "{needle} landed on {covered:?}");
    }
}

// -- completion ---------------------------------------------------------------

/// Completion items at the end of `typed`, typed as a new line inside
/// `main`'s body (the line after `let a = ...`).
const MAIN_ANCHOR: &str = "    let a = shapes::inner::depth()\n";
const SIZES_ANCHOR: &str = "    let width = box_width(b)\n";

fn with_completion(typed: &str, anchor: &str) -> String {
    MAIN.replace(anchor, &format!("{anchor}    {typed}\n"))
}

fn complete_at(
    bone: &mut BoneWorkspace,
    main: &str,
    text: &str,
    marker: &str,
) -> Vec<lsp_types::CompletionItem> {
    bone.completion(main, pos_of(text, marker, 0, marker.len()))
}

fn complete_in_main(typed: &str) -> Vec<lsp_types::CompletionItem> {
    complete_at_anchor(typed, MAIN_ANCHOR)
}

fn complete_in_sizes(typed: &str) -> Vec<lsp_types::CompletionItem> {
    complete_at_anchor(&format!("let q = {typed}"), SIZES_ANCHOR)
}

fn complete_at_anchor(typed: &str, anchor: &str) -> Vec<lsp_types::CompletionItem> {
    let (_project, mut bone, main) = opened();
    let text = with_completion(typed, anchor);
    bone.update(&main, text.clone());
    let items = complete_at(&mut bone, &main, &text, typed);
    assert_invariants(&format!("{typed}<|>"), &items);
    items
}

fn labels(items: &[lsp_types::CompletionItem]) -> Vec<String> {
    let mut out = labels_of(items);
    out.sort();
    out.dedup();
    out
}

#[test]
fn module_path_completions() {
    for (typed, expected) in [
        (
            "shapes::",
            &[
                "BOX",
                "Box",
                "Kind",
                "LIMIT",
                "area",
                "box_area",
                "box_width",
                "inner",
                "perimeter",
                "width_value",
            ][..],
        ),
        ("shapes::inner::", &["depth"]),
        ("facade::", &["depth"]),
        ("shapes::per", &["perimeter"]),
    ] {
        assert_eq!(labels(&complete_in_main(typed)), expected, "{typed}");
    }
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
    assert_lacks(&got, &["hidden_helper"]);
    let got = labels(&complete_in_main("let q = inner_sec"));
    assert_lacks(&got, &["inner_secret"]);
}

#[test]
fn bare_completion_of_an_unimported_accessible_name_carries_an_import_edit() {
    let items = complete_in_main("let q = dept");
    let depth = find(&items, "depth");
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
    let perimeter = find(&items, "perimeter");
    assert!(
        perimeter.additional_text_edits.is_none(),
        "a glob already binds it"
    );
}

#[test]
fn synthesized_completions_respect_bound_spelling_and_visibility() {
    let cases: &[(bool, &str, &[&str])] = &[
        (
            false,
            "let q = boxwidthb",
            &["box_width(BOX)", "!*SECRET_BOX*", "!*app::shapes::*"],
        ),
        (
            true,
            "widthvaluebw",
            &["width_value(BOX.w)", "!*b.secret*", "!*app::shapes::*"],
        ),
        (
            true,
            "widthvalues",
            &["!width_value(BOX.secret)", "!*app::shapes::*"],
        ),
        (false, "let q = hiddenwidthb", &["!hidden_width(BOX)"]),
    ];
    for (sizes, typed, patterns) in cases {
        let items = if *sizes {
            complete_in_sizes(typed)
        } else {
            complete_in_main(typed)
        };
        assert_labels(typed, &labels(&items), patterns);
    }
}

#[test]
fn changing_v1_visibility_invalidates_completion_caches() {
    for (typed, label, public, private) in [
        (
            "let q = boxwidthb",
            "box_width(BOX)",
            "pub global BOX",
            "global BOX",
        ),
        (
            "let q = perim",
            "perimeter",
            "pub fn perimeter",
            "fn perimeter",
        ),
    ] {
        let project = V1Project::new();
        let mut bone = project.open();
        let main = project.path("main.rock");
        let text = with_completion(typed, MAIN_ANCHOR);
        bone.update(&main, text.clone());
        assert_offers(
            &labels(&complete_at(&mut bone, &main, &text, typed)),
            &[label],
        );
        bone.update(
            &project.path("shapes.rock"),
            SHAPES.replace(public, private),
        );
        assert_lacks(
            &labels(&complete_at(&mut bone, &main, &text, typed)),
            &[label],
        );
    }
}

#[test]
fn changing_v1_import_alias_invalidates_bound_completion_cache() {
    let (_project, mut bone, main) = opened();
    let text = with_completion("let q = surf", MAIN_ANCHOR);
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
    let surf = find(&after, "surf");
    assert_eq!(surf.detail.as_deref(), Some("fn surf(w: int) -> int"));
}

#[test]
fn aliased_callable_completion_carries_signature_detail() {
    let items = complete_in_main("let q = surf");
    let surface = find(&items, "surface");
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
    let (_project, mut bone, main) = opened();
    let text = with_completion("let q = LIM", MAIN_ANCHOR);
    bone.update(&main, text.clone());
    let marker = "let q = LIM";
    let before = complete_at(&mut bone, &main, &text, marker);
    let limit = find(&before, "LIMIT");
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
    let after = complete_at(&mut bone, &main, &body_changed, marker);
    assert_eq!(
        after, before,
        "body-only edit changed global completion details"
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
    let text = MAIN
        .replace(
            "use shapes::*;",
            "use shapes::*;\nuse shapes::family as fam;",
        )
        .replace(MAIN_ANCHOR, &format!("{MAIN_ANCHOR}    let q = fam\n"));
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
    let edit = bone
        .rename(path, pos, new_name)
        .expect("no refusal")
        .expect("renameable");
    let mut out = Vec::new();
    for (uri, edits) in edit.changes.unwrap_or_default() {
        let file = uri.to_file_path().unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        for e in edits {
            let r = e.range;
            assert_eq!(r.start.line, r.end.line);
            out.push((
                file_name(&file).to_string(),
                r.start.line,
                covered(&text, r).to_string(),
                e.new_text,
            ));
        }
    }
    out.sort();
    out
}

#[test]
fn renaming_declarations_updates_all_references() {
    let project = V1Project::new();
    let bone = project.open();
    let cases: &[(&str, &str, &str, &str, &str, &[(&str, u32)])] = &[
        (
            "`surface` remains an alias",
            "shapes.rock",
            SHAPES,
            "area",
            "size",
            &[("main.rock", 0), ("shapes.rock", 2)],
        ),
        (
            "qualified paths and re-exports",
            "shapes_inner.rock",
            INNER,
            "depth",
            "level",
            &[
                ("facade.rock", 2),
                ("main.rock", 5),
                ("main.rock", 8),
                ("shapes_inner.rock", 2),
            ],
        ),
        (
            "glob-imported bare uses",
            "shapes.rock",
            SHAPES,
            "perimeter",
            "outline",
            &[("main.rock", 7), ("shapes.rock", 6)],
        ),
    ];

    for (case, source_file, source, needle, new_name, expected) in cases {
        let path = project.path(source_file);
        let edits = rename_edits(&bone, &path, pos_of(source, needle, 0, 1), new_name);
        let want: Vec<_> = expected
            .iter()
            .map(|(file, line)| {
                (
                    file.to_string(),
                    *line,
                    needle.to_string(),
                    new_name.to_string(),
                )
            })
            .collect();
        assert_eq!(edits, want, "{case}");
    }
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
            .expect("no refusal")
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
    assert_no_errors(&diags);
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
    assert_no_errors(&diags);
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
    let (_project, mut bone, main) = opened();
    let head = "use shapes::area as surface;\nuse shapes::*;\nuse shapes::Box as Crate;\n";
    let sorted = "use shapes::*;\nuse shapes::Box as Crate;\nuse shapes::area as surface;\nuse shapes::perimeter;\n";
    assert!(MAIN.contains(head));
    let text = MAIN
        .replacen(head, sorted, 1)
        .replace(MAIN_ANCHOR, &format!("{MAIN_ANCHOR}    let q = dept\n"));
    bone.update(&main, text.clone());
    let items = bone.completion(
        &main,
        pos_of(&text, "let q = dept", 0, "let q = dept".len()),
    );
    let depth = find(&items, "depth");
    let edits = depth
        .additional_text_edits
        .as_ref()
        .expect("an unimported name carries its import");
    assert_eq!(edits[0].new_text, "use shapes::inner::depth;\n");
    // Before `use shapes::perimeter;`, the first `use` that sorts after it.
    assert_eq!(edits[0].range.start, Position::new(3, 0));
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

// -- inlay hints --------------------------------------------------------------

fn inlay_labels(bone: &BoneWorkspace, path: &str) -> Vec<String> {
    let all = lsp_types::Range::new(Position::new(0, 0), Position::new(1000, 0));
    bone.inlay_hints(path, all)
        .into_iter()
        .map(|h| match h.label {
            lsp_types::InlayHintLabel::String(s) => s,
        })
        .collect()
}

/// A v1 file that enters the session with a hard tokenizer error has no live
/// module, so inlay hints come from the recovery reparse of the buffer. That
/// reparse must read the file as its package does: read as standalone it lost its
/// `pub mod` header and `use` items, and `area` typed as `<error>`. The
/// control breaks the same file recoverably, so the session itself answers.
#[test]
fn recovery_reparse_reads_a_v1_buffer_as_its_package_does() {
    let head = "pub mod extra;\n\nuse super::shapes::area;\n\npub fn more() -> int {\n    let n = area(2)\n    n\n}\n\nfn broken() {\n";
    for (tail, live) in [("    let q = (1\n", true), ("    let q = \"abc\n", false)] {
        let project = V1Project::new();
        let mut bone = project.open();
        let text = format!("{head}{tail}");
        std::fs::write(project.dir.path().join("extra.rock"), &text).unwrap();
        let extra = project.path("extra.rock");
        let diags = bone.update(&extra, text);
        assert!(
            !errors_in(&diags, &extra).is_empty(),
            "the buffer is broken"
        );
        let labels = inlay_labels(&bone, &extra);
        assert_eq!(
            labels.first().map(String::as_str),
            Some(": int"),
            "`let n = area(2)` (live session: {live}): {labels:?}"
        );
    }
}

/// `use p::operator(+);` imports the `+` family under its internal name. Its
/// glyph segment lands on the operator, as a named use item's leaf lands on
/// what it names, and so does an alias of it.
#[test]
fn an_operator_use_item_segment_lands_on_the_operator() {
    let project = V1Project::new();
    let shapes_text =
        format!("{SHAPES}\npub operator +(a: Box, b: Box) -> int {{\n    a.w + b.w\n}}\n");
    std::fs::write(project.dir.path().join("shapes.rock"), &shapes_text).unwrap();
    let main_text = MAIN.replace(
        "use shapes::*;\n",
        "use shapes::*;\nuse shapes::operator(+);\nuse shapes::operator(+) as plus;\n",
    );
    std::fs::write(project.dir.path().join("main.rock"), &main_text).unwrap();
    let mut bone = BoneWorkspace::new();
    let main = project.path("main.rock");
    let diags = bone.update(&main, main_text.clone());
    assert_eq!(errors_in(&diags, &main), Vec::<String>::new());
    for (needle, into) in [("operator(+);", 9), ("operator(+) as", 9), ("plus;", 1)] {
        let loc = bone
            .definition(&main, pos_of(&main_text, needle, 0, into))
            .unwrap_or_else(|| panic!("no definition at `{needle}` +{into}"));
        let (path, covered) = located_text(&loc);
        assert_eq!(file_name(&path), "shapes.rock");
        assert_eq!(covered, "+", "`{needle}` +{into}");
    }
    assert_eq!(
        definition_line(&bone, &main, &main_text, "shapes::operator(+);", 0, 1),
        ("shapes.rock".into(), 0)
    );
}

#[test]
fn renaming_a_function_stops_at_its_own_definition_and_the_stdlib() {
    const GET_MAIN: &str = "\
fn main() {
    let a = shapes::get(1)
    let b = other::get(2)
    print(\"{a} {b}\")
}
";
    const GET_SHAPES: &str = "pub mod shapes;\n\npub fn get(w: int) -> int {\n    w\n}\n";
    const GET_OTHER: &str = "pub mod other;\n\npub fn get(w: int) -> int {\n    w + 1\n}\n";
    let project = V1Project::new();
    let dir = project.dir.path();
    std::fs::write(dir.join("main.rock"), GET_MAIN).unwrap();
    std::fs::write(dir.join("shapes.rock"), GET_SHAPES).unwrap();
    std::fs::write(dir.join("other.rock"), GET_OTHER).unwrap();
    std::fs::remove_file(dir.join("shapes_inner.rock")).unwrap();
    std::fs::remove_file(dir.join("facade.rock")).unwrap();

    let mut bone = BoneWorkspace::new();
    let diags = bone.update(&project.path("main.rock"), GET_MAIN.to_string());
    assert_no_errors(&diags);
    let shapes = project.path("shapes.rock");
    let edits = rename_edits(&bone, &shapes, pos_of(GET_SHAPES, "get", 0, 1), "fetch");
    let want: Vec<(String, u32, String, String)> = vec![
        ("main.rock".into(), 1, "get".into(), "fetch".into()),
        ("shapes.rock".into(), 2, "get".into(), "fetch".into()),
    ];
    assert_eq!(edits, want);
}
