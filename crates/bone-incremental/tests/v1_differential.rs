//! Modules-v1 incremental == fresh, compared on what resolution decided, and
//! the deterministic gates for plan M5's scope epochs (plan section 2.6).
//!
//! A sibling of `incremental.rs` rather than an extension of it: that oracle
//! parses every file legacy through `parse_module_clean` and compares body
//! diagnostics and types. A v1 case needs each file parsed in its own dialect
//! with its package tag, the fresh reference built through the aliased entry,
//! lowering and interface diagnostics as well as body ones, the RESOLVED KEYS
//! of every reference (a resolution can move to another definition of the same
//! type, which neither diagnostics nor types can see), and a refused build as
//! a comparable outcome. The per-offset type reading is shared (`common`).
//!
//! The gates pin the scope path's work (plan 2.6): an edit to a
//! namespace-tree input re-assembles the tree, and re-lowers the edited file
//! plus every file whose lowering read a changed fact, at fragment level.
//! Each gate asserts its tier, the bodies re-inferred and re-lowered, and --
//! through `edit` -- that the result equals a fresh build.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use bone_core::modules::NsAliases;
use bone_incremental::session::{BodyId, Session, SessionFile, UpdateResult, UpdateTier};
use bone_semantic::hir::{BodyOwner, CallFamily, Expr, Module, TypeCtor, TypeExpr};
use bone_semantic::prelude::*;
use skull_parser::{Ast, Dialect, GlobalPolicy, PackageTag};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PACKAGE: &str = "livev1";

fn v1(path: &str, text: &str) -> SessionFile {
    SessionFile {
        path: path.into(),
        text: text.into(),
        dialect: Dialect::V1,
        package: Some(PackageTag { id: 1, self_name: PACKAGE.into(), identity_storage: Default::default() }),
        global_policy: GlobalPolicy::Any,
    }
}

fn legacy(path: &str, text: &str) -> SessionFile {
    SessionFile::legacy(path, text)
}

/// A two-function stand-in for the stdlib. It carries a stdlib file id, so it
/// is the `std` package and its names are the v1 prelude (tier 5). Every gate
/// builds with it so a count would show a rebuild that re-inferred the warm
/// modules.
const WARM_PATH: &str = "<bone-stdlib:epoch_warm>";
const WARM: &str = "fn warm_one() -> int {\n    1\n}\n\nfn warm_two() -> int {\n    2\n}\n";

fn warm() -> Vec<Ast> {
    vec![skull_parser::parse_module_with_dialect(WARM, Ustr::from(WARM_PATH), Dialect::Legacy).expect("warm parses")]
}

// ---------------------------------------------------------------------------
// What a build decided
// ---------------------------------------------------------------------------

/// One reference a body resolved, anchored by source text so an incremental
/// workspace and a fresh one compare despite different arena indices and
/// module ids: (file, start, end, kind, key, merged-family members).
type Reference = (String, usize, usize, &'static str, String, Option<String>);

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    /// Body, lowering and interface diagnostics as (file, offset, message).
    diagnostics: Vec<(String, usize, String)>,
    references: BTreeSet<Reference>,
    /// Nominal keys each file's signatures, fields and globals name. A set:
    /// the body-edit fast path appends signature types and orphans the old
    /// entries, which carry the same keys.
    signature_keys: BTreeMap<String, BTreeSet<String>>,
    /// `type_at` at every offset of every user file.
    types: BTreeMap<String, Vec<Option<String>>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Built(Snapshot),
    /// The whole pass returned `Err` (a tree-construction or API-closure
    /// rejection): nothing is queryable, the message is the verdict.
    Refused(String),
}

fn snapshot(ws: &Workspace, files: &[SessionFile]) -> Snapshot {
    let user: BTreeSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut diagnostics: Vec<(String, usize, String)> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| (e.location.file.to_string(), e.location.offset, e.message.clone()))
        .collect();
    diagnostics.sort();

    let mut references = BTreeSet::new();
    let mut signature_keys = BTreeMap::new();
    for module in ws.modules.iter().filter(|m| user.contains(m.file.as_str())) {
        let file = module.file.to_string();
        for body in module.bodies.values() {
            for (idx, expr) in body.expr_arena.iter() {
                let Some((kind, key, members)) = expr_reference(module, expr) else { continue };
                let (s, e) = body.expr_spans[idx].offset_range;
                references.insert((file.clone(), s, e, kind, key.to_string(), members));
            }
            for (idx, te) in body.lowered_tys.iter() {
                if let TypeExpr::App { ctor: TypeCtor::Nominal(key), .. } = te {
                    let (s, e) = body.lowered_ty_spans.get(idx).map(|sp| sp.offset_range).unwrap_or((0, 0));
                    references.insert((file.clone(), s, e, "type", key.to_string(), None));
                }
            }
        }
        // Which definition each overloaded call selected, named by the
        // selected function's file, name and position.
        if let Some(inf) = ws.inference(module.id) {
            for (owner, bi) in inf.per_fn.iter() {
                let Some(body) = module.bodies.get(owner) else { continue };
                for (call, (tmid, fid)) in bi.resolved_overload_calls.iter() {
                    let target = ws.module(*tmid);
                    let f = &target.functions[*fid];
                    let (s, e) = body.expr_spans[*call].offset_range;
                    let selected = format!("{}::{}@{}", target.file, f.name, f.name_span.offset_range.0);
                    references.insert((file.clone(), s, e, "selects", selected, None));
                }
            }
        }
        let keys: BTreeSet<String> = module
            .ty_arena
            .iter()
            .filter_map(|(_, te)| match te {
                TypeExpr::App { ctor: TypeCtor::Nominal(key), .. } => Some(key.to_string()),
                _ => None,
            })
            .collect();
        signature_keys.insert(file, keys);
    }

    let types = files
        .iter()
        .map(|f| (f.path.clone(), common::type_profile(ws, &f.path, &f.text, 1)))
        .collect();

    Snapshot { diagnostics, references, signature_keys, types }
}

/// A reference an expression makes: its kind, key, and for a merged call
/// family the members lowering recorded, with their lookup tiers.
fn expr_reference(module: &Module, expr: &Expr) -> Option<(&'static str, Ustr, Option<String>)> {
    let members = |family: &CallFamily| match family {
        CallFamily::Merged { name, .. } => module.call_families.get(name).map(|members| {
            let mut shown: Vec<String> =
                members.iter().map(|m| format!("{}/{:?}/{:?}", m.key, m.owner, m.tier)).collect();
            shown.sort();
            shown.join(",")
        }),
        _ => None,
    };
    match expr {
        Expr::Call { family, .. } => Some(("call", family.key(), members(family))),
        Expr::SafeNavCall { family, .. } => Some(("safe-nav-call", family.key(), members(family))),
        Expr::Global(key) => Some(("global", *key, None)),
        Expr::StructLiteral { name, .. } => Some(("struct", *name, None)),
        Expr::EnumVariant { enum_name, .. } => Some(("enum", *enum_name, None)),
        _ => None,
    }
}

