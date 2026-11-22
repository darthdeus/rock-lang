//! The LSP over a modules-v1 package.

use crate::completion::{assert_invariants, find, labels_of};
use crate::fixture::{Project, assert_no_errors, covered, position_of_offset};
use bone_query::BoneWorkspace;
use bone_query::FileUriExt;
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

    /// A workspace loaded through the editor route (`update` on the root fragment), asserted clean.
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
        let lsp_types::HoverContents::Markup(markup) = &hover.contents else {
            panic!("expected markup hover")
        };
        assert!(
            markup.value.contains("struct app::shapes::Box"),
            "{markup:?}"
        );
    }
    assert_ne!(alias_hover.range, qualified_hover.range);
}

/// Completion items at the end of `typed`, typed as a new line inside
/// `main`'s body (the line after `let a = ...`).
const MAIN_ANCHOR: &str = "    let a = shapes::inner::depth()\n";

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
    // Rebuilding the first file's bodies moves the one-arg overload to the front of the family.
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

    // Keeping the item/signature layout takes the session's BodyOnly rebuild path.
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
fn every_overload_renames_the_import_target_and_keeps_the_alias() {
    let shapes =
        "mod shapes;\npub fn area(n: int) -> int { n * n }\npub fn area(s: string) -> int { 5 }\n";
    let project = Project::new(&[
        (
            "stick.toml",
            "[package]\nname = \"app\"\ntype = \"bin\"\n[language]\nmodules = \"v1\"\n",
        ),
        (
            "main.rock",
            "use shapes::area as surface;\nfn main() { print(surface(\"a\")) }\n",
        ),
        ("shapes.rock", shapes),
    ]);
    let (bone, diags) = project.open("main.rock");
    assert_no_errors(&diags);
    let path = project.path("shapes.rock");
    let wanted = [("main.rock", 0), ("shapes.rock", 1), ("shapes.rock", 2)].map(|(file, line)| {
        (
            file.to_string(),
            line,
            "area".to_string(),
            "volume".to_string(),
        )
    });
    for overload in 0..2 {
        assert_eq!(
            rename_edits(&bone, &path, pos_of(shapes, "area", overload, 1), "volume"),
            wanted
        );
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

fn inlay_labels(bone: &BoneWorkspace, path: &str) -> Vec<String> {
    let all = lsp_types::Range::new(Position::new(0, 0), Position::new(1000, 0));
    bone.inlay_hints(path, all)
        .into_iter()
        .map(|h| match h.label {
            lsp_types::InlayHintLabel::String(s) => s,
            _ => panic!("expected a string inlay label"),
        })
        .collect()
}

/// A v1 buffer with a hard tokenizer error gets inlay hints from a reparse read as its package.
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

/// The glyph segment of `use p::operator(+);`, and an alias of it, land on the operator.
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
