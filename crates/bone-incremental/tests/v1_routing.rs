//! Namespace edits in a modules-v1 package must match a fresh build.

use crate::common::{self, in_package, package_tag, v1};
use crate::v1_differential::V1Harness;

use bone_core::modules::NsAliases;
use bone_incremental::session::{Session, SessionFile};
use skull_parser::GlobalPolicy;

fn edit_with_aliases(
    files: Vec<SessionFile>,
    path: &str,
    edit: &str,
    aliases: &NsAliases,
) -> Vec<String> {
    let mut h = V1Harness::with_aliases(files, aliases.clone());
    h.edit(path, edit).expect("the edit is accepted");
    h.diagnostics()
}

const A: &str =
    "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
const ROOT: &str = "fn f() -> int {\n    a::bump(1)\n}\n\nfn g(p: a::P) -> int {\n    p.x\n}\n";

fn package() -> Vec<SessionFile> {
    vec![v1("a.rock", A), v1("root.rock", ROOT)]
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

const IMPORT_BOTH: &str = "use lib_a::pick;\nuse lib_b::pick;\n\n";

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

    let diags = edit_with_aliases(files(PICK_INT), "b.rock", PICK_STRING, &aliases);
    assert!(diags.iter().any(|d| d.contains(TIED_PICK)), "{diags:?}");

    let diags = edit_with_aliases(files(PICK_STRING), "b.rock", PICK_INT, &aliases);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_unary_operator_added_to_a_globbed_module_resolves_then_removal_fails() {
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

    let diags = edit_with_aliases(files(without), "geo.rock", with, &aliases);
    assert!(diags.is_empty(), "{diags:?}");

    let diags = edit_with_aliases(files(with), "geo.rock", without, &aliases);
    assert!(
        diags
            .iter()
            .any(|d| d.contains("operator `-` is not defined for `geo::Pt`")),
        "{diags:?}"
    );
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
fn callable_only_edits_leave_unrelated_interface_errors_in_place() {
    let broken = "fn broken(x: Missing) {}\nfn valid() -> int { 7 }\n";
    let before = "fn callable(x: int) -> int { 1 }\n";
    let after = before.replace("x: int", "x: string");
    let mut session = common::with_warm(
        vec![],
        vec![v1("broken.rock", broken), v1("callable.rock", before)],
    );
    assert!(
        session
            .diagnostics()
            .iter()
            .any(|error| error.message.contains("Missing"))
    );
    let update = session.update_file("callable.rock", &after).unwrap();
    assert_eq!(
        update.tier,
        bone_incremental::session::UpdateTier::Interface
    );
    assert!(
        update
            .relowered
            .iter()
            .all(|(mid, _)| { session.workspace().module(*mid).file.as_str() != "broken.rock" }),
        "{update:?}"
    );
    session
        .update_file("callable.rock", &format!("{after}\nstruct Missing {{}}"))
        .unwrap();
    assert!(session.diagnostics().is_empty());
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

    let diags = edit_with_aliases(files(without), "use.rock", with, &aliases);
    assert!(diags.is_empty(), "{diags:?}");

    let diags = edit_with_aliases(files(with), "use.rock", without, &aliases);
    assert!(
        diags.iter().any(|d| d.contains("did not resolve")),
        "{diags:?}"
    );
}