fn parse(files: &[SessionFile]) -> Result<Vec<Ast>, CompilerError> {
    files
        .iter()
        .map(|f| {
            let mut ast = skull_parser::parse_module_with_dialect(&f.text, Ustr::from(f.path.as_str()), f.dialect)?;
            ast.package = f.package.clone();
            Ok(ast)
        })
        .collect()
}

/// The fresh reference for `files`, built twice: through `Session::new` and
/// through the batch entry `semantic_pass_with_aliases`. The two entries must
/// agree before either is trusted as the reference.
fn fresh(files: &[SessionFile]) -> Outcome {
    fresh_with(files, &NsAliases::default())
}

/// [`fresh`] under an explicit package alias table.
fn fresh_with(files: &[SessionFile], aliases: &NsAliases) -> Outcome {
    let session = Session::new(warm(), files.to_vec(), aliases);
    let batch = parse(files).and_then(|asts| {
        let mut modules = warm();
        modules.extend(asts);
        semantic_pass_with_aliases(&mut modules, aliases)
    });
    match (session, batch) {
        (Ok(s), Ok(b)) => {
            let from_session = snapshot(s.workspace(), files);
            assert_eq!(from_session, snapshot(&b, files), "Session::new and the batch pass disagree");
            Outcome::Built(from_session)
        }
        (Err(s), Err(b)) => {
            assert_eq!(s.message, b.message, "Session::new and the batch pass refuse differently");
            Outcome::Refused(s.message)
        }
        (s, b) => panic!(
            "Session::new and the batch pass disagree on refusal: session {:?}, batch {:?}",
            s.err().map(|e| e.message),
            b.err().map(|e| e.message)
        ),
    }
}

// ---------------------------------------------------------------------------
// The edit-script harness
// ---------------------------------------------------------------------------

struct V1Harness {
    session: Session,
    /// The texts the session holds, in tracking order.
    files: Vec<SessionFile>,
    aliases: NsAliases,
}

impl V1Harness {
    #[track_caller]
    fn new(files: Vec<SessionFile>) -> Self {
        Self::with_aliases(files, NsAliases::default())
    }

    #[track_caller]
    fn with_aliases(files: Vec<SessionFile>, aliases: NsAliases) -> Self {
        let session = Session::new(warm(), files.clone(), &aliases).expect("baseline builds");
        let h = V1Harness { session, files, aliases };
        h.assert_matches_fresh();
        h
    }

    /// Apply one edit. On `Ok`, the session must equal a fresh build of the
    /// edited texts. On `Err`, a fresh build of the edited texts must refuse
    /// with the same message, and the session (which reverts) must still
    /// equal a fresh build of the previous texts.
    #[track_caller]
    fn edit(&mut self, path: &str, text: &str) -> Result<UpdateResult, String> {
        let mut edited = self.files.clone();
        edited.iter_mut().find(|f| f.path == path).expect("edits a tracked file").text = text.to_string();
        match self.session.update_file(path, text) {
            Ok(result) => {
                self.files = edited;
                self.assert_matches_fresh();
                Ok(result)
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
        let reference = fresh_with(&self.files, &self.aliases);
        let Outcome::Built(reference) = reference else {
            panic!("the session holds texts a fresh build refuses: {reference:?}");
        };
        let incremental = snapshot(self.session.workspace(), &self.files);
        assert_eq!(incremental.diagnostics, reference.diagnostics, "incremental != fresh: diagnostics");
        assert_eq!(incremental.references, reference.references, "incremental != fresh: resolved references");
        assert_eq!(incremental.signature_keys, reference.signature_keys, "incremental != fresh: signature keys");
        assert_eq!(incremental.types, reference.types, "incremental != fresh: types");
    }

    /// Function bodies the last update re-inferred, warm stdlib included if
    /// it re-inferred any (`Workspace.reinferred_body_count`, reset per
    /// update).
    fn reinferred_body_count(&self) -> usize {
        self.session.workspace().reinferred_body_count.load(Ordering::Relaxed)
    }

    /// Function bodies the last update lowered, including one that failed to
    /// lower (`Workspace.relowered_body_count`, reset per update).
    fn relowered_body_count(&self) -> usize {
        self.session.workspace().relowered_body_count.load(Ordering::Relaxed)
    }

    /// Distinct function bodies in `ids`, per file (files with none omitted).
    fn functions_per_file(&self, ids: &[BodyId]) -> BTreeMap<String, usize> {
        let ws = self.session.workspace();
        let distinct: BTreeSet<(String, u32)> = ids
            .iter()
            .filter_map(|(mid, owner)| match owner {
                BodyOwner::Function(fid) => Some((ws.module(*mid).file.to_string(), fid.into_raw().into_u32())),
                _ => None,
            })
            .collect();
        let mut out = BTreeMap::new();
        for (file, _) in distinct {
            *out.entry(file).or_insert(0) += 1;
        }
        out
    }

    fn call_keys(&self, path: &str) -> Vec<(String, Option<String>)> {
        snapshot(self.session.workspace(), &self.files)
            .references
            .into_iter()
            .filter(|r| r.0 == path && r.3 == "call")
            .map(|r| (r.4, r.5))
            .collect()
    }

    fn diagnostics(&self) -> Vec<String> {
        self.session.diagnostics().iter().map(|d| d.message.clone()).collect()
    }

    /// The definitions the overloaded calls in `path` selected, by name.
    fn selected(&self, path: &str) -> Vec<String> {
        snapshot(self.session.workspace(), &self.files)
            .references
            .into_iter()
            .filter(|r| r.0 == path && r.3 == "selects")
            .map(|r| {
                let (_, rest) = r.4.split_once("::").expect("file::name@offset");
                rest.split('@').next().expect("name").to_string()
            })
            .collect()
    }
}

/// The members of a merged v1 family as `call_keys` renders them.
fn family(members: &[(&str, &str)]) -> Option<String> {
    Some(members.iter().map(|(key, tier)| format!("{key}/None/{tier}")).collect::<Vec<_>>().join(","))
}

fn per_file(list: &[(&str, usize)]) -> BTreeMap<String, usize> {
    list.iter().map(|(f, n)| (f.to_string(), *n)).collect()
}

fn key(path: &str) -> String {
    format!("{PACKAGE}::{path}")
}

// ---------------------------------------------------------------------------
// Comparator self-tests: the reading must see what it claims to compare.
// ---------------------------------------------------------------------------

const A_BUMP: &str = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
const B_BUMP: &str = "mod b;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
const UNRELATED: &str = "mod d;\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n";

/// Two programs whose only difference is WHICH `bump` a call resolves to:
/// same diagnostics (none), same types at every offset. Only the resolved
/// references tell them apart, which is why the harness reads them.
#[test]
fn comparator_sees_a_resolution_that_moves_between_same_typed_definitions() {
    let case = |target: &str| {
        vec![
            v1("a.rock", A_BUMP),
            v1("b.rock", B_BUMP),
            v1("c.rock", &format!("mod c;\n\nuse super::{target}::bump;\n\nfn pick() -> int {{\n    bump(1)\n}}\n")),
        ]
    };
    let (Outcome::Built(to_a), Outcome::Built(to_b)) = (fresh(&case("a")), fresh(&case("b"))) else {
        panic!("both build");
    };
    assert_eq!(to_a.diagnostics, to_b.diagnostics);
    assert_eq!(to_a.types, to_b.types);
    assert_ne!(to_a.references, to_b.references, "the reference reading cannot see the resolution move");
    assert!(to_a.references.iter().any(|r| r.3 == "call" && r.4 == key("a::bump")), "{:?}", to_a.references);
    assert!(to_b.references.iter().any(|r| r.3 == "call" && r.4 == key("b::bump")), "{:?}", to_b.references);
}

/// A refused build is an outcome the harness compares, not a panic: the
/// session refuses the edit with the message a fresh build refuses with, and
/// is still equal to a fresh build of the texts it kept.
#[test]
fn a_refused_build_is_a_comparable_outcome() {
    let s0 = "mod s;\n\nstruct Hidden {\n    pub x: int,\n}\n\npub fn mk() -> int {\n    1\n}\n";
    let s1 = "mod s;\n\nstruct Hidden {\n    pub x: int,\n}\n\npub fn mk() -> Hidden {\n    Hidden { x: 1 }\n}\n";
    let mut h = V1Harness::new(vec![v1("s.rock", s0), v1("root.rock", "pub use s::mk;\n")]);
    let err = h.edit("s.rock", s1).expect_err("the API closure refuses it");
    assert!(err.contains("type `Hidden` in public API is not accessible"), "{err}");
}

/// The full rebuild re-lowers and re-infers the user files only. The warm
/// stdlib is immutable for the session's lifetime and consults no user scope
/// (`legacy_name` answers `Bare` for a stdlib file, and a stdlib file has no
/// fragment scope), so its lowered and inferred state is reused.
#[test]
fn the_full_rebuild_reinfers_no_warm_body() {
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("d.rock", UNRELATED)]);
    h.session.ws.reinferred_body_count.store(0, Ordering::Relaxed);
    h.session.rebuild_users().expect("an unchanged program rebuilds");
    assert_eq!(h.reinferred_body_count(), 3, "a's bump and d's two bodies; no warm body");
    h.assert_matches_fresh();
}

