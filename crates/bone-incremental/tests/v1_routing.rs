//! Which update path a session edit takes in a modules-v1 package, and that
//! the path it takes reaches the same verdict a fresh session would.
//!
//! Only an edit to what the namespace tree is built from -- an item's name,
//! kind or visibility, a `use`, a `mod` header -- needs the full rebuild. A
//! signature, field or payload edit keeps its DefKey and stays on the
//! interface path.

use bone_core::modules::NsAliases;
use bone_incremental::session::{Session, SessionFile, UpdateTier};
use skull_parser::{Dialect, PackageTag};

fn v1(path: &str, text: &str) -> SessionFile {
    SessionFile {
        path: path.into(),
        text: text.into(),
        dialect: Dialect::V1,
        package: Some(PackageTag { id: 1, self_name: "livev1".into() }),
    }
}

fn diagnostics(session: &Session) -> Vec<String> {
    let mut out: Vec<String> = session.diagnostics().iter().map(|d| d.message.clone()).collect();
    out.sort();
    out
}

/// Apply `edit` to `path` in a session over `files`, check the session's
/// diagnostics against a fresh session over the edited texts, and return the
/// tier and those diagnostics.
fn edit_with_diagnostics(files: Vec<SessionFile>, path: &str, edit: &str) -> (UpdateTier, Vec<String>) {
    let mut session = Session::new(vec![], files.clone(), &NsAliases::default()).expect("baseline builds");
    let tier = session.update_file(path, edit).expect("the edit is accepted").tier;

    let edited: Vec<SessionFile> = files
        .into_iter()
        .map(|mut f| {
            if f.path == path {
                f.text = edit.to_string();
            }
            f
        })
        .collect();
    let fresh = Session::new(vec![], edited, &NsAliases::default()).expect("fresh build");
    let diags = diagnostics(&session);
    assert_eq!(diags, diagnostics(&fresh), "incremental and fresh disagree");
    (tier, diags)
}

fn edit_matches_fresh(files: Vec<SessionFile>, path: &str, edit: &str) -> UpdateTier {
    edit_with_diagnostics(files, path, edit).0
}

const A: &str = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
const ROOT: &str = "fn f() -> int {\n    a::bump(1)\n}\n\nfn g(p: a::P) -> int {\n    p.x\n}\n";

fn package() -> Vec<SessionFile> {
    vec![v1("a.rock", A), v1("root.rock", ROOT)]
}

#[test]
fn a_signature_edit_stays_on_the_interface_path() {
    let edit = "mod a;\n\npub fn bump(n: int, m: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
    assert_eq!(edit_matches_fresh(package(), "a.rock", edit), UpdateTier::Interface);
}

#[test]
fn a_field_edit_stays_on_the_interface_path() {
    let edit = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n    pub y: int,\n}\n";
    assert_eq!(edit_matches_fresh(package(), "a.rock", edit), UpdateTier::Interface);
}

#[test]
fn a_field_visibility_edit_stays_on_the_interface_path() {
    let root = "fn set(p: a::P) {\n    p.x = 2\n}\n";
    let edit = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub private(set) x: int,\n}\n";
    let files = vec![v1("a.rock", A), v1("root.rock", root)];
    assert_eq!(edit_matches_fresh(files, "a.rock", edit), UpdateTier::Interface);
}

#[test]
fn an_item_visibility_edit_rebuilds_the_tree() {
    let edit = "mod a;\n\nfn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
    let (tier, diags) = edit_with_diagnostics(package(), "a.rock", edit);
    assert_eq!(tier, UpdateTier::FullUser);
    assert!(diags.iter().any(|d| d.contains("`bump` is not accessible from this module")), "{diags:?}");
}

#[test]
fn an_added_definition_rebuilds_the_tree() {
    let edit = format!("{A}\npub fn twice(n: int) -> int {{\n    n\n}}\n");
    assert_eq!(edit_matches_fresh(package(), "a.rock", &edit), UpdateTier::FullUser);
}

/// The public-API closure (spec 11) is a whole-program check that the full
/// rebuild runs. A signature edit is enough to break it, so the interface
/// path has to run it too.
#[test]
fn a_signature_edit_that_exports_a_private_type_is_refused() {
    let s0 = "mod s;\n\nstruct Hidden {\n    pub x: int,\n}\n\npub fn mk() -> int {\n    1\n}\n";
    let s1 = "mod s;\n\nstruct Hidden {\n    pub x: int,\n}\n\npub fn mk() -> Hidden {\n    Hidden { x: 1 }\n}\n";
    let root = "pub use s::mk;\n";
    let files = |s: &str| vec![v1("s.rock", s), v1("root.rock", root)];
    let expected = "type `Hidden` in public API is not accessible";

    let fresh = Session::new(vec![], files(s1), &NsAliases::default()).err().expect("a fresh build refuses it");
    assert!(fresh.message.contains(expected), "{}", fresh.message);

    let mut session = Session::new(vec![], files(s0), &NsAliases::default()).expect("baseline builds");
    let err = session.update_file("s.rock", s1).expect_err("the session refuses it too");
    assert!(err.message.contains(expected), "{}", err.message);
}

