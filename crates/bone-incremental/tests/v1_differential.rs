//! Modules-v1 incremental == fresh, compared on what resolution decided.

use crate::common::{self, PACKAGE, v1};

use std::collections::{BTreeMap, BTreeSet};

use bone_core::modules::NsAliases;
use bone_incremental::session::{Session, SessionFile};
use bone_semantic::hir::Expr;
use bone_semantic::prelude::*;
use skull_parser::Ast;

fn warm_path() -> String {
    rock_stdlib::stdlib_file_id("epoch_warm")
}
const WARM: &str = "fn warm_one() -> int {\n    1\n}\n\nfn warm_two() -> int {\n    2\n}\n";

fn warm() -> Vec<Ast> {
    vec![skull_parser::parse_module(WARM, Ustr::from(warm_path().as_str())).expect("warm parses")]
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    diagnostics: Vec<(String, usize, String)>,
    calls: BTreeSet<(String, usize, usize, String)>,
    selected_calls: BTreeSet<(String, usize, usize, String)>,
    types: BTreeMap<String, Vec<Option<String>>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Built(Snapshot),
    Refused(String),
}

fn snapshot(ws: &Workspace, files: &[SessionFile]) -> Snapshot {
    let user: BTreeSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut diagnostics: Vec<(String, usize, String)> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten()),
        )
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| {
            (
                e.location.file.to_string(),
                e.location.offset,
                e.message.clone(),
            )
        })
        .collect();
    diagnostics.sort();

    let mut calls = BTreeSet::new();
    let mut selected_calls = BTreeSet::new();
    for module in ws.modules.iter().filter(|m| user.contains(m.file.as_str())) {
        let file = module.file.to_string();
        for body in module.bodies.values() {
            for (idx, expr) in body.expr_arena.iter() {
                let Some(key) = call_key(expr) else {
                    continue;
                };
                let (s, e) = body.expr_spans[idx].offset_range();
                calls.insert((file.clone(), s, e, key.to_string()));
            }
        }
        if let Some(inf) = ws.inference(module.id) {
            for (owner, bi) in inf.per_fn.iter() {
                let Some(body) = module.bodies.get(owner) else {
                    continue;
                };
                for (call, (tmid, fid)) in bi.resolved_overload_calls.iter() {
                    let target = ws.module(*tmid);
                    let f = &target.functions[*fid];
                    let (s, e) = body.expr_spans[*call].offset_range();
                    let selected = format!(
                        "{}::{}@{}",
                        target.file,
                        f.name,
                        f.name_span.offset_range().0
                    );
                    selected_calls.insert((file.clone(), s, e, selected));
                }
            }
        }
    }

    let types = files
        .iter()
        .map(|f| {
            (
                f.path.clone(),
                common::type_profile(ws, &f.path, &f.text, 1),
            )
        })
        .collect();

    Snapshot {
        diagnostics,
        calls,
        selected_calls,
        types,
    }
}

fn call_key(expr: &Expr) -> Option<Ustr> {
    match expr {
        Expr::Call { family, .. } | Expr::SafeNavCall { family, .. } => Some(family.key()),
        _ => None,
    }
}

fn parse(files: &[SessionFile]) -> Result<Vec<Ast>, CompilerError> {
    files
        .iter()
        .map(|f| {
            let mut ast = skull_parser::parse_module(&f.text, Ustr::from(f.path.as_str()))?;
            ast.package = f.package.clone();
            Ok(ast)
        })
        .collect()
}

fn fresh(files: &[SessionFile]) -> Outcome {
    fresh_with(files, &NsAliases::default())
}

fn fresh_with(files: &[SessionFile], aliases: &NsAliases) -> Outcome {
    let session = Session::new(warm(), files.to_vec(), aliases);
    let batch = parse(files).and_then(|asts| {
        let mut modules = warm();
        modules.extend(asts);
        semantic_pass_with_aliases(&mut modules, aliases)
    });
    let outcome = |result: Result<Workspace, CompilerError>| match result {
        Ok(ws) => Outcome::Built(snapshot(&ws, files)),
        Err(error) => Outcome::Refused(error.message),
    };
    let session = outcome(session.map(|s| s.ws));
    assert_eq!(session, outcome(batch), "Session::new and batch disagree");
    session
}

pub(crate) struct V1Harness {
    pub(crate) session: Session,
    files: Vec<SessionFile>,
    aliases: NsAliases,
}