// ---------------------------------------------------------------------------
// M5 gates (plan 2.6). Fragment-level: whole fragments whose recorded
// lookups the delta touches re-lower and re-infer. "Per-body" names the next
// step, which needs rung 2 (per-body re-lower) and does not exist yet.
// ---------------------------------------------------------------------------

/// Control for the gate readings: on a gate fixture, an edit that is NOT a
/// namespace-tree input takes a narrow tier and both readings drop to it. A
/// gate's count is therefore the routing's, not a constant of the fixture,
/// and narrowing the routing moves it.
#[test]
fn control_a_body_edit_on_a_gate_fixture_reads_narrow() {
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let d1 = "mod d;\n\nfn far() -> int {\n    20\n}\n\nfn farther() -> int {\n    3\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", B_BUMP), v1("c.rock", c), v1("d.rock", UNRELATED)]);

    let r = h.edit("d.rock", d1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::BodyOnly);
    assert_eq!(h.reinferred_body_count(), 1);
    assert_eq!(h.relowered_body_count(), 1);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("d.rock", 1)]));
}

/// Gate (a): an explicit `use` added to one fragment joins the glob family
/// it used to resolve through, and wins the exact tie (spec 8.3: tier 3 is
/// nearer than tier 4).
///
/// c's 2 bodies re-lower and re-infer, nothing else: the import is
/// fragment-local (spec 8.2), so only c's own fragment scope changed.
/// Per-body: only `pick`, the one body with a bare lookup of `bump`.
#[test]
fn gate_a_use_added_to_one_fragment() {
    let c0 = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let c1 = "mod c;\n\nuse super::b::*;\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", B_BUMP), v1("c.rock", c0), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::bump"), None)]);

    let r = h.edit("c.rock", c1).expect("accepted");
    let both = family(&[(&key("a::bump"), "Imports"), (&key("b::bump"), "Globs")]);
    assert_eq!(h.call_keys("c.rock"), vec![("bump".to_string(), both)]);
    assert_eq!(h.selected("c.rock"), vec![key("a::bump")]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 2);
    assert_eq!(h.relowered_body_count(), 2);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("c.rock", 2)]));
}

/// Gate (b): the same `use` removed, leaving the lower-tier glob candidate
/// that lost the exact tie to it (spec 14.3: removing an import removes its
/// callables from every family it contributed to).
///
/// c's 2 bodies re-lower and re-infer, nothing else. Per-body: `pick` only.
#[test]
fn gate_b_use_removed_from_one_fragment() {
    let c0 = "mod c;\n\nuse super::b::*;\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let c1 = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", B_BUMP), v1("c.rock", c0), v1("d.rock", UNRELATED)]);
    let both = family(&[(&key("a::bump"), "Imports"), (&key("b::bump"), "Globs")]);
    assert_eq!(h.call_keys("c.rock"), vec![("bump".to_string(), both)]);
    assert_eq!(h.selected("c.rock"), vec![key("a::bump")]);

    let r = h.edit("c.rock", c1).expect("accepted");
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::bump"), None)]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 2);
    assert_eq!(h.relowered_body_count(), 2);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("c.rock", 2)]));
}

/// Gate (c): a `pub fn` added to a module another fragment globs. Here the
/// new member joins the prelude family the globber was already calling and
/// wins the exact tie (spec 8.3: tier 4 is nearer than tier 5), so the
/// globber's selection really moves.
///
/// b (edited, 2 bodies after the edit) and c (its bare lookup of `warm_one`
/// probed b through the glob and missed) -- 3; d and the warm stdlib 0.
/// Per-body: b's new `warm_one` plus c's `pick`.
#[test]
fn gate_c_glob_member_added() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\npub fn warm_one() -> int {\n    5\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), None)]);

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), warm_one_family())]);
    assert_eq!(h.selected("c.rock"), vec![key("b::warm_one")]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 3);
    assert_eq!(h.relowered_body_count(), 3);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("b.rock", 2), ("c.rock", 1)]));
}

