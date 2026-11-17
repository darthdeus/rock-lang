//! Which update path a session edit takes in a modules-v1 package, and that
//! the path it takes reaches the same verdict a fresh session would.
//!
//! Only an edit to what the namespace tree is built from -- an item's name,
//! kind or visibility, a `use`, a `mod` header -- takes the scope path, which
//! re-assembles the tree. A signature, field or payload edit keeps
//! its DefKey and stays on the interface path.

use crate::common::{in_package, package_tag, v1};
use crate::v1_differential::V1Harness;

use bone_core::modules::NsAliases;
use bone_incremental::session::{Session, SessionFile, UpdateTier};
use skull_parser::{Dialect, GlobalPolicy};

fn diagnostics(session: &Session) -> Vec<String> {
    let mut out: Vec<String> = session
        .diagnostics()
        .iter()
        .map(|d| d.message.clone())
        .collect();
    out.sort();
    out
}

/// Apply `edit` to `path` in a harness over `files` (which checks it against
/// a fresh build) and return the tier and the session's diagnostics.
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

fn edit_matches_fresh(files: Vec<SessionFile>, path: &str, edit: &str) -> UpdateTier {
    edit_with_diagnostics(files, path, edit).0
}

const A: &str =
    "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
const ROOT: &str = "fn f() -> int {\n    a::bump(1)\n}\n\nfn g(p: a::P) -> int {\n    p.x\n}\n";

fn package() -> Vec<SessionFile> {
    vec![v1("a.rock", A), v1("root.rock", ROOT)]
}

#[test]
fn a_signature_edit_stays_on_the_interface_path() {
    let edit = "mod a;\n\npub fn bump(n: int, m: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
    assert_eq!(
        edit_matches_fresh(package(), "a.rock", edit),
        UpdateTier::Interface
    );
}

#[test]
fn a_field_edit_stays_on_the_interface_path() {
    let edit = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n    pub y: int,\n}\n";
    assert_eq!(
        edit_matches_fresh(package(), "a.rock", edit),
        UpdateTier::Interface
    );
}

#[test]
fn a_field_visibility_edit_stays_on_the_interface_path() {
    let root = "fn set(p: a::P) {\n    p.x = 2\n}\n";
    let edit = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub private(set) x: int,\n}\n";
    let files = vec![v1("a.rock", A), v1("root.rock", root)];
    assert_eq!(
        edit_matches_fresh(files, "a.rock", edit),
        UpdateTier::Interface
    );
}

#[test]
fn an_item_visibility_edit_rebuilds_the_tree() {
    let edit =
        "mod a;\n\nfn bump(n: int) -> int {\n    n\n}\n\npub struct P {\n    pub x: int,\n}\n";
    let (tier, diags) = edit_with_diagnostics(package(), "a.rock", edit);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(
        diags
            .iter()
            .any(|d| d.contains("`bump` is not accessible from this module")),
        "{diags:?}"
    );
}

/// Two files of one module share its overload families, so a second file
/// defining `bump` with the same parameter types duplicates it.
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
fn an_added_definition_rebuilds_the_tree() {
    let edit = format!("{A}\npub fn twice(n: int) -> int {{\n    n\n}}\n");
    assert_eq!(
        edit_matches_fresh(package(), "a.rock", &edit),
        UpdateTier::Scope
    );
}

/// A glob opens only the members accessible from the importer,
/// so adding a private definition to a globbed module changes no other
/// module's resolution: `c` still reaches `a::width` through its glob.
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

/// Every package of `files` names every other by its self name.
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