impl V1Harness {
    #[track_caller]
    pub(crate) fn new(files: Vec<SessionFile>) -> Self {
        Self::with_aliases(files, NsAliases::default())
    }

    #[track_caller]
    pub(crate) fn with_aliases(files: Vec<SessionFile>, aliases: NsAliases) -> Self {
        let session = Session::new(warm(), files.clone(), &aliases).expect("baseline builds");
        let h = V1Harness {
            session,
            files,
            aliases,
        };
        h.assert_matches_fresh();
        h
    }

    #[track_caller]
    pub(crate) fn edit(&mut self, path: &str, text: &str) -> Result<(), String> {
        let mut edited = self.files.clone();
        edited
            .iter_mut()
            .find(|f| f.path == path)
            .expect("edits a tracked file")
            .text = text.to_string();
        match self.session.update_file(path, text) {
            Ok(_) => {
                self.files = edited;
                self.assert_matches_fresh();
                Ok(())
            }
            Err(e) => {
                assert_eq!(
                    fresh_with(&edited, &self.aliases),
                    Outcome::Refused(e.message.clone()),
                    "the session refused an edit a fresh build does not refuse the same way"
                );
                self.assert_matches_fresh();
                Err(e.message)
            }
        }
    }

    #[track_caller]
    fn assert_matches_fresh(&self) {
        assert_eq!(
            Outcome::Built(snapshot(self.session.workspace(), &self.files)),
            fresh_with(&self.files, &self.aliases),
            "incremental != fresh: diagnostics, calls or types",
        );
    }

    fn assert_call(&self, path: &str, target: &str) {
        assert!(
            self.call_targets(path).contains(&key(target)),
            "{path} did not resolve to {target}: {:?}",
            self.call_targets(path)
        );
    }

    fn call_targets(&self, path: &str) -> Vec<String> {
        snapshot(self.session.workspace(), &self.files)
            .calls
            .into_iter()
            .filter(|r| r.0 == path)
            .map(|r| r.3)
            .collect()
    }

    #[track_caller]
    fn assert_no_diagnostics(&self) {
        common::assert_clean(&self.session);
    }

    #[track_caller]
    fn assert_diagnostic(&self, needle: &str) {
        let diags = self.diagnostics();
        assert!(
            diags.iter().any(|d| d.contains(needle)),
            "no diagnostic contains {needle:?}: {diags:?}"
        );
    }

    pub(crate) fn diagnostics(&self) -> Vec<String> {
        self.session
            .diagnostics()
            .iter()
            .map(|d| d.message.clone())
            .collect()
    }

    fn selected(&self, path: &str) -> Vec<String> {
        snapshot(self.session.workspace(), &self.files)
            .selected_calls
            .into_iter()
            .filter(|r| r.0 == path)
            .map(|r| {
                let (_, rest) = r.3.split_once("::").expect("file::name@offset");
                rest.split('@').next().expect("name").to_string()
            })
            .collect()
    }
}

fn key(path: &str) -> String {
    format!("{PACKAGE}::{path}")
}

const A_BUMP: &str = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
const B_BUMP: &str = "mod b;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
const UNRELATED: &str =
    "mod d;\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n";

#[test]
fn comparator_sees_a_resolution_that_moves_between_same_typed_definitions() {
    let case = |target: &str| {
        vec![
            v1("a.rock", A_BUMP),
            v1("b.rock", B_BUMP),
            v1(
                "c.rock",
                &format!(
                    "mod c;\n\nuse super::{target}::bump;\n\nfn pick() -> int {{\n    bump(1)\n}}\n"
                ),
            ),
        ]
    };
    let (Outcome::Built(to_a), Outcome::Built(to_b)) = (fresh(&case("a")), fresh(&case("b")))
    else {
        panic!("both build");
    };
    assert_eq!(to_a.diagnostics, to_b.diagnostics);
    assert_eq!(to_a.types, to_b.types);
    assert_ne!(
        to_a.calls, to_b.calls,
        "call target must move with the import"
    );
    assert!(to_a.calls.iter().any(|call| call.3 == key("a::bump")));
    assert!(to_b.calls.iter().any(|call| call.3 == key("b::bump")));
}

#[test]
fn a_refused_build_is_a_comparable_outcome() {
    let s0 = "mod s;\n\nstruct Hidden {\n    pub x: int,\n}\n\npub fn mk() -> int {\n    1\n}\n";
    let s1 = "mod s;\n\nstruct Hidden {\n    pub x: int,\n}\n\npub fn mk() -> Hidden {\n    Hidden { x: 1 }\n}\n";
    let mut h = V1Harness::new(vec![v1("s.rock", s0), v1("root.rock", "pub use s::mk;\n")]);
    let err = h
        .edit("s.rock", s1)
        .expect_err("the API closure refuses it");
    assert!(
        err.contains("type `Hidden` in public API is not accessible"),
        "{err}"
    );
}