/// A lower-tier overload added and removed (spec 8.3, 14.3): c's own
/// `bump(string)` binds the name first, and b's `bump(int)`, added to a
/// module c globs, joins its family and is the only fit for `bump(1)`. The
/// caller re-lowers on both edits, as a fresh build of each text agrees.
#[test]
fn a_lower_tier_overload_added_joins_the_callers_family() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\npub fn bump(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn bump(s: string) -> string {\n    s\n}\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("type mismatch")), "{:?}", h.diagnostics());

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    let both = family(&[(&key("b::bump"), "Globs"), (&key("c::bump"), "Own")]);
    assert_eq!(h.call_keys("c.rock"), vec![("bump".to_string(), both)]);
    assert_eq!(h.selected("c.rock"), vec![key("b::bump")]);
    assert!(h.functions_per_file(&r.relowered).get("c.rock").is_some_and(|&n| n >= 1), "{:?}", r.relowered);

    let r = h.edit("b.rock", b0).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.call_keys("c.rock"), vec![(key("c::bump"), None)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("type mismatch")), "{:?}", h.diagnostics());
}

/// Gate (d): a private helper added in a module nothing else names. A glob
/// never opens an inaccessible member (`glob_candidates`), and no path
/// reaches it, so no other fragment's resolution can change.
///
/// ZERO bodies outside d: d's 3 re-lower and re-infer (per-body: only the
/// new `helper`).
#[test]
fn gate_d_private_helper_added_in_an_unrelated_module() {
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let d1 = "mod d;\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n\nfn helper() -> int {\n    4\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c), v1("d.rock", UNRELATED)]);

    let r = h.edit("d.rock", d1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 3);
    assert_eq!(h.relowered_body_count(), 3);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("d.rock", 3)]));
}

/// Gate (e): a module header renamed. b's definitions change DefKey, and the
/// fragment that named `super::b::depth` by path loses it.
///
/// b (its keys moved, 1 body) and c (its path walked node `b`; it must
/// re-lower to report the miss) re-lower -- 2 lowered, 1 re-inferred, since
/// c's `pick` now fails to LOWER and so is in neither the re-inferred count
/// nor the `relowered` list; d and the warm stdlib 0.
#[test]
fn gate_e_module_header_changed() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod bee;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::b::depth()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::depth"), None)]);

    let r = h.edit("b.rock", b1).expect("accepted");
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 1);
    assert_eq!(h.relowered_body_count(), 2);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("b.rock", 1)]));
}

/// Gate (f): a `pub use` re-export edited in `facade`, reaching `user`
/// through a glob chain: user globs hub, hub glob-re-exports facade, and
/// facade's named re-export is re-pointed from `a::bump` to `b::bump`
/// (`resolve::members_of` follows the chain to a fixed point).
///
/// facade (edited, no bodies) and user (its bare `bump` walked hub ->
/// facade, 1 body) re-lower -- 1; a, b, d and the warm stdlib 0. Per-body:
/// the same 1.
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
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("user.rock"), vec![(key("a::bump"), None)]);

    let r = h.edit("facade.rock", facade1).expect("accepted");
    assert_eq!(h.call_keys("user.rock"), vec![(key("b::bump"), None)]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 1);
    assert_eq!(h.relowered_body_count(), 1);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("user.rock", 1)]));
}

/// A glob re-export of a module that binds the name through a NAMED
/// re-export resolves, by path and through a glob of the hub. The glob
/// guard and the export-binding guard used to share `(package, node, name)`
/// visit keys, so facade's named export read as a cycle once the glob of
/// hub had opened facade.
#[test]
fn a_glob_reexport_of_a_named_reexport_resolves() {
    let facade = "mod facade;\n\npub use super::a::bump;\n";
    let hub = "mod hub;\n\npub use super::facade::*;\n";
    let by_path = "mod user;\n\nfn pick() -> int {\n    super::hub::bump(1)\n}\n";
    let by_glob = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let direct = "mod user;\n\nuse super::facade::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let build = |user: &str| {
        V1Harness::new(vec![v1("a.rock", A_BUMP), v1("facade.rock", facade), v1("hub.rock", hub), v1("user.rock", user)])
    };
    for user in [direct, by_path, by_glob] {
        let h = build(user);
        assert!(h.diagnostics().is_empty(), "{user}: {:?}", h.diagnostics());
        assert_eq!(h.call_keys("user.rock"), vec![(key("a::bump"), None)], "{user}");
    }
}

/// Gate (g): visibility narrowed on an item another module names by a
/// qualified path. The access check is baked in at lowering, so the user
/// must re-LOWER; re-inference alone would keep the stale verdict.
///
/// a (edited, 1 body) and c (its path resolved `a::bump`; it must re-lower)
/// re-lower -- 2 lowered, 1 re-inferred, since c's `pick` now fails to LOWER
/// with the access error; d and the warm stdlib 0.
#[test]
fn gate_g_visibility_narrowed_on_a_path_used_item() {
    let a1 = "mod a;\n\nfn bump(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());

    let r = h.edit("a.rock", a1).expect("accepted");
    assert!(
        h.diagnostics().iter().any(|d| d.contains("`bump` is not accessible from this module")),
        "{:?}",
        h.diagnostics()
    );
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 1);
    assert_eq!(h.relowered_body_count(), 2);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("a.rock", 1)]));
}

/// Gate (h): a legacy definer adds a callable the v1 package also projects to
/// legacy files, so a legacy caller's bare `bump` becomes one merged overload
/// family (`CallFamily::Merged`), decided at lowering. The two members
/// have the same signature, so the call is an overload tie at inference.
///
/// legacy_def (edited, 2 bodies) and legacy_use (its bare `bump` read the
/// projection alone; it must re-lower into the merged family) -- 3; the v1
/// files, legacy_far and the warm stdlib 0. Per-body: the new `bump` plus
/// `use_it`'s re-lower.
#[test]
fn gate_h_legacy_definer_edit_collides_with_a_projected_name() {
    let without = "fn other(n: int) -> int {\n    n\n}\n";
    let with = "fn other(n: int) -> int {\n    n\n}\n\nfn bump(n: int) -> int {\n    n\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n"),
        v1("root.rock", "pub use a::bump;\n"),
        legacy("legacy_def.rock", without),
        legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
        legacy("legacy_far.rock", "fn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n"),
    ]);
    assert_eq!(h.call_keys("legacy_use.rock"), vec![(key("a::bump"), None)]);

    let r = h.edit("legacy_def.rock", with).expect("accepted");
    assert!(
        h.diagnostics().iter().any(|d| d.contains("Ambiguous overload for `bump` with argument types (int)")),
        "{:?}",
        h.diagnostics()
    );
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.reinferred_body_count(), 3);
    assert_eq!(h.relowered_body_count(), 3);
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("legacy_def.rock", 2), ("legacy_use.rock", 1)])
    );
}


