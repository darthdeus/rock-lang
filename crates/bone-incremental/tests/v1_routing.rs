//! Which update path a session edit takes in a modules-v1 package, and that
//! the path it takes reaches the same verdict a fresh session would.
//!
//! Only an edit to what the namespace tree is built from -- an item's name,
//! kind or visibility, a `use`, a `mod` header -- takes the scope path, which
//! re-assembles the tree (plan 2.6). A signature, field or payload edit keeps
//! its DefKey and stays on the interface path.

use bone_core::modules::NsAliases;
use bone_incremental::session::{Session, SessionFile, UpdateTier};
use skull_parser::{Dialect, GlobalPolicy, PackageTag};

fn v1(path: &str, text: &str) -> SessionFile {
    SessionFile {
        path: path.into(),
        text: text.into(),
        dialect: Dialect::V1,
        package: Some(PackageTag { id: 1, self_name: "livev1".into(), identity_storage: Default::default() }),
        global_policy: GlobalPolicy::Any,
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
    edit_with_aliases(files, path, edit, &NsAliases::default())
}

/// `edit_with_diagnostics` under an explicit package alias table.
fn edit_with_aliases(
    files: Vec<SessionFile>,
    path: &str,
    edit: &str,
    aliases: &NsAliases,
) -> (UpdateTier, Vec<String>) {
    let mut session = Session::new(vec![], files.clone(), aliases).expect("baseline builds");
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
    let fresh = Session::new(vec![], edited, aliases).expect("fresh build");
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
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.iter().any(|d| d.contains("`bump` is not accessible from this module")), "{diags:?}");
}

#[test]
fn an_added_definition_rebuilds_the_tree() {
    let edit = format!("{A}\npub fn twice(n: int) -> int {{\n    n\n}}\n");
    assert_eq!(edit_matches_fresh(package(), "a.rock", &edit), UpdateTier::Scope);
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

/// A legacy `bump(int)` and the projected v1 `bump(int)` have the same
/// signature: one merged family, and overload selection ties at the call,
/// naming both origins -- what two same-signature legacy definitions do.
const TIED_BUMP: &str = "Ambiguous overload for `bump` with argument types (int)\n\
     tied candidates (equally-ranked matches):\n  \
     bump(n: int) -> int (legacy definition in legacy_def.rock)\n  \
     livev1::a::bump(n: int) -> int (v1 definition livev1::a::bump)";

/// A legacy file defines `bump(int)` and a v1 package projects
/// `bump(int, int)` to legacy files: one overload family (spec 3.2), so both
/// arities resolve, and the verdict is the same in a fresh session and after a
/// body-only edit of the caller. Legacy resolution once checked
/// `ws.functions_by_name`, which is empty while a fresh build lowers bodies
/// and populated during an incremental re-lower, so the two disagreed.
#[test]
fn a_legacy_and_projected_family_merges_after_a_body_edit() {
    let a = "mod a;\n\npub fn bump(n: int, m: int) -> int {\n    n\n}\n";
    let root = "pub use a::bump;\n";
    let def = "fn bump(n: int) -> int {\n    n\n}\n";
    let edit = "fn use_it() -> int {\n    bump(2, 3)\n}\n";
    let files = vec![
        v1("a.rock", a),
        v1("root.rock", root),
        SessionFile::legacy("legacy_def.rock", def),
        SessionFile::legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
    ];
    let (tier, diags) = edit_with_diagnostics(files, "legacy_use.rock", edit);
    assert_eq!(tier, UpdateTier::BodyOnly);
    assert!(diags.is_empty(), "{diags:?}");
}

/// The same family changed by an edit to the legacy DEFINER rather than the
/// caller. Whether a legacy name is a merged family is decided when the
/// caller LOWERS, so the caller has to re-lower and re-infer: adding a
/// same-signature `bump` must report the tie, and removing it must clear it,
/// exactly as a fresh session does.
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
    assert!(diags.iter().any(|d| d.contains(TIED_BUMP)), "{diags:?}");

    let (_, diags) = edit_with_diagnostics(files(with), "legacy_def.rock", without);
    assert!(diags.is_empty(), "{diags:?}");
}