const AMBIGUOUS_BUMP: &str =
    "ambiguous legacy reference to `bump`: legacy definition in legacy_def.rock and v1 definition livev1::a::bump";

/// A legacy file defines `bump` and a v1 package projects `bump` to legacy
/// files. A legacy `bump(..)` is ambiguous, and the verdict is the same in a
/// fresh session and after a body-only edit of the caller. Legacy resolution
/// once checked `ws.functions_by_name`, which is empty while a fresh build
/// lowers bodies and populated during an incremental re-lower, so the fresh
/// build took the projection and the edited session took the legacy `bump`.
#[test]
fn a_legacy_name_that_is_also_projected_is_ambiguous_after_a_body_edit() {
    let a = "mod a;\n\npub fn bump(n: int, m: int) -> int {\n    n\n}\n";
    let root = "pub use a::bump;\n";
    let def = "fn bump(n: int) -> int {\n    n\n}\n";
    let edit = "fn use_it() -> int {\n    bump(2)\n}\n";
    let files = vec![
        v1("a.rock", a),
        v1("root.rock", root),
        SessionFile::legacy("legacy_def.rock", def),
        SessionFile::legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
    ];
    let (tier, diags) = edit_with_diagnostics(files, "legacy_use.rock", edit);
    assert_eq!(tier, UpdateTier::BodyOnly);
    assert!(diags.iter().any(|d| d.contains(AMBIGUOUS_BUMP)), "{diags:?}");
}

/// The same collision introduced or removed by an edit to the legacy DEFINER
/// rather than the caller. The ambiguity is a lowering diagnostic, so the
/// caller has to re-lower: adding a colliding `bump` must report it, and
/// removing it must clear it, exactly as a fresh session does.
#[test]
fn a_legacy_definer_edit_that_changes_a_collision_relowers_its_callers() {
    let a = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
    let without = "fn other(n: int) -> int {\n    n\n}\n";
    let with = "fn other(n: int) -> int {\n    n\n}\n\nfn bump(n: int) -> int {\n    n\n}\n";
    let files = |def: &str| {
        vec![
            v1("a.rock", a),
            v1("root.rock", "pub use a::bump;\n"),
            SessionFile::legacy("legacy_def.rock", def),
            SessionFile::legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
        ]
    };

    let (_, diags) = edit_with_diagnostics(files(without), "legacy_def.rock", with);
    assert!(diags.iter().any(|d| d.contains(AMBIGUOUS_BUMP)), "{diags:?}");

    let (_, diags) = edit_with_diagnostics(files(with), "legacy_def.rock", without);
    assert!(diags.is_empty(), "{diags:?}");
}

/// A glob opens only the members accessible from the importer (spec 8.1),
/// so adding a private definition to a globbed module changes no other
/// module's resolution: `c` still reaches `a::width` through its glob.
#[test]
fn a_private_definition_added_to_a_globbed_module_changes_no_globber() {
    let files = vec![
        v1("a.rock", "mod a;\n\npub fn width() -> string {\n    \"wide\"\n}\n"),
        v1("b.rock", "mod b;\n\npub fn depth() -> int {\n    1\n}\n"),
        v1("c.rock", "mod c;\n\nuse super::a::*;\nuse super::b::*;\n\nfn pick() -> string {\n    width()\n}\n"),
    ];
    let edit = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\nfn width() -> int {\n    2\n}\n";
    let (tier, diags) = edit_with_diagnostics(files, "b.rock", edit);
    assert_eq!(tier, UpdateTier::FullUser);
    assert!(diags.is_empty(), "{diags:?}");
}

/// A legacy file contributes no definitions to the namespace tree, so adding
/// one stays on the interface path it always took.
#[test]
fn a_legacy_added_definition_stays_on_the_interface_path() {
    let files = vec![
        SessionFile::legacy("a.rock", "fn bump(n: int) -> int {\n    n\n}\n"),
        SessionFile::legacy("b.rock", "fn f() -> int {\n    bump(1)\n}\n"),
    ];
    let edit = "fn bump(n: int) -> int {\n    n\n}\n\nfn twice(n: int) -> int {\n    n\n}\n";
    assert_eq!(edit_matches_fresh(files, "a.rock", edit), UpdateTier::Interface);
}