#[test]
fn gate_a_b_use_added_to_and_removed_from_one_fragment() {
    let c0 = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let c1 = "mod c;\n\nuse super::b::*;\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("b.rock", B_BUMP),
        v1("c.rock", c0),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_call("c.rock", "b::bump");

    h.edit("c.rock", c1).expect("import is accepted");
    assert_eq!(h.selected("c.rock"), vec![key("a::bump")]);

    h.edit("c.rock", c0).expect("removing import is accepted");
    h.assert_call("c.rock", "b::bump");
}

#[test]
fn gate_c_glob_member_added_and_removed() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 =
        "mod b;\n\npub fn depth() -> int {\n    1\n}\n\npub fn warm_one() -> int {\n    5\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_no_diagnostics();
    h.edit("b.rock", b1).expect("glob member is added");
    h.assert_no_diagnostics();
    h.edit("b.rock", b0).expect("glob member is removed");
    h.assert_no_diagnostics();
}

#[test]
fn a_lower_tier_overload_added_joins_the_callers_family() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 =
        "mod b;\n\npub fn depth() -> int {\n    1\n}\n\npub fn bump(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn bump(s: string) -> string {\n    s\n}\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_diagnostic("type mismatch");

    h.edit("b.rock", b1).expect("glob member is added");
    h.assert_no_diagnostics();
    assert_eq!(h.selected("c.rock"), vec![key("b::bump")]);

    h.edit("b.rock", b0).expect("glob member is removed");
    h.assert_call("c.rock", "c::bump");
    h.assert_diagnostic("type mismatch");
}

#[test]
fn gate_d_private_helper_added_in_an_unrelated_module() {
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let d1 = "mod d;\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n\nfn helper() -> int {\n    4\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);

    h.edit("d.rock", d1).expect("private helper is added");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "a::bump");
}

#[test]
fn gate_e_module_header_changed_and_restored() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod bee;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::b::depth()\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_call("c.rock", "b::depth");

    h.edit("b.rock", b1).expect("module header changes");
    h.assert_diagnostic("did not resolve");

    h.edit("b.rock", b0).expect("module header is restored");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "b::depth");
}

#[test]
fn gate_f_reexport_edit_reaches_a_globber_through_a_glob_chain() {
    let facade0 = "mod facade;\n\npub use super::a::bump;\n";
    let facade1 = "mod facade;\n\npub use super::b::bump;\n";
    let hub = "mod hub;\n\npub use super::facade::*;\n";
    let user = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("b.rock", B_BUMP),
        v1("facade.rock", facade0),
        v1("hub.rock", hub),
        v1("user.rock", user),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_no_diagnostics();
    h.assert_call("user.rock", "a::bump");

    h.edit("facade.rock", facade1)
        .expect("reexport target changes");
    h.assert_call("user.rock", "b::bump");
}

#[test]
fn a_glob_reexport_of_a_named_reexport_resolves() {
    let facade = "mod facade;\n\npub use super::a::bump;\n";
    let hub = "mod hub;\n\npub use super::facade::*;\n";
    let by_path = "mod user;\n\nfn pick() -> int {\n    super::hub::bump(1)\n}\n";
    let by_glob = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let direct = "mod user;\n\nuse super::facade::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let build = |user: &str| {
        V1Harness::new(vec![
            v1("a.rock", A_BUMP),
            v1("facade.rock", facade),
            v1("hub.rock", hub),
            v1("user.rock", user),
        ])
    };
    for user in [direct, by_path, by_glob] {
        let h = build(user);
        h.assert_no_diagnostics();
        h.assert_call("user.rock", "a::bump");
    }
}

#[test]
fn gate_g_visibility_narrowed_and_widened_on_a_path_used_item() {
    let a1 = "mod a;\n\nfn bump(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_no_diagnostics();

    h.edit("a.rock", a1).expect("visibility is narrowed");
    h.assert_diagnostic("`bump` is not accessible from this module");

    h.edit("a.rock", A_BUMP).expect("visibility is restored");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "a::bump");
}