/// A legacy definer that keeps the name but changes its KIND: a global
/// `bump` collides with the projected callable (ambiguous), a function `bump`
/// merges with it. The name set is unchanged, so only the kind tells the
/// session the callers must re-lower.
#[test]
fn a_legacy_definer_kind_edit_relowers_its_callers() {
    let a = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
    let global = "global bump: string = \"g\"\n";
    let function = "fn bump(n: string) -> string {\n    n\n}\n";
    let files = |def: &str| {
        vec![
            v1("a.rock", a),
            v1("root.rock", "pub use a::bump;\n"),
            SessionFile::legacy("legacy_def.rock", def),
            SessionFile::legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
        ]
    };

    let (_, diags) = edit_with_diagnostics(files(global), "legacy_def.rock", function);
    assert!(diags.is_empty(), "{diags:?}");

    let (_, diags) = edit_with_diagnostics(files(function), "legacy_def.rock", global);
    assert!(
        diags.iter().any(|d| d.contains("ambiguous legacy reference to `bump`")),
        "{diags:?}"
    );
}

/// A signature edit of the v1 member stays on the interface path, and the
/// legacy caller of the merged family depends on that member's DefKey as well
/// as on the bare name: narrowing `bump(int, int)` to `bump(int)` makes the
/// caller's `bump(1)` tie, and widening it back clears the tie.
#[test]
fn a_v1_member_signature_edit_reinfers_a_merged_family_caller() {
    let wide = "mod a;\n\npub fn bump(n: int, m: int) -> int {\n    n\n}\n";
    let narrow = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
    let files = |a: &str| {
        vec![
            v1("a.rock", a),
            v1("root.rock", "pub use a::bump;\n"),
            SessionFile::legacy("legacy_def.rock", "fn bump(n: int) -> int {\n    n\n}\n"),
            SessionFile::legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
        ]
    };

    let (tier, diags) = edit_with_diagnostics(files(wide), "a.rock", narrow);
    assert_eq!(tier, UpdateTier::Interface);
    assert!(diags.iter().any(|d| d.contains(TIED_BUMP)), "{diags:?}");

    let (tier, diags) = edit_with_diagnostics(files(narrow), "a.rock", wide);
    assert_eq!(tier, UpdateTier::Interface);
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
    assert_eq!(tier, UpdateTier::Scope);
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

fn v1_in(package: u32, name: &str, path: &str, text: &str) -> SessionFile {
    SessionFile {
        path: path.into(),
        text: text.into(),
        dialect: Dialect::V1,
        package: Some(PackageTag { id: package, self_name: name.into(), identity_storage: Default::default() }),
        global_policy: GlobalPolicy::Any,
    }
}

const PICK_INT: &str = "pub fn pick(n: int) -> int {\n    n\n}\n";
const PICK_STRING: &str = "pub fn pick(s: string) -> string {\n    s\n}\n";
const PICK_USE: &str = "fn use_it() -> int {\n    let s = pick(\"v1\")\n    pick(1)\n}\n";

/// Two v1 packages each project `pick` to a package-less legacy file that
/// defines no `pick`: one overload family (spec 3.2), as legacy overloading
/// merges them. Narrowing one member to the other's signature ties the call,
/// naming both origins; widening it back clears the tie. Both edits stay on
/// the interface path and agree with a fresh session.
#[test]
fn projected_callables_with_no_legacy_owner_merge_across_a_member_edit() {
    const TIED_PICK: &str = "Ambiguous overload for `pick` with argument types (string)\n\
         tied candidates (equally-ranked matches):\n  \
         lib_a::pick(s: string) -> string (v1 definition lib_a::pick)\n  \
         lib_b::pick(s: string) -> string (v1 definition lib_b::pick)";
    let files = |b: &str| {
        vec![
            v1_in(1, "lib_a", "a.rock", PICK_STRING),
            v1_in(2, "lib_b", "b.rock", b),
            SessionFile::legacy("legacy_use.rock", PICK_USE),
        ]
    };

    let (tier, diags) = edit_with_diagnostics(files(PICK_INT), "b.rock", PICK_STRING);
    assert_eq!(tier, UpdateTier::Interface);
    assert!(diags.iter().any(|d| d.contains(TIED_PICK)), "{diags:?}");

    let (tier, diags) = edit_with_diagnostics(files(PICK_STRING), "b.rock", PICK_INT);
    assert_eq!(tier, UpdateTier::Interface);
    assert!(diags.is_empty(), "{diags:?}");
}

/// A second package's `pick` appearing turns the single projected definition
/// into a family; removing it turns the family back. Each is a namespace
/// edit (scope path: the caller looked `pick` up, so it re-lowers), and the
/// caller's verdict matches a fresh session either way.
#[test]
fn a_second_projected_callable_added_or_removed_relowers_its_callers() {
    let other = "pub fn other() -> int {\n    1\n}\n";
    let files = |b: &str| {
        vec![
            v1_in(1, "lib_a", "a.rock", PICK_INT),
            v1_in(2, "lib_b", "b.rock", b),
            SessionFile::legacy("legacy_use.rock", PICK_USE),
        ]
    };

    let (tier, diags) = edit_with_diagnostics(files(other), "b.rock", PICK_STRING);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_diagnostics(files(PICK_STRING), "b.rock", other);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.iter().any(|d| d.contains("type mismatch")), "{diags:?}");
}

