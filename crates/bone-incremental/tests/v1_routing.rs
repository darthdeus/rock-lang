//! Which update path a session edit takes in a modules-v1 package, and that the path it takes
//! reaches the same verdict a fresh session would. Only an edit to what the namespace tree is
//! built from -- an item's name, kind or visibility, a `use`, a `mod` header -- takes the scope
//! path, which re-assembles the tree.

use crate::common::{self, in_package, package_tag, v1};
use crate::v1_differential::V1Harness;

use bone_core::modules::NsAliases;
use bone_incremental::session::{Session, SessionFile, UpdateTier};
use skull_parser::GlobalPolicy;

fn diagnostics(session: &Session) -> Vec<String> {
    let mut out: Vec<String> = session
        .diagnostics()
        .iter()
        .map(|d| d.message.clone())
        .collect();
    out.sort();
    out
}

fn edit_with_aliases(
    files: Vec<SessionFile>,
    path: &str,
    edit: &str,
    aliases: &NsAliases,
) -> (UpdateTier, Vec<String>) {
    let mut h = V1Harness::with_aliases(files, aliases.clone());
    let tier = h.edit(path, edit).expect("the edit is accepted").tier;
    (tier, h.diagnostics())
}

fn edit_with_diagnostics(
    files: Vec<SessionFile>,
    path: &str,
    edit: &str,
) -> (UpdateTier, Vec<String>) {
    edit_with_aliases(files, path, edit, &NsAliases::default())
}

const A: &str =
    "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
const ROOT: &str = "fn f() -> int {\n    a::bump(1)\n}\n\nfn g(p: a::P) -> int {\n    p.x\n}\n";

fn package() -> Vec<SessionFile> {
    vec![v1("a.rock", A), v1("root.rock", ROOT)]
}

#[test]
fn declaration_edits_take_the_matching_tier() {
    for (root, edit, tier, diagnostic) in [
        (
            ROOT,
            A.replace("n: int)", "n: int, m: int)"),
            UpdateTier::Interface,
            None,
        ),
        (
            ROOT,
            A.replace("pub x: int,", "pub x: int,\n    pub y: int,"),
            UpdateTier::Interface,
            None,
        ),
        (
            "fn set(p: a::P) {\n    p.x = 2\n}\n",
            A.replace("pub x:", "pub private(set) x:"),
            UpdateTier::Interface,
            None,
        ),
        (
            ROOT,
            A.replace("pub fn bump", "fn bump"),
            UpdateTier::Scope,
            Some("`bump` is not accessible from this module"),
        ),
        (
            ROOT,
            format!("{A}\npub fn twice(n: int) -> int {{\n    n\n}}\n"),
            UpdateTier::Scope,
            None,
        ),
    ] {
        let (actual, diags) = edit_with_diagnostics(
            vec![v1("a.rock", A), v1("root.rock", root)],
            "a.rock",
            &edit,
        );
        assert_eq!(actual, tier, "{edit}");
        if let Some(needle) = diagnostic {
            assert!(diags.iter().any(|d| d.contains(needle)), "{diags:?}");
        }
    }
}