// ---------------------------------------------------------------------------
// Scope-epoch soundness cases beyond the gates. Each edit is checked
// incremental == fresh by `edit` (diagnostics, resolved keys, overload
// selections, signature keys, types, or the same refusal); the tier checks
// that the case exercises the scope path rather than the full rebuild.
// ---------------------------------------------------------------------------

/// The glob `warm_one` joined with the std prelude's (spec 8.3).
fn warm_one_family() -> Option<String> {
    Some(format!("{}/None/Globs,warm_one/Some({})/Std", key("b::warm_one"), bone_core::modules::STD_PACKAGE_ID))
}

/// A glob member that won an exact tie against a std name is removed again:
/// the globber's family is the std one alone.
#[test]
fn a_glob_member_that_shadowed_std_is_removed() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\npub fn warm_one() -> int {\n    5\n}\n";
    let b1 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), warm_one_family())]);

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), None)]);
    assert_eq!(h.relowered_body_count(), 2, "b's depth and c's pick");
}

/// A legacy file that starts defining a std-named function does not join the
/// std family a v1 call reaches through tier 5 (spec 5): the call keeps
/// selecting the std definition rather than tying with the legacy one, in
/// the incremental session as in a fresh build.
#[test]
fn a_legacy_definition_of_a_std_name_added_leaves_the_v1_call_on_std() {
    let without = "fn other() -> int {\n    2\n}\n";
    let with = "fn other() -> int {\n    2\n}\n\nfn warm_one() -> int {\n    5\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![v1("c.rock", c), legacy("legacy_def.rock", without)]);
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), None)]);

    h.edit("legacy_def.rock", with).expect("accepted");
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), None)]);
    let selected: Vec<String> = snapshot(h.session.workspace(), &h.files)
        .references
        .into_iter()
        .filter(|r| r.0 == "c.rock" && r.3 == "selects")
        .map(|r| r.4)
        .collect();
    assert!(selected.iter().all(|s| s.starts_with(WARM_PATH)), "{selected:?}");
}

/// A private member added to a globbed module never opens through the glob,
/// so the globber's lookup keeps its std answer. The globber probed that
/// member (a miss), so it still re-lowers: the design's replay refinement,
/// which would skip it, is not implemented.
#[test]
fn a_private_member_added_to_a_globbed_module_keeps_the_std_answer() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\nfn warm_one() -> int {\n    5\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), None)]);
    assert_eq!(h.relowered_body_count(), 3, "b's two bodies and c's pick");
}

/// Visibility widened on an item another module names by path: the user's
/// body failed to lower with the access error and must re-lower clean.
#[test]
fn visibility_widened_on_a_path_used_item() {
    let a0 = "mod a;\n\nfn bump(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", a0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("not accessible")), "{:?}", h.diagnostics());

    let r = h.edit("a.rock", A_BUMP).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), None)]);
    assert_eq!(h.relowered_body_count(), 2, "a's bump and c's pick");
}

/// A visibility-only edit to one member of an overload family invalidates the
/// external caller even though the family key and its public sibling remain.
/// The incremental result must match a fresh build after the per-member table
/// changes, rather than reusing the old selected overload.
#[test]
fn overload_member_visibility_edit_rechecks_external_callers() {
    let a0 = "mod a;\n\npub fn f(n: int) -> int {\n    n\n}\n\npub fn f(s: string) -> string {\n    s\n}\n";
    let a1 = "mod a;\n\nfn f(n: int) -> int {\n    n\n}\n\npub fn f(s: string) -> string {\n    s\n}\n";
    let c = "mod c;\n\nuse super::a::f;\n\nfn pick() -> int {\n    f(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", a0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::f"), None)]);

    let r = h.edit("a.rock", a1).expect("visibility edit is accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().iter().any(|d| d.contains("not accessible")), "{:?}", h.diagnostics());
    assert_eq!(h.relowered_body_count(), 3, "a's two overloads and c's caller");
}

/// The file of every overloaded call `path` selected.
fn selected_files(h: &V1Harness, path: &str) -> Vec<String> {
    snapshot(h.session.workspace(), &h.files)
        .references
        .into_iter()
        .filter(|r| r.0 == path && r.3 == "selects")
        .map(|r| r.4.split_once("::").expect("file::name@offset").0.to_string())
        .collect()
}

/// A signature edit in another file moves a declaration into, and back out
/// of, the backend-variant group of the caller's portable `now(int)`. Paired,
/// the call collapses to the portable declaration instead of tying; split,
/// the exact `int` overload wins.
#[test]
fn a_signature_edit_regroups_backend_variants() {
    let a = "fn now(n: int) -> int {\n    n\n}\n\nfn pick() -> int {\n    now(1)\n}\n";
    let apart = "extern(js) fn now(n: float) -> int\n";
    let paired = "extern(js) fn now(n: int) -> int\n";
    let mut h = V1Harness::new(vec![legacy("a.rock", a), legacy("b.rock", apart)]);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());

    for text in [paired, apart] {
        h.edit("b.rock", text).expect("accepted");
        assert!(h.diagnostics().is_empty(), "{text}: {:?}", h.diagnostics());
        assert_eq!(selected_files(&h, "a.rock"), vec!["a.rock".to_string()], "{text}");
    }
}

/// A backend sibling added by an edit joins the group of an unchanged
/// module's declaration, and removing it leaves that declaration alone.
#[test]
fn an_added_backend_sibling_joins_the_group() {
    let a = "fn now(n: int) -> int {\n    n\n}\n\nfn pick() -> int {\n    now(1)\n}\n";
    let without = "fn other() -> int {\n    2\n}\n";
    let with = "fn other() -> int {\n    2\n}\n\nextern(js) fn now(n: int) -> int\n";
    let mut h = V1Harness::new(vec![legacy("a.rock", a), legacy("b.rock", without)]);

    for text in [with, without] {
        h.edit("b.rock", text).expect("accepted");
        assert!(h.diagnostics().is_empty(), "{text}: {:?}", h.diagnostics());
        assert_eq!(selected_files(&h, "a.rock"), vec!["a.rock".to_string()], "{text}");
    }
}