/// A legacy definer adding a `pick` to a family that had no legacy member,
/// and removing it again. The family stays a family, but its member set
/// changes, so the caller re-lowers and re-infers: a same-signature legacy
/// `pick(int)` ties with `lib_a::pick`.
#[test]
fn a_legacy_member_added_to_an_ownerless_family_relowers_its_callers() {
    let without = "fn other(n: int) -> int {\n    n\n}\n";
    let with = "fn other(n: int) -> int {\n    n\n}\n\nfn pick(n: int) -> int {\n    n\n}\n";
    let files = |def: &str| {
        vec![
            v1_in(1, "lib_a", "a.rock", PICK_INT),
            v1_in(2, "lib_b", "b.rock", PICK_STRING),
            SessionFile::legacy("legacy_def.rock", def),
            SessionFile::legacy("legacy_use.rock", PICK_USE),
        ]
    };

    let (_, diags) = edit_with_diagnostics(files(without), "legacy_def.rock", with);
    assert!(
        diags.iter().any(|d| d.contains("Ambiguous overload for `pick` with argument types (int)")
            && d.contains("(legacy definition in legacy_def.rock)")
            && d.contains("(v1 definition lib_a::pick)")),
        "{diags:?}"
    );

    let (_, diags) = edit_with_diagnostics(files(with), "legacy_def.rock", without);
    assert!(diags.is_empty(), "{diags:?}");
}

/// Legacy operator syntax reaches a projected v1 `operator +` (spec 3.2,
/// 8.5), through `+` and `+=` alike, after a body edit of the caller; and a
/// legacy definer adding a same-signature `operator +` ties both, as a fresh
/// session does.
#[test]
fn legacy_operator_syntax_merges_a_projected_operator_across_edits() {
    let geo = "pub struct Pt {\n    pub x: int,\n}\n\npub operator +(a: Pt, b: Pt) -> Pt {\n    a\n}\n";
    let caller = "fn use_it() -> int {\n    let p = Pt { x: 1 } + Pt { x: 2 }\n    p.x\n}\n";
    let edit = "fn use_it() -> int {\n    let p = Pt { x: 1 }\n    p += Pt { x: 2 }\n    p.x\n}\n";
    let without = "fn other(n: int) -> int {\n    n\n}\n";
    let with = "fn other(n: int) -> int {\n    n\n}\n\noperator +(a: Pt, b: Pt) -> Pt {\n    b\n}\n";
    let files = |def: &str, use_text: &str| {
        vec![
            v1_in(1, "geo", "geo.rock", geo),
            SessionFile::legacy("legacy_def.rock", def),
            SessionFile::legacy("legacy_use.rock", use_text),
        ]
    };

    let (tier, diags) = edit_with_diagnostics(files(without, caller), "legacy_use.rock", edit);
    assert_eq!(tier, UpdateTier::BodyOnly);
    assert!(diags.is_empty(), "{diags:?}");

    let (_, diags) = edit_with_diagnostics(files(without, edit), "legacy_def.rock", with);
    assert!(
        diags.iter().any(|d| d.contains("Ambiguous overload for `__op_add`")
            && d.contains("(legacy definition in legacy_def.rock)")
            && d.contains("(v1 definition geo::__op_add)")),
        "{diags:?}"
    );

    let (_, diags) = edit_with_diagnostics(files(with, edit), "legacy_def.rock", without);
    assert!(diags.is_empty(), "{diags:?}");
}