#[test]
fn a_private_member_added_to_a_globbed_module_keeps_the_std_answer() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\nfn warm_one() -> string {\n    \"shadow\"\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);

    h.assert_no_diagnostics();
    h.edit("b.rock", b1).expect("private glob member is added");
    h.assert_no_diagnostics();
}

#[test]
fn overload_member_visibility_edit_rechecks_external_callers() {
    let a0 = "mod a;\n\npub fn f(n: int) -> int {\n    n\n}\n\npub fn f(s: string) -> string {\n    s\n}\n";
    let a1 =
        "mod a;\n\nfn f(n: int) -> int {\n    n\n}\n\npub fn f(s: string) -> string {\n    s\n}\n";
    let c = "mod c;\n\nuse super::a::f;\n\nfn pick() -> int {\n    f(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", a0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_call("c.rock", "a::f");

    h.edit("a.rock", a1).expect("visibility edit is accepted");
    h.assert_diagnostic("not accessible");
}

fn selected_files(h: &V1Harness, path: &str) -> Vec<String> {
    snapshot(h.session.workspace(), &h.files)
        .selected_calls
        .into_iter()
        .filter(|r| r.0 == path)
        .map(|r| {
            r.3.split_once("::")
                .expect("file::name@offset")
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn a_signature_edit_regroups_backend_variants() {
    let a = "fn now(n: int) -> int {\n    n\n}\n\nfn pick() -> int {\n    now(1)\n}\n";
    let apart = "extern(js) fn now(n: float) -> int\n";
    let paired = "extern(js) fn now(n: int) -> int\n";
    let mut h = V1Harness::new(vec![v1("a.rock", a), v1("b.rock", apart)]);
    h.assert_no_diagnostics();

    for text in [paired, apart] {
        h.edit("b.rock", text).expect("accepted");
        h.assert_no_diagnostics();
        assert_eq!(
            selected_files(&h, "a.rock"),
            vec!["a.rock".to_string()],
            "{text}"
        );
    }
}

#[test]
fn an_added_backend_sibling_joins_the_group() {
    let a = "fn now(n: int) -> int {\n    n\n}\n\nfn pick() -> int {\n    now(1)\n}\n";
    let without = "fn other() -> int {\n    2\n}\n";
    let with = "fn other() -> int {\n    2\n}\n\nextern(js) fn now(n: int) -> int\n";
    let mut h = V1Harness::new(vec![v1("a.rock", a), v1("b.rock", without)]);

    for text in [with, without] {
        h.edit("b.rock", text).expect("accepted");
        h.assert_no_diagnostics();
        assert_eq!(
            selected_files(&h, "a.rock"),
            vec!["a.rock".to_string()],
            "{text}"
        );
    }
}

#[test]
fn a_reexport_added_makes_a_failed_path_resolve() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub use super::a::bump;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::b::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_diagnostic("did not resolve");

    h.edit("b.rock", b1).expect("reexport is added");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "a::bump");

    h.edit("b.rock", b0).expect("reexport is removed");
    h.assert_diagnostic("did not resolve");
}

#[test]
fn a_use_added_makes_an_unresolved_name_resolve() {
    let c0 = "mod c;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let c1 = "mod c;\n\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("c.rock", c0),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_diagnostic("did not resolve");

    h.edit("c.rock", c1).expect("import is added");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "a::bump");
}

#[test]
fn a_definition_renamed_in_one_fragment_moves_its_sibling_fragment() {
    let b1 = "mod b;\n\npub fn helper() -> int {\n    1\n}\n";
    let b1_renamed = "mod b;\n\npub fn helper2() -> int {\n    1\n}\n";
    let b2 = "mod b;\n\nfn user() -> int {\n    helper()\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b1.rock", b1),
        v1("b2.rock", b2),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_call("b2.rock", "b::helper");

    h.edit("b1.rock", b1_renamed)
        .expect("definition is renamed");
    h.assert_diagnostic("`helper` did not resolve");
}