/// A named export binding added to a module makes a path that did not
/// resolve through it resolve, and removing it breaks the path again.
#[test]
fn a_reexport_added_makes_a_failed_path_resolve() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub use super::a::bump;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::b::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), None)]);

    let r = h.edit("b.rock", b0).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
}

/// A `use` added to the fragment that failed to resolve a bare name.
#[test]
fn a_use_added_makes_an_unresolved_name_resolve() {
    let c0 = "mod c;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let c1 = "mod c;\n\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c0), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());

    let r = h.edit("c.rock", c1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.relowered_body_count(), 1, "c's pick alone");
}

/// A module header renamed away and back: the path user recovers.
#[test]
fn a_module_header_renamed_back_restores_the_path() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod bee;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::b::depth()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    h.edit("b.rock", b1).expect("accepted");
    let r = h.edit("b.rock", b0).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::depth"), None)]);
}

/// Two fragments of one logical module: a definition renamed in one moves
/// the other's tier-2 lookup.
#[test]
fn a_definition_renamed_in_one_fragment_moves_its_sibling_fragment() {
    let b1 = "mod b;\n\npub fn helper() -> int {\n    1\n}\n";
    let b1_renamed = "mod b;\n\npub fn helper2() -> int {\n    1\n}\n";
    let b2 = "mod b;\n\nfn user() -> int {\n    helper()\n}\n";
    let mut h = V1Harness::new(vec![v1("b1.rock", b1), v1("b2.rock", b2), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("b2.rock"), vec![(key("b::helper"), None)]);

    let r = h.edit("b1.rock", b1_renamed).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().iter().any(|d| d.contains("`helper` did not resolve")), "{:?}", h.diagnostics());
    assert_eq!(h.relowered_body_count(), 2, "b1's helper2 and b2's user");
}

/// An inline `mod` block added: a path into it that did not resolve does.
#[test]
fn an_inline_module_added_makes_a_path_into_it_resolve() {
    let d1 = "mod d;\n\npub mod inner {\n    pub fn f() -> int {\n        1\n    }\n}\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::d::inner::f()\n}\n";
    let mut h = V1Harness::new(vec![v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(!h.diagnostics().is_empty());

    let r = h.edit("d.rock", d1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![(key("d::inner::f"), None)]);
}

/// A module's own visibility widened: a path through it that was denied at
/// the module segment resolves.
#[test]
fn a_module_visibility_widened_opens_a_path_through_it() {
    let hidden0 = "mod outer::hidden;\n\npub fn f() -> int {\n    1\n}\n";
    let hidden1 = "pub mod outer::hidden;\n\npub fn f() -> int {\n    1\n}\n";
    let outer = "pub mod outer;\n\npub fn g() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::outer::hidden::f()\n}\n";
    let mut h = V1Harness::new(vec![v1("outer.rock", outer), v1("hidden.rock", hidden0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("module `hidden` is not accessible")), "{:?}", h.diagnostics());

    let r = h.edit("hidden.rock", hidden1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![(key("outer::hidden::f"), None)]);
}

/// A glob export added to the hub a fragment globs composes a second
/// same-signature `bump` into the globber's family, so its call ties.
#[test]
fn a_glob_reexport_added_ties_a_globbers_call() {
    let hub0 = "mod hub;\n\npub use super::a::*;\n";
    let hub1 = "mod hub;\n\npub use super::a::*;\npub use super::b::*;\n";
    let user = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", B_BUMP), v1("hub.rock", hub0), v1("user.rock", user)]);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());

    let r = h.edit("hub.rock", hub1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().iter().any(|d| d.contains("Ambiguous overload for `bump`")), "{:?}", h.diagnostics());
}

/// The definition at the end of a re-export chain renamed: the chain's own
/// `pub use` no longer resolves, which refuses the build where it is
/// declared -- the session refuses it the same way and keeps its state.
#[test]
fn a_reexport_chain_target_renamed_is_refused_like_a_fresh_build() {
    let facade = "mod facade;\n\npub use super::a::bump;\n";
    let hub = "mod hub;\n\npub use super::facade::*;\n";
    let user = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let a1 = "mod a;\n\npub fn bumped(n: int) -> int {\n    n\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("facade.rock", facade), v1("hub.rock", hub), v1("user.rock", user)]);
    let err = h.edit("a.rock", a1).expect_err("a dangling `pub use` refuses the build");
    assert!(err.contains("invalid `use` target"), "{err}");
}

/// A v1 type's flavor changes from struct to enum under the same name, an
/// interface-path edit: a body that lowered `b::E::V` against the struct
/// (not an enum) must re-lower to see the variant.
#[test]
fn a_type_flavor_change_relowers_a_variant_path_user() {
    let b0 = "mod b;\n\npub struct E {\n    pub x: int,\n}\n";
    let b1 = "mod b;\n\npub enum E {\n    V,\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    let e = super::b::E::V\n    1\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(!h.diagnostics().is_empty());

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Interface);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
}

/// A legacy file's projection gains a name: the root re-exports another v1
/// function, and a legacy caller of that name that did not resolve does.
#[test]
fn a_projected_name_added_resolves_a_legacy_caller() {
    let a = "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n\npub fn twice(n: int) -> int {\n    n\n}\n";
    let root0 = "pub use a::bump;\n";
    let root1 = "pub use a::bump;\npub use a::twice;\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", a),
        v1("root.rock", root0),
        legacy("legacy_use.rock", "fn use_it() -> int {\n    twice(1)\n}\n"),
        legacy("legacy_far.rock", "fn far() -> int {\n    2\n}\n"),
    ]);
    assert!(!h.diagnostics().is_empty());

    let r = h.edit("root.rock", root1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("legacy_use.rock"), vec![(key("a::twice"), None)]);
    assert_eq!(h.relowered_body_count(), 1, "legacy_use's use_it alone");
}

/// A merged legacy family (`CallFamily::Merged`) whose member set grows: the
/// legacy caller's callee spelling is the same before and after, but its
/// recorded member keys (`Module::call_families`) are not, so the caller
/// must still be found through its legacy probe and re-lowered.
#[test]
fn a_merged_legacy_family_gains_a_member() {
    let a = "mod a;\n\npub fn bump(n: int, m: int) -> int {\n    n\n}\n";
    let b = "mod b;\n\npub fn bump(n: int) -> int {\n    n\n}\n";
    let root0 = "pub use a::*;\n";
    let root1 = "pub use a::*;\npub use b::*;\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", a),
        v1("b.rock", b),
        v1("root.rock", root0),
        legacy("legacy_def.rock", "fn bump(s: string) -> string {\n    s\n}\n"),
        legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
    ]);
    assert!(!h.diagnostics().is_empty(), "no member of the family takes one int");

    let r = h.edit("root.rock", root1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
}