/// Every package of `files` names every other by its self name.
fn aliases_of(files: &[SessionFile]) -> NsAliases {
    let mut packages: Vec<(u32, String)> =
        files.iter().filter_map(|f| f.package.as_ref().map(|p| (p.id, p.self_name.clone()))).collect();
    packages.sort();
    packages.dedup();
    NsAliases {
        packages: packages
            .iter()
            .map(|(id, name)| bone_core::modules::PackageAliases {
                id: *id,
                self_name: name.as_str().into(),
                aliases: packages
                    .iter()
                    .filter(|(other, _)| other != id)
                    .map(|(other, other_name)| (other_name.as_str().into(), *other))
                    .collect(),
            })
            .collect(),
    }
}

fn in_package(dialect: Dialect, package: u32, name: &str, path: &str, text: &str) -> SessionFile {
    SessionFile {
        path: path.into(),
        text: text.into(),
        dialect,
        package: Some(PackageTag { id: package, self_name: name.into(), identity_storage: Default::default() }),
        global_policy: GlobalPolicy::Any,
    }
}

const IMPORT_A: &str = "use lib_a::pick;\n\n";
const IMPORT_BOTH: &str = "use lib_a::pick;\nuse lib_b::pick;\n\n";

/// Explicit imports of two different callables under one name compose into
/// one family (spec 8.3), in a v1 and in a legacy file alike. Adding the
/// second import composes it (a `use` edit: scope path); removing it
/// splits the family again, and `pick("v1")` no longer has a member. Each
/// edit agrees with a fresh session.
#[test]
fn a_second_explicit_import_composes_and_removing_it_splits_the_family() {
    for dialect in [Dialect::V1, Dialect::Legacy] {
        let files = |imports: &str| {
            vec![
                v1_in(1, "lib_a", "a.rock", PICK_INT),
                v1_in(2, "lib_b", "b.rock", PICK_STRING),
                in_package(dialect, 3, "game", "use.rock", &format!("{imports}{PICK_USE}")),
            ]
        };
        let aliases = aliases_of(&files(""));
        let both = format!("{IMPORT_BOTH}{PICK_USE}");
        let one = format!("{IMPORT_A}{PICK_USE}");

        let (tier, diags) = edit_with_aliases(files(IMPORT_A), "use.rock", &both, &aliases);
        assert_eq!(tier, UpdateTier::Scope, "{dialect:?}");
        assert!(diags.is_empty(), "{dialect:?}: {diags:?}");

        let (tier, diags) = edit_with_aliases(files(IMPORT_BOTH), "use.rock", &one, &aliases);
        assert_eq!(tier, UpdateTier::Scope, "{dialect:?}");
        assert!(diags.iter().any(|d| d.contains("type mismatch")), "{dialect:?}: {diags:?}");
    }
}

/// A member of a composed import family narrowed to the other member's
/// signature ties the call, naming both origins, and widening it back clears
/// the tie -- on the interface path, as a fresh session decides.
#[test]
fn a_member_signature_edit_ties_a_composed_import_family() {
    const TIED_PICK: &str = "Ambiguous overload for `pick` with argument types (string)\n\
         tied candidates (equally-ranked matches):\n  \
         lib_a::pick(s: string) -> string (v1 definition lib_a::pick)\n  \
         lib_b::pick(s: string) -> string (v1 definition lib_b::pick)";
    for dialect in [Dialect::V1, Dialect::Legacy] {
        let files = |b: &str| {
            vec![
                v1_in(1, "lib_a", "a.rock", PICK_STRING),
                v1_in(2, "lib_b", "b.rock", b),
                in_package(dialect, 3, "game", "use.rock", &format!("{IMPORT_BOTH}{PICK_USE}")),
            ]
        };
        let aliases = aliases_of(&files(PICK_INT));

        let (tier, diags) = edit_with_aliases(files(PICK_INT), "b.rock", PICK_STRING, &aliases);
        assert_eq!(tier, UpdateTier::Interface, "{dialect:?}");
        assert!(diags.iter().any(|d| d.contains(TIED_PICK)), "{dialect:?}: {diags:?}");

        let (tier, diags) = edit_with_aliases(files(PICK_STRING), "b.rock", PICK_INT, &aliases);
        assert_eq!(tier, UpdateTier::Interface, "{dialect:?}");
        assert!(diags.is_empty(), "{dialect:?}: {diags:?}");
    }
}