#[test]
fn an_inline_module_added_makes_a_path_into_it_resolve() {
    let d1 = "mod d;\n\npub mod inner {\n    pub fn f() -> int {\n        1\n    }\n}\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::d::inner::f()\n}\n";
    let mut h = V1Harness::new(vec![v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(!h.diagnostics().is_empty());

    h.edit("d.rock", d1).expect("inline module is added");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "d::inner::f");
}

#[test]
fn a_module_visibility_widened_opens_a_path_through_it() {
    let hidden0 = "mod outer::hidden;\n\npub fn f() -> int {\n    1\n}\n";
    let hidden1 = "pub mod outer::hidden;\n\npub fn f() -> int {\n    1\n}\n";
    let outer = "pub mod outer;\n\npub fn g() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::outer::hidden::f()\n}\n";
    let mut h = V1Harness::new(vec![
        v1("outer.rock", outer),
        v1("hidden.rock", hidden0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_diagnostic("module `hidden` is not accessible");

    h.edit("hidden.rock", hidden1)
        .expect("module visibility is widened");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "outer::hidden::f");
}

#[test]
fn a_glob_reexport_added_ties_a_globbers_call() {
    let hub0 = "mod hub;\n\npub use super::a::*;\n";
    let hub1 = "mod hub;\n\npub use super::a::*;\npub use super::b::*;\n";
    let user = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("b.rock", B_BUMP),
        v1("hub.rock", hub0),
        v1("user.rock", user),
    ]);
    h.assert_no_diagnostics();

    h.edit("hub.rock", hub1).expect("glob reexport is added");
    h.assert_diagnostic("Ambiguous overload for `bump`");
}

#[test]
fn a_reexport_chain_target_renamed_is_refused_like_a_fresh_build() {
    let facade = "mod facade;\n\npub use super::a::bump;\n";
    let hub = "mod hub;\n\npub use super::facade::*;\n";
    let user = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let a1 = "mod a;\n\npub fn bumped(n: int) -> int {\n    n\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("facade.rock", facade),
        v1("hub.rock", hub),
        v1("user.rock", user),
    ]);
    let err = h
        .edit("a.rock", a1)
        .expect_err("a dangling `pub use` refuses the build");
    assert!(err.contains("invalid `use` target"), "{err}");
}

#[test]
fn a_type_flavor_change_makes_a_variant_path_resolve() {
    let b0 = "mod b;\n\npub struct E {\n    pub x: int,\n}\n";
    let b1 = "mod b;\n\npub enum E {\n    V,\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    let e = super::b::E::V\n    1\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    assert!(!h.diagnostics().is_empty());

    h.edit("b.rock", b1).expect("type flavor changes");
    h.assert_no_diagnostics();
}

#[test]
fn removing_and_restoring_a_type_updates_its_users() {
    let b0 = "mod b;\n\npub struct T {\n    pub x: int,\n}\n";
    let b1 = "mod b;\n\npub fn keep() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn body() -> int {\n    let t = super::b::T { x: 1 }\n    t.x\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_no_diagnostics();

    h.edit("b.rock", b1).expect("type is removed");
    h.assert_diagnostic("did not resolve");
    h.edit("b.rock", b0).expect("type is restored");
    h.assert_no_diagnostics();

    let sig = "mod c;\n\nfn sig(t: super::b::T) -> int {\n    t.x\n}\n";
    let mut h = V1Harness::new(vec![
        v1("b.rock", b0),
        v1("c.rock", sig),
        v1("d.rock", UNRELATED),
    ]);
    let err = h
        .edit("b.rock", b1)
        .expect_err("a signature path that does not resolve refuses the build");
    assert!(err.contains("did not resolve"), "{err}");
}

#[test]
fn a_function_named_like_a_macro_is_refused_like_a_fresh_build() {
    let macros = "macro twice(e: expr) -> int {\n    e * 2\n}\n";
    let user0 = "fn f() -> int {\n    1\n}\n";
    let user1 = "fn f() -> int {\n    1\n}\n\nfn twice(n: int) -> int {\n    n\n}\n";
    let files = |user: &str| vec![v1("macros.rock", macros), v1("user.rock", user)];
    let expected = "macro `twice` collides with a function named `twice`";

    let fresh = Session::new(warm(), files(user1), &NsAliases::default())
        .expect_err("a fresh session refuses it");
    assert!(fresh.message.contains(expected), "{}", fresh.message);

    let mut session = common::with_warm(warm(), files(user0));
    let err = session
        .update_file("user.rock", user1)
        .expect_err("the session refuses the edit too");
    assert!(err.message.contains(expected), "{}", err.message);
}