/// A legacy definer drops the collision again: the legacy caller leaves the
/// merged family for the projection alone.
#[test]
fn a_legacy_definer_removes_a_colliding_name() {
    let without = "fn other(n: int) -> int {\n    n\n}\n";
    let with = "fn other(n: int) -> int {\n    n\n}\n\nfn bump(n: int) -> int {\n    n\n}\n";
    let mut h = V1Harness::new(vec![
        v1("a.rock", "mod a;\n\npub fn bump(n: int) -> int {\n    n\n}\n"),
        v1("root.rock", "pub use a::bump;\n"),
        legacy("legacy_def.rock", with),
        legacy("legacy_use.rock", "fn use_it() -> int {\n    bump(1)\n}\n"),
    ]);
    assert!(!h.diagnostics().is_empty());

    let r = h.edit("legacy_def.rock", without).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("legacy_use.rock"), vec![(key("a::bump"), None)]);
}

/// A type removed from a module: a body that named it by path re-lowers into
/// its error, and adding the type back clears it. A SIGNATURE naming it by
/// path refuses the whole build instead, in a session as in a fresh build.
#[test]
fn a_type_removed_and_restored_relowers_its_users() {
    let b0 = "mod b;\n\npub struct T {\n    pub x: int,\n}\n";
    let b1 = "mod b;\n\npub fn keep() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn body() -> int {\n    let t = super::b::T { x: 1 }\n    t.x\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
    let r = h.edit("b.rock", b0).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());

    let sig = "mod c;\n\nfn sig(t: super::b::T) -> int {\n    t.x\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", sig), v1("d.rock", UNRELATED)]);
    let err = h.edit("b.rock", b1).expect_err("a signature path that does not resolve refuses the build");
    assert!(err.contains("did not resolve"), "{err}");
}

/// The probes a lowering recorded are readable per file: c's path walk
/// entered node `a` and asked it for `bump`.
#[test]
fn scope_probes_record_the_path_walk() {
    use bone_core::modules::ScopeProbe;
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c)]);
    let probes = h.session.scope_probes("c.rock").expect("c is held");
    assert!(probes.contains(&ScopeProbe::Node { package: 1, node: Ustr::from("a") }), "{probes:?}");
    assert!(
        probes.contains(&ScopeProbe::Member { package: 1, node: Ustr::from("a"), name: Ustr::from("bump") }),
        "{probes:?}"
    );
}

/// A function added under a name a macro of another file already has: the
/// whole-program macro/function collision check refuses it in a fresh
/// session, so the session refuses the edit too rather than lowering it on a
/// narrow path. (Outside the harness: the batch `semantic_pass` leaves that
/// check to its CLI caller, so the harness's two fresh entries disagree.)
#[test]
fn a_function_named_like_a_macro_is_refused_like_a_fresh_build() {
    let macros = "macro twice(e: expr) -> int {\n    e * 2\n}\n";
    let user0 = "fn f() -> int {\n    1\n}\n";
    let user1 = "fn f() -> int {\n    1\n}\n\nfn twice(n: int) -> int {\n    n\n}\n";
    let files = |user: &str| vec![legacy("macros.rock", macros), legacy("user.rock", user)];
    let expected = "macro `twice` collides with a function named `twice`";

    let fresh = Session::new(warm(), files(user1), &NsAliases::default()).err().expect("a fresh session refuses it");
    assert!(fresh.message.contains(expected), "{}", fresh.message);

    let mut session = Session::new(warm(), files(user0), &NsAliases::default()).expect("baseline builds");
    let err = session.update_file("user.rock", user1).expect_err("the session refuses the edit too");
    assert!(err.message.contains(expected), "{}", err.message);
}

/// A macro call whose argument label names no parameter: a fresh session
/// refuses it (`check_macro_call_labels`), so an edit that introduces it is
/// refused the same way on every update path -- a body edit, an interface
/// edit and a scope edit alike -- rather than leaving the call unexpanded.
#[test]
fn a_bad_macro_argument_label_is_refused_like_a_fresh_build_on_every_path() {
    let macros = "macro twice(e: expr) -> int {\n    e * 2\n}\n";
    let user0 = "fn f() -> int {\n    twice(e: 1)\n}\n";
    let expected = "macro `twice` has no parameter named `x`";
    let edits = [
        (UpdateTier::BodyOnly, "fn f() -> int {\n    twice(x: 2)\n}\n"),
        (UpdateTier::Interface, "fn f(n: int) -> int {\n    twice(x: n)\n}\n"),
        (UpdateTier::Scope, "fn f() -> int {\n    twice(x: 1)\n}\n\npub fn g() -> int {\n    2\n}\n"),
    ];
    for (tier, user1) in edits {
        let files = |user: &str, dialect: fn(&str, &str) -> SessionFile| {
            vec![legacy("macros.rock", macros), dialect("user.rock", user)]
        };
        let dialect: fn(&str, &str) -> SessionFile = if tier == UpdateTier::Scope { v1 } else { legacy };
        let user0 = if tier == UpdateTier::Scope { format!("mod user;\n\n{user0}") } else { user0.to_string() };
        let user1 = if tier == UpdateTier::Scope { format!("mod user;\n\n{user1}") } else { user1.to_string() };

        let fresh = Session::new(warm(), files(&user1, dialect), &NsAliases::default())
            .err()
            .unwrap_or_else(|| panic!("{tier:?}: a fresh session refuses it"));
        assert!(fresh.message.contains(expected), "{tier:?}: {}", fresh.message);

        // The same edit with a good label pins which path it takes.
        let mut control = Session::new(warm(), files(&user0, dialect), &NsAliases::default()).expect("baseline builds");
        let good = control.update_file("user.rock", &user1.replace("x:", "e:")).expect("a good label is accepted");
        assert_eq!(good.tier, tier);

        let mut session = Session::new(warm(), files(&user0, dialect), &NsAliases::default()).expect("baseline builds");
        let err = session
            .update_file("user.rock", &user1)
            .map(|r| r.tier)
            .expect_err(&format!("{tier:?}: the session refuses the edit too"));
        assert!(err.message.contains(expected), "{tier:?}: {}", err.message);
    }
}