const GLOB_A: &str = "use lib_a::*;\n\n";
const GLOB_BOTH: &str = "use lib_a::*;\nuse lib_b::*;\n\n";

/// Glob candidates of two different callables compose into one family when
/// no higher tier binds the name (spec 8.3, author decision: merge). Adding
/// the second glob composes it (a `use` edit: scope path); removing it
/// splits the family again, and `pick("v1")` no longer has a member. Each
/// edit agrees with a fresh session.
#[test]
fn a_second_glob_composes_and_removing_it_splits_the_family() {
    let files = |imports: &str| {
        vec![
            v1_in(1, "lib_a", "a.rock", PICK_INT),
            v1_in(2, "lib_b", "b.rock", PICK_STRING),
            v1_in(3, "game", "use.rock", &format!("{imports}{PICK_USE}")),
        ]
    };
    let aliases = aliases_of(&files(""));
    let both = format!("{GLOB_BOTH}{PICK_USE}");
    let one = format!("{GLOB_A}{PICK_USE}");

    let (tier, diags) = edit_with_aliases(files(GLOB_A), "use.rock", &both, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(GLOB_BOTH), "use.rock", &one, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.iter().any(|d| d.contains("type mismatch")), "{diags:?}");
}

/// A globbed module gaining or losing a `pick` is an edit to ANOTHER file:
/// the globber's recorded lookup of `pick` is dirty, so the scope path
/// re-lowers it and its family gains or loses the member, as a fresh
/// session decides.
#[test]
fn a_member_added_to_a_second_globbed_module_relowers_the_globber() {
    let other = "pub fn other() -> int {\n    1\n}\n";
    let files = |b: &str| {
        vec![
            v1_in(1, "lib_a", "a.rock", PICK_INT),
            v1_in(2, "lib_b", "b.rock", b),
            v1_in(3, "game", "use.rock", &format!("{GLOB_BOTH}{PICK_USE}")),
        ]
    };
    let aliases = aliases_of(&files(other));
    let relowers_the_globber = |b0: &str, b1: &str| {
        let mut session = Session::new(vec![], files(b0), &aliases).expect("baseline builds");
        let before = diagnostics(&session);
        let tier = session.update_file("b.rock", b1).expect("the edit is accepted").tier;
        let relowered =
            session.workspace().relowered_body_count.load(std::sync::atomic::Ordering::Relaxed);
        (tier, before, relowered)
    };

    let (tier, before, relowered) = relowers_the_globber(other, PICK_STRING);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(before.iter().any(|d| d.contains("type mismatch")), "{before:?}");
    // b.rock's `pick` and use.rock's `use_it`.
    assert_eq!(relowered, 2);
    let (_, diags) = edit_with_aliases(files(other), "b.rock", PICK_STRING, &aliases);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, before, relowered) = relowers_the_globber(PICK_STRING, other);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(relowered, 2);
    let (_, diags) = edit_with_aliases(files(PICK_STRING), "b.rock", other, &aliases);
    assert!(diags.iter().any(|d| d.contains("type mismatch")), "{diags:?}");
}

/// Unary `-x` reads a family lowering decided, so a globbed module gaining
/// or losing `__unm` re-lowers the caller on the scope path, even when the
/// caller's lookup found nothing before, as a fresh session decides.
#[test]
fn a_unary_operator_added_to_a_globbed_module_relowers_the_caller() {
    let without = "pub struct Pt {\n    pub x: int,\n}\n";
    let with = "pub struct Pt {\n    pub x: int,\n}\n\npub fn __unm(a: Pt) -> Pt {\n    a\n}\n";
    let caller = "use geo::*;\n\nfn neg() -> int {\n    let p = -Pt { x: 1 }\n    let negative = -p\n    negative.x\n}\n";
    let files = |geo: &str| vec![v1_in(1, "geo", "geo.rock", geo), v1_in(2, "app", "app.rock", caller)];
    let aliases = aliases_of(&files(without));

    let (tier, diags) = edit_with_aliases(files(without), "geo.rock", with, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(with), "geo.rock", without, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.iter().any(|d| d.contains("operator `-` is not defined for `geo::Pt`")), "{diags:?}");
}