#[test]
fn a_bad_macro_argument_label_is_refused_like_a_fresh_build_on_every_path() {
    let macros = "macro twice(e: expr) -> int {\n    e * 2\n}\n";
    let user0 = "fn f() -> int {\n    twice(e: 1)\n}\n";
    let expected = "macro `twice` has no parameter named `x`";
    let edits = [
        ("body edit", user0, "fn f() -> int {\n    twice(x: 2)\n}\n"),
        (
            "signature edit",
            user0,
            "fn f(n: int) -> int {\n    twice(x: n)\n}\n",
        ),
        (
            "item addition",
            "mod user;\n\nfn f() -> int {\n    twice(e: 1)\n}\n",
            "mod user;\n\nfn f() -> int {\n    twice(x: 1)\n}\n\npub fn g() -> int {\n    2\n}\n",
        ),
    ];
    for (edit_kind, user0, user1) in edits {
        let files = |user: &str| vec![v1("macros.rock", macros), v1("user.rock", user)];

        let fresh = Session::new(warm(), files(user1), &NsAliases::default())
            .err()
            .unwrap_or_else(|| panic!("{edit_kind}: a fresh session refuses it"));
        assert!(
            fresh.message.contains(expected),
            "{edit_kind}: {}",
            fresh.message
        );

        let mut session = common::with_warm(warm(), files(user0));
        let err = session
            .update_file("user.rock", user1)
            .expect_err(&format!("{edit_kind}: the session refuses the edit too"));
        assert!(
            err.message.contains(expected),
            "{edit_kind}: {}",
            err.message
        );
    }
}

#[test]
fn a_module_alias_added_and_removed_moves_its_fragment() {
    let c0 = "mod c;\n\nfn pick() -> int {\n    m::bump(1)\n}\n";
    let c1 = "mod c;\n\nuse super::a as m;\n\nfn pick() -> int {\n    m::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("c.rock", c0),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_diagnostic("did not resolve");

    h.edit("c.rock", c1).expect("accepted");
    h.assert_no_diagnostics();
    h.assert_call("c.rock", "a::bump");

    h.edit("c.rock", c0).expect("accepted");
    h.assert_diagnostic("did not resolve");
}

#[test]
fn a_member_removed_from_an_aliased_module_breaks_and_restores_the_alias_user() {
    let a1 = "mod a;\n\npub fn other(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nuse super::a as m;\n\nfn pick() -> int {\n    m::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("c.rock", c),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_call("c.rock", "a::bump");

    h.edit("a.rock", a1).expect("accepted");
    h.assert_diagnostic("did not resolve");

    h.edit("a.rock", A_BUMP).expect("accepted");
    h.assert_call("c.rock", "a::bump");
}

#[test]
fn a_module_reexport_added_and_repointed_moves_a_path_through_it() {
    let facade0 = "mod facade;\n\npub fn keep() -> int {\n    1\n}\n";
    let facade1 = "mod facade;\n\npub use super::a as api;\n\npub fn keep() -> int {\n    1\n}\n";
    let facade2 = "mod facade;\n\npub use super::b as api;\n\npub fn keep() -> int {\n    1\n}\n";
    let user = "mod user;\n\nfn pick() -> int {\n    super::facade::api::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", A_BUMP),
        v1("b.rock", B_BUMP),
        v1("facade.rock", facade0),
        v1("user.rock", user),
        v1("d.rock", UNRELATED),
    ]);
    h.assert_diagnostic("did not resolve");

    h.edit("facade.rock", facade1).expect("accepted");
    h.assert_no_diagnostics();
    h.assert_call("user.rock", "a::bump");

    h.edit("facade.rock", facade2).expect("accepted");
    h.assert_call("user.rock", "b::bump");
}

#[test]
fn imported_iterator_protocol_tracks_visibility_and_body_edits() {
    let provider = "mod sequence;\npub value Sequence { item: int }\n\
        pub struct Cursor { item: int, done: bool = false }\n\
        pub fn iter(sequence: Sequence) -> Cursor { return Cursor { item: sequence.item } }\n\
        pub fn next(cursor: Cursor) -> (bool, int) {\n\
            let live = !cursor.done\n cursor.done = true\n return (live, cursor.item)\n }\n";
    let visible = "use self::sequence::{Sequence, iter, next};\n\
        fn consume(sequence: Sequence) -> int {\n\
            for item in sequence { return item }\n return 0\n }\n";
    let hidden = visible.replace("iter, next", "iter");
    let mut h = V1Harness::new(vec![
        v1("sequence.rock", provider),
        v1("caller.rock", visible),
    ]);
    h.assert_no_diagnostics();
    h.edit("caller.rock", &hidden).expect("scope edit");
    assert!(!h.diagnostics().is_empty(), "unimported next is rejected");
    h.edit("caller.rock", visible).expect("restore import");
    h.assert_no_diagnostics();
    h.edit("caller.rock", &visible.replace("return item", "return 1"))
        .expect("body edit");
    h.assert_no_diagnostics();
}