#[test]
fn a_same_signature_definition_in_another_file_of_the_module_is_a_duplicate() {
    let other = |body: &str| v1("a2.rock", &format!("mod a;\n\n{body}"));
    let files = |body: &str| {
        let mut files = package();
        files.push(other(body));
        files
    };
    let duplicate = "fn bump(m: int) -> float {\n    1.0\n}\n";
    let overload = "fn bump(m: float) -> float {\n    m\n}\n";

    let (_, diags) = edit_with_diagnostics(
        files(overload),
        "a2.rock",
        &format!("mod a;\n\n{duplicate}"),
    );
    assert!(
        diags
            .iter()
            .any(|d| d.contains("duplicate definition of `bump(int)`: already defined at a.rock:3")),
        "{diags:?}"
    );

    let (_, diags) = edit_with_diagnostics(
        files(duplicate),
        "a2.rock",
        &format!("mod a;\n\n{overload}"),
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_private_definition_added_to_a_globbed_module_changes_no_globber() {
    let files = vec![
        v1(
            "a.rock",
            "mod a;\n\npub fn width() -> string {\n    \"wide\"\n}\n",
        ),
        v1("b.rock", "mod b;\n\npub fn depth() -> int {\n    1\n}\n"),
        v1(
            "c.rock",
            "mod c;\n\nuse super::a::*;\nuse super::b::*;\n\nfn pick() -> string {\n    width()\n}\n",
        ),
    ];
    let edit = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\nfn width() -> int {\n    2\n}\n";
    let (tier, diags) = edit_with_diagnostics(files, "b.rock", edit);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");
}

const PICK_INT: &str = "pub fn pick(n: int) -> int {\n    n\n}\n";
const PICK_STRING: &str = "pub fn pick(s: string) -> string {\n    s\n}\n";
const PICK_USE: &str = "fn use_it() -> int {\n    let s = pick(\"v1\")\n    pick(1)\n}\n";

fn aliases_of(files: &[SessionFile]) -> NsAliases {
    let mut packages: Vec<(u32, String)> = files
        .iter()
        .filter_map(|f| f.package.as_ref().map(|p| (p.id, p.self_name.clone())))
        .collect();
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

const IMPORT_A: &str = "use lib_a::pick;\n\n";
const IMPORT_BOTH: &str = "use lib_a::pick;\nuse lib_b::pick;\n\n";

#[test]
fn a_second_import_composes_and_removing_it_splits_the_family() {
    for (one, both) in [(IMPORT_A, IMPORT_BOTH), (GLOB_A, GLOB_BOTH)] {
        let files = |imports: &str| {
            vec![
                in_package(1, "lib_a", "a.rock", PICK_INT),
                in_package(2, "lib_b", "b.rock", PICK_STRING),
                in_package(3, "game", "use.rock", &format!("{imports}{PICK_USE}")),
            ]
        };
        let aliases = aliases_of(&files(""));
        for (before, after, clean) in [(one, both, true), (both, one, false)] {
            let edit = format!("{after}{PICK_USE}");
            let (tier, diags) = edit_with_aliases(files(before), "use.rock", &edit, &aliases);
            assert_eq!(tier, UpdateTier::Scope);
            if clean {
                assert!(diags.is_empty(), "{diags:?}");
            } else {
                assert!(
                    diags.iter().any(|d| d.contains("type mismatch")),
                    "{diags:?}"
                );
            }
        }
    }
}

#[test]
fn a_member_signature_edit_ties_a_composed_import_family() {
    const TIED_PICK: &str = "Ambiguous overload for `pick` with argument types (string)\n\
         tied candidates (equally-ranked matches):\n  \
         lib_a::pick(s: string) -> string (v1 definition lib_a::pick)\n  \
         lib_b::pick(s: string) -> string (v1 definition lib_b::pick)";
    let files = |b: &str| {
        vec![
            in_package(1, "lib_a", "a.rock", PICK_STRING),
            in_package(2, "lib_b", "b.rock", b),
            in_package(3, "game", "use.rock", &format!("{IMPORT_BOTH}{PICK_USE}")),
        ]
    };
    let aliases = aliases_of(&files(PICK_INT));

    let (tier, diags) = edit_with_aliases(files(PICK_INT), "b.rock", PICK_STRING, &aliases);
    assert_eq!(tier, UpdateTier::Interface);
    assert!(diags.iter().any(|d| d.contains(TIED_PICK)), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(PICK_STRING), "b.rock", PICK_INT, &aliases);
    assert_eq!(tier, UpdateTier::Interface);
    assert!(diags.is_empty(), "{diags:?}");
}

const GLOB_A: &str = "use lib_a::*;\n\n";
const GLOB_BOTH: &str = "use lib_a::*;\nuse lib_b::*;\n\n";

#[test]
fn a_member_added_to_a_second_globbed_module_relowers_the_globber() {
    let other = "pub fn other() -> int {\n    1\n}\n";
    let files = |b: &str| {
        vec![
            in_package(1, "lib_a", "a.rock", PICK_INT),
            in_package(2, "lib_b", "b.rock", b),
            in_package(3, "game", "use.rock", &format!("{GLOB_BOTH}{PICK_USE}")),
        ]
    };
    let aliases = aliases_of(&files(other));
    let relowers_the_globber = |b0: &str, b1: &str| {
        let mut session = Session::new(vec![], files(b0), &aliases).expect("baseline builds");
        let before = diagnostics(&session);
        let tier = session
            .update_file("b.rock", b1)
            .expect("the edit is accepted")
            .tier;
        let relowered = session
            .workspace()
            .relowered_body_count
            .load(std::sync::atomic::Ordering::Relaxed);
        (tier, before, relowered)
    };

    let (tier, before, relowered) = relowers_the_globber(other, PICK_STRING);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(
        before.iter().any(|d| d.contains("type mismatch")),
        "{before:?}"
    );
    // b.rock's `pick` and use.rock's `use_it`.
    assert_eq!(relowered, 2);
    let (_, diags) = edit_with_aliases(files(other), "b.rock", PICK_STRING, &aliases);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, before, relowered) = relowers_the_globber(PICK_STRING, other);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(relowered, 2);
    let (_, diags) = edit_with_aliases(files(PICK_STRING), "b.rock", other, &aliases);
    assert!(
        diags.iter().any(|d| d.contains("type mismatch")),
        "{diags:?}"
    );
}

#[test]
fn a_unary_operator_added_to_a_globbed_module_relowers_the_caller() {
    let without = "pub struct Pt {\n    pub x: int,\n}\n";
    let with = "pub struct Pt {\n    pub x: int,\n}\n\npub fn __unm(a: Pt) -> Pt {\n    a\n}\n";
    let caller = "use geo::*;\n\nfn neg() -> int {\n    let p = -Pt { x: 1 }\n    let negative = -p\n    negative.x\n}\n";
    let files = |geo: &str| {
        vec![
            in_package(1, "geo", "geo.rock", geo),
            in_package(2, "app", "app.rock", caller),
        ]
    };
    let aliases = aliases_of(&files(without));

    let (tier, diags) = edit_with_aliases(files(without), "geo.rock", with, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(with), "geo.rock", without, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(
        diags
            .iter()
            .any(|d| d.contains("operator `-` is not defined for `geo::Pt`")),
        "{diags:?}"
    );
}

#[test]
fn a_file_entering_after_new_reads_with_its_declared_facts() {
    let extra = "mod extra;\n\npub fn more() -> int {\n    super::a::bump(2)\n}\n";
    let mut session = common::with_warm(vec![], package());
    session
        .declare_file_facts(
            "extra.rock",
            Some(package_tag(1, "livev1")),
            GlobalPolicy::Any,
        )
        .expect("an unheld file takes declared facts");
    session
        .update_file("extra.rock", extra)
        .expect("a v1 fragment enters as v1");

    let mut files = package();
    files.push(v1("extra.rock", extra));
    let fresh = common::with_warm(vec![], files);
    assert_eq!(diagnostics(&session), diagnostics(&fresh));
    assert_eq!(diagnostics(&session), Vec::<String>::new());
}

#[test]
fn a_held_file_refuses_different_facts() {
    let mut session = common::with_warm(vec![], package());
    assert!(
        session
            .declare_file_facts("a.rock", None, GlobalPolicy::Any)
            .is_err()
    );
    assert!(
        session
            .declare_file_facts("a.rock", Some(package_tag(1, "livev1")), GlobalPolicy::Any)
            .is_ok()
    );
}

#[test]
fn globals_none_policy_survives_initial_and_incremental_parses() {
    for (name, invalid, valid, edit) in [
        (
            "COUNT",
            "global COUNT: int = 0\n\nfn tick() { COUNT = COUNT + 1 }\n",
            "const COUNT: int = 0\n\nfn tick() { print(\"{COUNT}\") }\n",
            "global COUNT: int = 0\n\nfn tick() { COUNT = COUNT + 1 }\n",
        ),
        (
            "host_tick",
            "inline(lua) fn host_tick() = \"return nil\"\nfn tick() { host_tick() }\n",
            "inline(js) fn host_tick() = \"return null;\"\nfn tick() {}\n",
            "inline(lua) fn host_tick() = \"return nil\"\nfn tick() {}\n",
        ),
    ] {
        let file = |text: &str| {
            let mut file = v1("sim.rock", text);
            file.global_policy = GlobalPolicy::None;
            file
        };
        let initial = Session::new(vec![], vec![file(invalid)], &NsAliases::default())
            .expect_err("the owning policy rejects the initial source");
        let mut session = common::with_warm(vec![], vec![file(valid)]);
        let update = session
            .update_file("sim.rock", edit)
            .expect_err("the reparse retains the owning policy");
        for error in [initial, update] {
            assert!(
                error.message.contains(name) && error.message.contains("globals = \"none\""),
                "{error:?}"
            );
        }
    }
}

#[test]
fn a_dependency_module_alias_added_or_removed_takes_the_scope_path() {
    let lib = "pub mod sub;\n";
    let sub = "mod sub;\n\npub fn pick(n: int) -> int {\n    n\n}\n";
    let without = "fn use_it() -> int {\n    s::pick(1)\n}\n";
    let with = "use lib_a::sub as s;\n\nfn use_it() -> int {\n    s::pick(1)\n}\n";
    let files = |user: &str| {
        vec![
            in_package(1, "lib_a", "lib.rock", lib),
            in_package(1, "lib_a", "sub.rock", sub),
            in_package(2, "game", "use.rock", user),
        ]
    };
    let aliases = aliases_of(&files(without));

    let (tier, diags) = edit_with_aliases(files(without), "use.rock", with, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(with), "use.rock", without, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(
        diags.iter().any(|d| d.contains("did not resolve")),
        "{diags:?}"
    );
}