/// A file that enters after `Session::new` reads with the facts its host
/// declared for it, and reaches the verdict a fresh session over the same
/// files reaches. Undeclared, it would parse as a standalone legacy source.
#[test]
fn a_file_entering_after_new_reads_with_its_declared_facts() {
    let extra = "mod extra;\n\npub fn more() -> int {\n    super::a::bump(2)\n}\n";
    let mut session = Session::new(vec![], package(), &NsAliases::default()).expect("baseline builds");
    session
        .declare_file_facts("extra.rock", Dialect::V1, Some(PackageTag { id: 1, self_name: "livev1".into(), identity_storage: Default::default() }), GlobalPolicy::Any)
        .expect("an unheld file takes declared facts");
    session.update_file("extra.rock", extra).expect("a v1 fragment enters as v1");

    let mut files = package();
    files.push(v1("extra.rock", extra));
    let fresh = Session::new(vec![], files, &NsAliases::default()).expect("fresh build");
    assert_eq!(diagnostics(&session), diagnostics(&fresh));
    assert_eq!(diagnostics(&session), Vec::<String>::new());
}

/// A held file's facts come from its manifest and cannot change under it.
#[test]
fn a_held_file_refuses_different_facts() {
    let mut session = Session::new(vec![], package(), &NsAliases::default()).expect("baseline builds");
    assert!(session.declare_file_facts("a.rock", Dialect::Legacy, None, GlobalPolicy::Any).is_err());
    assert!(session
        .declare_file_facts("a.rock", Dialect::V1, Some(PackageTag { id: 1, self_name: "livev1".into(), identity_storage: Default::default() }), GlobalPolicy::Any)
        .is_ok());
}

/// The incremental session re-parses from `SessionFile` facts rather than the
/// batch `SourceFile`. A `globals = "none"` package must therefore reject its
/// mutable global on the initial load and keep the same policy when the file
/// is edited into an accepted `const` declaration.
#[test]
fn globals_none_fact_reaches_incremental_reparse_and_update() {
    let mut invalid_file = v1(
        "sim.rock",
        "global COUNT: int = 0\n\nfn tick() { COUNT = COUNT + 1 }\n",
    );
    invalid_file.global_policy = GlobalPolicy::None;
    let initial_error = Session::new(vec![], vec![invalid_file], &NsAliases::default())
        .expect_err("the initial mutable global must be rejected");
    assert!(
        initial_error.message.contains("COUNT")
            && initial_error.message.contains("globals = \"none\""),
        "the owning package policy must reject the initial mutable global: {initial_error:?}"
    );

    let mut valid_file = v1(
        "sim.rock",
        "const COUNT: int = 0\n\nfn tick() { print(\"{COUNT}\") }\n",
    );
    valid_file.global_policy = GlobalPolicy::None;
    let mut session = Session::new(vec![], vec![valid_file], &NsAliases::default()).expect("const session builds");
    let update_error = session
        .update_file(
            "sim.rock",
            "global COUNT: int = 0\n\nfn tick() { COUNT = COUNT + 1 }\n",
        )
        .expect_err("the same package policy must reject a mutable update");
    assert!(
        update_error.message.contains("COUNT")
            && update_error.message.contains("globals = \"none\""),
        "the reparse must retain the owning policy: {update_error:?}"
    );
}

/// The same package fact gates opaque Lua bodies on both the initial check and
/// a later edit. JavaScript inline bodies are a separate backend and remain a
/// valid control.
#[test]
fn globals_none_fact_reaches_inline_lua_reparse_and_update() {
    let mut invalid_file = v1(
        "sim.rock",
        "inline(lua) fn host_tick() = \"return nil\"\nfn tick() { host_tick() }\n",
    );
    invalid_file.global_policy = GlobalPolicy::None;
    let initial_error = Session::new(vec![], vec![invalid_file], &NsAliases::default())
        .expect_err("the initial inline Lua body must be rejected");
    assert!(
        initial_error.message.contains("host_tick")
            && initial_error.message.contains("globals = \"none\""),
        "the owning package policy must reject the opaque body: {initial_error:?}"
    );

    let mut valid_file = v1(
        "sim.rock",
        "inline(js) fn host_tick() = \"return null;\"\nfn tick() {}\n",
    );
    valid_file.global_policy = GlobalPolicy::None;
    let mut session = Session::new(vec![], vec![valid_file], &NsAliases::default())
        .expect("the separate backend control must be accepted");
    let update_error = session
        .update_file(
            "sim.rock",
            "inline(lua) fn host_tick() = \"return nil\"\nfn tick() {}\n",
        )
        .expect_err("the same package policy must reject a Lua edit");
    assert!(
        update_error.message.contains("host_tick")
            && update_error.message.contains("globals = \"none\""),
        "the reparse must retain the owning policy: {update_error:?}"
    );
}