/// Explicit imports of two different callables under one name compose into
/// one family. Adding the
/// second import composes it (a `use` edit: scope path); removing it
/// splits the family again, and `pick("v1")` no longer has a member. Each
/// edit agrees with a fresh session.
#[test]
fn a_second_explicit_import_composes_and_removing_it_splits_the_family() {
    let files = |imports: &str| {
        vec![
            in_package(Dialect::V1, 1, "lib_a", "a.rock", PICK_INT),
            in_package(Dialect::V1, 2, "lib_b", "b.rock", PICK_STRING),
            in_package(
                Dialect::V1,
                3,
                "game",
                "use.rock",
                &format!("{imports}{PICK_USE}"),
            ),
        ]
    };
    let aliases = aliases_of(&files(""));
    let both = format!("{IMPORT_BOTH}{PICK_USE}");
    let one = format!("{IMPORT_A}{PICK_USE}");

    let (tier, diags) = edit_with_aliases(files(IMPORT_A), "use.rock", &both, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(diags.is_empty(), "{diags:?}");

    let (tier, diags) = edit_with_aliases(files(IMPORT_BOTH), "use.rock", &one, &aliases);
    assert_eq!(tier, UpdateTier::Scope);
    assert!(
        diags.iter().any(|d| d.contains("type mismatch")),
        "{diags:?}"
    );
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
    let files = |b: &str| {
        vec![
            in_package(Dialect::V1, 1, "lib_a", "a.rock", PICK_STRING),
            in_package(Dialect::V1, 2, "lib_b", "b.rock", b),
            in_package(
                Dialect::V1,
                3,
                "game",
                "use.rock",
                &format!("{IMPORT_BOTH}{PICK_USE}"),
            ),
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

/// Glob candidates of two different callables compose into one family when
/// no higher tier binds the name. Adding
/// the second glob composes it (a `use` edit: scope path); removing it
/// splits the family again, and `pick("v1")` no longer has a member. Each
/// edit agrees with a fresh session.
#[test]
fn a_second_glob_composes_and_removing_it_splits_the_family() {
    let files = |imports: &str| {
        vec![
            in_package(Dialect::V1, 1, "lib_a", "a.rock", PICK_INT),
            in_package(Dialect::V1, 2, "lib_b", "b.rock", PICK_STRING),
            in_package(
                Dialect::V1,
                3,
                "game",
                "use.rock",
                &format!("{imports}{PICK_USE}"),
            ),
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
    assert!(
        diags.iter().any(|d| d.contains("type mismatch")),
        "{diags:?}"
    );
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
            in_package(Dialect::V1, 1, "lib_a", "a.rock", PICK_INT),
            in_package(Dialect::V1, 2, "lib_b", "b.rock", b),
            in_package(
                Dialect::V1,
                3,
                "game",
                "use.rock",
                &format!("{GLOB_BOTH}{PICK_USE}"),
            ),
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

/// Unary `-x` reads a family lowering decided, so a globbed module gaining
/// or losing `__unm` re-lowers the caller on the scope path, even when the
/// caller's lookup found nothing before, as a fresh session decides.
#[test]
fn a_unary_operator_added_to_a_globbed_module_relowers_the_caller() {
    let without = "pub struct Pt {\n    pub x: int,\n}\n";
    let with = "pub struct Pt {\n    pub x: int,\n}\n\npub fn __unm(a: Pt) -> Pt {\n    a\n}\n";
    let caller = "use geo::*;\n\nfn neg() -> int {\n    let p = -Pt { x: 1 }\n    let negative = -p\n    negative.x\n}\n";
    let files = |geo: &str| {
        vec![
            in_package(Dialect::V1, 1, "geo", "geo.rock", geo),
            in_package(Dialect::V1, 2, "app", "app.rock", caller),
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

/// A file that enters after `Session::new` reads with the facts its host
/// declared for it, and reaches the verdict a fresh session over the same
/// files reaches. Undeclared, it parses as a standalone v1 source.
#[test]
fn a_file_entering_after_new_reads_with_its_declared_facts() {
    let extra = "mod extra;\n\npub fn more() -> int {\n    super::a::bump(2)\n}\n";
    let mut session =
        Session::new(vec![], package(), &NsAliases::default()).expect("baseline builds");
    session
        .declare_file_facts(
            "extra.rock",
            Dialect::V1,
            Some(package_tag(1, "livev1")),
            GlobalPolicy::Any,
        )
        .expect("an unheld file takes declared facts");
    session
        .update_file("extra.rock", extra)
        .expect("a v1 fragment enters as v1");

    let mut files = package();
    files.push(v1("extra.rock", extra));
    let fresh = Session::new(vec![], files, &NsAliases::default()).expect("fresh build");
    assert_eq!(diagnostics(&session), diagnostics(&fresh));
    assert_eq!(diagnostics(&session), Vec::<String>::new());
}

/// A held file's facts come from its manifest and cannot change under it.
#[test]
fn a_held_file_refuses_different_facts() {
    let mut session =
        Session::new(vec![], package(), &NsAliases::default()).expect("baseline builds");
    assert!(
        session
            .declare_file_facts("a.rock", Dialect::V1, None, GlobalPolicy::Any)
            .is_err()
    );
    assert!(
        session
            .declare_file_facts(
                "a.rock",
                Dialect::V1,
                Some(package_tag(1, "livev1")),
                GlobalPolicy::Any
            )
            .is_ok()
    );
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
    let mut session = Session::new(vec![], vec![valid_file], &NsAliases::default())
        .expect("const session builds");
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
            in_package(Dialect::V1, 1, "lib_a", "lib.rock", lib),
            in_package(Dialect::V1, 1, "lib_a", "sub.rock", sub),
            in_package(Dialect::V1, 2, "game", "use.rock", user),
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