/// Warm and user files share the legacy bare-name maps, so a user type named
/// like a warm generic type is a cross-module duplicate: the batch pass, a
/// first session build and a rebuilt one all report `duplicate struct` on the
/// user file and nothing on the warm one. A session that lowered the warm
/// modules alone would not see that; with such a collision the session lowers
/// warm and user files together.
#[test]
fn a_user_type_named_like_a_warm_type_builds_like_a_fresh_build() {
    let warm_text = "struct Box<T> {\n    v: T,\n}\n\nfn unbox<T>(b: Box<T>) -> T {\n    b.v\n}\n";
    let warm = || {
        vec![skull_parser::parse_module_with_dialect(warm_text, Ustr::from("<bone-stdlib:collide_warm>"), Dialect::Legacy)
            .expect("warm parses")]
    };
    let apart = "struct Crate {\n    x: int,\n}\n\nfn f(b: Crate) -> int {\n    b.x\n}\n";
    let collide = "struct Box {\n    x: int,\n}\n\nfn f(b: Box) -> int {\n    b.x\n}\n";
    let files = vec![legacy("user.rock", collide)];

    let mut modules = warm();
    modules.extend(parse(&files).expect("parses"));
    let batch = semantic_pass_with_aliases(&mut modules, &NsAliases::default()).expect("the batch pass builds it");
    let reference = snapshot(&batch, &files);
    assert!(
        reference
            .diagnostics
            .iter()
            .any(|(file, _, message)| file == "user.rock" && message == "duplicate struct `Box` across modules"),
        "the batch pass reports the duplicate at the user file: {:?}",
        reference.diagnostics
    );
    assert!(
        reference.diagnostics.iter().all(|(file, _, _)| !is_stdlib_module_file(file)),
        "no diagnostic may point at the warm modules: {:?}",
        reference.diagnostics
    );

    let first = Session::new(warm(), files.clone(), &NsAliases::default()).expect("Session::new builds it too");
    assert_eq!(snapshot(first.workspace(), &files), reference, "Session::new");

    let mut session = Session::new(warm(), vec![legacy("user.rock", apart)], &NsAliases::default()).expect("builds");
    session.files.get_mut("user.rock").unwrap().text = collide.to_string();
    session.rebuild_users().expect("rebuild_users builds it too");
    assert_eq!(snapshot(session.workspace(), &files), reference, "rebuild_users");
}

// ---------------------------------------------------------------------------
// Module anchors (spec 8.1, 8.6): an import or a named re-export that lands
// on a module is a path segment, and the walk through it is probed.
// ---------------------------------------------------------------------------

/// A module alias added to the fragment anchors `m::bump`, and removing it
/// breaks the path again; the fragment's own scope changed, so only it
/// re-lowers.
#[test]
fn a_module_alias_added_and_removed_moves_its_fragment() {
    let c0 = "mod c;\n\nfn pick() -> int {\n    m::bump(1)\n}\n";
    let c1 = "mod c;\n\nuse super::a as m;\n\nfn pick() -> int {\n    m::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c0), v1("d.rock", UNRELATED)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());

    let r = h.edit("c.rock", c1).expect("accepted");
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), None)]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("c.rock", 1)]));

    h.edit("c.rock", c0).expect("accepted");
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
}

/// The module an alias names is edited in ANOTHER file: the aliasing
/// fragment's walk through `m` probed `a`'s members, so it re-lowers and
/// follows `bump` out of `a`.
#[test]
fn a_member_removed_from_an_aliased_module_relowers_the_alias_user() {
    let a1 = "mod a;\n\npub fn other(n: int) -> int {\n    n\n}\n";
    let c = "mod c;\n\nuse super::a as m;\n\nfn pick() -> int {\n    m::bump(1)\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), None)]);

    let r = h.edit("a.rock", a1).expect("accepted");
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.relowered_body_count(), 2, "a's `other` and c's `pick`");

    h.edit("a.rock", A_BUMP).expect("accepted");
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), None)]);
}

/// A named re-export of a module added to a facade makes a path through it
/// resolve in another fragment, and re-pointing it moves that path: the
/// user's walk probed the facade's member `api`.
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
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());

    let r = h.edit("facade.rock", facade1).expect("accepted");
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("user.rock"), vec![(key("a::bump"), None)]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("facade.rock", 1), ("user.rock", 1)]));

    let r = h.edit("facade.rock", facade2).expect("accepted");
    assert_eq!(h.call_keys("user.rock"), vec![(key("b::bump"), None)]);
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("facade.rock", 1), ("user.rock", 1)]));
}

// ---------------------------------------------------------------------------
// A v1 package over a legacy provider (spec 3.2, V1 -> Legacy): the view of
// what the provider's own files define.
// ---------------------------------------------------------------------------

fn in_package(dialect: Dialect, id: u32, name: &str, path: &str, text: &str) -> SessionFile {
    SessionFile { path: path.into(), text: text.into(), dialect, package: Some(PackageTag { id, self_name: name.into(), identity_storage: Default::default() }), global_policy: GlobalPolicy::Any }
}

/// `app` (2) depends on the legacy `x_lib` (1).
fn app_over_x_lib() -> NsAliases {
    NsAliases {
        packages: vec![
            bone_core::modules::PackageAliases { id: 1, self_name: "x_lib".into(), aliases: vec![] },
            bone_core::modules::PackageAliases { id: 2, self_name: "app".into(), aliases: vec![("x_lib".into(), 1)] },
        ],
    }
}

/// A legacy definer adds the name a v1 glob user was missing, then a second
/// overload the user's call selects, then drops the name again: each step
/// resolves and selects what a fresh build does.
#[test]
fn a_legacy_providers_family_grows_and_shrinks_under_a_v1_glob_user() {
    let none = "fn other() -> int {\n    1\n}\n";
    let int_only = "fn other() -> int {\n    1\n}\n\nfn mix(a: int) -> int {\n    a\n}\n";
    let both = "fn other() -> int {\n    1\n}\n\nfn mix(a: int) -> int {\n    a\n}\n\nfn mix(a: string) -> string {\n    a\n}\n";
    let user = "use x_lib::*;\n\nfn use_it() -> string {\n    mix(\"s\")\n}\n";
    let mut h = V1Harness::with_aliases(
        vec![in_package(Dialect::Legacy, 1, "x_lib", "xlib.rock", none), in_package(Dialect::V1, 2, "app", "use.rock", user)],
        app_over_x_lib(),
    );
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());

    let r = h.edit("xlib.rock", int_only).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert_eq!(h.call_keys("use.rock"), vec![("mix".to_string(), None)]);
    assert!(h.diagnostics().iter().any(|d| d.contains("type mismatch")), "{:?}", h.diagnostics());

    h.edit("xlib.rock", both).expect("accepted");
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());

    let r = h.edit("xlib.rock", none).expect("accepted");
    assert_eq!(r.tier, UpdateTier::Scope);
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
}