/// A module alias of a dependency's module is a `use` edit (scope path):
/// adding it anchors `s::pick`, and removing it leaves the path unresolved,
/// each as a fresh session decides.
#[test]
fn a_dependency_module_alias_added_or_removed_takes_the_scope_path() {
    let lib = "pub mod sub;\n";
    let sub = "mod sub;\n\npub fn pick(n: int) -> int {\n    n\n}\n";
    let without = "fn use_it() -> int {\n    s::pick(1)\n}\n";
    let with = "use lib_a::sub as s;\n\nfn use_it() -> int {\n    s::pick(1)\n}\n";
    let files = |user: &str| {
        vec![
            v1_in(1, "lib_a", "lib.rock", lib),
            v1_in(1, "lib_a", "sub.rock", sub),
            v1_in(2, "game", "use.rock", user),
        ]
    };
    let aliases = aliases_of(&files(without));

    let (tier, diags) = edit_with_aliases(files(without), "use.rock", with, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(with), "use.rock", without, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.iter().any(|d| d.contains("did not resolve")), "{diags:?}");
}

/// A v1 package reaches a legacy dependency through the view of what the
/// provider's own files define (spec 3.2), so a legacy definer edit that
/// adds or removes a name is a namespace edit: the scope path re-lowers the
/// v1 users that looked the name up, as a fresh session decides.
#[test]
fn a_legacy_definition_added_or_removed_relowers_its_v1_users() {
    let without = "fn other() -> int {\n    1\n}\n";
    let with = "fn other() -> int {\n    1\n}\n\nfn pick(n: int) -> int {\n    n\n}\n";
    let by_path = "fn use_it() -> int {\n    x_lib::pick(1)\n}\n";
    let by_glob = "use x_lib::*;\n\nfn use_it() -> int {\n    pick(1)\n}\n";
    for user in [by_path, by_glob] {
        let files = |lib: &str| {
            vec![in_package(Dialect::Legacy, 1, "x_lib", "xlib.rock", lib), v1_in(2, "app", "use.rock", user)]
        };
        let aliases = aliases_of(&files(without));

        let (tier, diags) = edit_with_aliases(files(without), "xlib.rock", with, &aliases);
        assert_eq!(tier, UpdateTier::Scope, "{user}");
        assert!(diags.is_empty(), "{user}: {diags:?}");

        let (tier, diags) = edit_with_aliases(files(with), "xlib.rock", without, &aliases);
        assert_eq!(tier, UpdateTier::Scope, "{user}");
        assert!(diags.iter().any(|d| d.contains("did not resolve")), "{user}: {diags:?}");
    }
}

/// A legacy provider with no v1 dependent has no view, so its definer edits
/// keep the legacy routing.
#[test]
fn a_legacy_definition_added_with_no_v1_dependent_stays_on_the_interface_path() {
    let without = "fn other() -> int {\n    1\n}\n";
    let with = "fn other() -> int {\n    1\n}\n\nfn pick(n: int) -> int {\n    n\n}\n";
    let user = "fn use_it() -> int {\n    other()\n}\n";
    let files = |lib: &str| {
        vec![
            in_package(Dialect::Legacy, 1, "x_lib", "xlib.rock", lib),
            in_package(Dialect::Legacy, 2, "app", "use.rock", user),
        ]
    };
    let aliases = aliases_of(&files(without));
    let (tier, diags) = edit_with_aliases(files(without), "xlib.rock", with, &aliases);
    assert_eq!(tier, UpdateTier::Interface);
    assert!(diags.is_empty(), "{diags:?}");
}
