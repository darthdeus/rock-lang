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
//! The gates pin today's over-invalidation: every edit to a namespace-tree
//! input takes the full user rebuild, which re-lowers and re-infers every
//! user body AND the warm stdlib. Each is a sentinel (docs/test-sentinels.md)
//! whose comment carries the scope-epoch target. When an epoch lands and a
//! count drops, the gate goes red on purpose: move its assertion to the
//! target, and keep the incremental == fresh check that `edit` runs.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use bone_core::modules::NsAliases;
use bone_incremental::session::{BodyId, Session, SessionFile, UpdateResult, UpdateTier};
use bone_semantic::hir::{BodyOwner, Expr, TypeCtor, TypeExpr};
use bone_semantic::prelude::*;
use skull_parser::{Ast, Dialect, PackageTag};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PACKAGE: &str = "livev1";

fn v1(path: &str, text: &str) -> SessionFile {
    SessionFile {
        path: path.into(),
        text: text.into(),
        dialect: Dialect::V1,
        package: Some(PackageTag { id: 1, self_name: PACKAGE.into() }),
    }
}

fn legacy(path: &str, text: &str) -> SessionFile {
    SessionFile::legacy(path, text)
}

/// A two-function stand-in for the stdlib. It carries a stdlib file id, so it
/// is the `std` package and its names are the v1 prelude (tier 5). Every gate
/// builds with it so the count shows the full rebuild re-inferring the warm
/// modules too (`combined_unexpanded` prepends them).
const WARM_PATH: &str = "<bone-stdlib:epoch_warm>";
const WARM: &str = "fn warm_one() -> int {\n    1\n}\n\nfn warm_two() -> int {\n    2\n}\n";
const WARM_BODIES: usize = 2;

fn warm() -> Vec<Ast> {
    vec![skull_parser::parse_module_with_dialect(WARM, Ustr::from(WARM_PATH), Dialect::Legacy).expect("warm parses")]
}

// ---------------------------------------------------------------------------
// What a build decided
// ---------------------------------------------------------------------------

/// One reference a body resolved, anchored by source text so an incremental
/// workspace and a fresh one compare despite different arena indices and
/// module ids: (file, start, end, kind, key, hidden-origin key).
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
                let Some((kind, key, hidden)) = expr_reference(expr) else { continue };
                let (s, e) = body.expr_spans[idx].offset_range;
                references.insert((file.clone(), s, e, kind, key.to_string(), hidden.map(|h| h.to_string())));
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

fn expr_reference(expr: &Expr) -> Option<(&'static str, Ustr, Option<Ustr>)> {
    match expr {
        Expr::Call { family, .. } => Some(("call", family.key(), family.hidden())),
        Expr::SafeNavCall { family, .. } => Some(("safe-nav-call", family.key(), family.hidden())),
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
    let session = Session::new(warm(), files.to_vec(), &NsAliases::default());
    let batch = parse(files).and_then(|asts| {
        let mut modules = warm();
        modules.extend(asts);
        semantic_pass_with_aliases(&mut modules, &NsAliases::default())
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
}

impl V1Harness {
    #[track_caller]
    fn new(files: Vec<SessionFile>) -> Self {
        let session = Session::new(warm(), files.clone(), &NsAliases::default()).expect("baseline builds");
        let h = V1Harness { session, files };
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
                    fresh(&edited),
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
        let reference = fresh(&self.files);
        let Outcome::Built(reference) = reference else {
            panic!("the session holds texts a fresh build refuses: {reference:?}");
        };
        let incremental = snapshot(self.session.workspace(), &self.files);
        assert_eq!(incremental.diagnostics, reference.diagnostics, "incremental != fresh: diagnostics");
        assert_eq!(incremental.references, reference.references, "incremental != fresh: resolved references");
        assert_eq!(incremental.signature_keys, reference.signature_keys, "incremental != fresh: signature keys");
        assert_eq!(incremental.types, reference.types, "incremental != fresh: types");
    }

    /// Function bodies the last update re-inferred, warm stdlib included
    /// (`Workspace.reinferred_body_count`, reset per update).
    fn reinferred_body_count(&self) -> usize {
        self.session.workspace().reinferred_body_count.load(Ordering::Relaxed)
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

// ---------------------------------------------------------------------------
// M5 gates. Each pins today's count (the full user rebuild) as a sentinel and
// names the scope-epoch target (plan 2.6). "Fragment-level" is the first
// target: re-lower and re-infer whole fragments whose recorded lookups the
// delta touches. "Per-body" needs rung 2 (per-body re-lower), which does not
// exist yet. When a gate goes red because a count dropped, check the new
// count against its target and flip the assertion to it.
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
    assert_eq!(h.functions_per_file(&r.relowered), per_file(&[("d.rock", 1)]));
}

/// Gate (a): an explicit `use` added to one fragment shadows the glob it
/// used to resolve through (spec 8.3: tier 3 beats tier 4).
///
/// PINNED OVER-INVALIDATION: all 6 user bodies and both warm bodies
/// re-infer. Target, fragment-level: c's 2 bodies re-lower and re-infer,
/// nothing else (the import is fragment-local, spec 8.2). Per-body: only
/// `pick`, the one body with a bare lookup of `bump`.
#[test]
fn gate_a_use_added_to_one_fragment() {
    let c0 = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let c1 = "mod c;\n\nuse super::b::*;\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", B_BUMP), v1("c.rock", c0), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::bump"), None)]);

    let r = h.edit("c.rock", c1).expect("accepted");
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), Some(key("b::bump")))]);
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 6 + WARM_BODIES, "target: 2 (c only); per-body: 1");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("a.rock", 1), ("b.rock", 1), ("c.rock", 2), ("d.rock", 2)]),
        "target: {{c.rock: 2}}"
    );
}

/// Gate (b): the same `use` removed, exposing the lower-tier glob candidate
/// it shadowed (spec 14.3: "removing an import may expose a previously
/// shadowed lower-tier family").
///
/// PINNED OVER-INVALIDATION: 6 user + 2 warm bodies. Target, fragment-level:
/// c's 2 bodies. Per-body: `pick` only.
#[test]
fn gate_b_use_removed_from_one_fragment() {
    let c0 = "mod c;\n\nuse super::b::*;\nuse super::a::bump;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let c1 = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    bump(1)\n}\n\nfn other() -> int {\n    2\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("b.rock", B_BUMP), v1("c.rock", c0), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("a::bump"), Some(key("b::bump")))]);

    let r = h.edit("c.rock", c1).expect("accepted");
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::bump"), None)]);
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 6 + WARM_BODIES, "target: 2 (c only); per-body: 1");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("a.rock", 1), ("b.rock", 1), ("c.rock", 2), ("d.rock", 2)]),
        "target: {{c.rock: 2}}"
    );
}

/// Gate (c): a `pub fn` added to a module another fragment globs. Here the
/// new member shadows a prelude name the globber was already calling (spec
/// 8.3: tier 4 beats tier 5), so the globber's resolution really moves.
///
/// PINNED OVER-INVALIDATION: 5 user + 2 warm bodies. Target, fragment-level:
/// b (edited, 2 bodies after the edit) and c (globs b, 1 body) -- 3; d and
/// the warm stdlib 0. Per-body: b's new `warm_one` plus c's `pick`, whose bare
/// lookup of `warm_one` missed b before the edit.
#[test]
fn gate_c_glob_member_added() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n\npub fn warm_one() -> int {\n    5\n}\n";
    let c = "mod c;\n\nuse super::b::*;\n\nfn pick() -> int {\n    warm_one()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![("warm_one".to_string(), None)]);

    let r = h.edit("b.rock", b1).expect("accepted");
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::warm_one"), None)]);
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 5 + WARM_BODIES, "target: 3 (b and c); per-body: 2");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("b.rock", 2), ("c.rock", 1), ("d.rock", 2)]),
        "target: {{b.rock: 2, c.rock: 1}}"
    );
}

/// Gate (d): a private helper added in a module nothing else names. A glob
/// never opens an inaccessible member (`glob_candidates`), and no path
/// reaches it, so no other fragment's resolution can change.
///
/// PINNED OVER-INVALIDATION: 5 user + 2 warm bodies. Target: ZERO bodies
/// outside d re-inferred; fragment-level d's 3, per-body only the new
/// `helper`.
#[test]
fn gate_d_private_helper_added_in_an_unrelated_module() {
    let c = "mod c;\n\nfn pick() -> int {\n    super::a::bump(1)\n}\n";
    let d1 = "mod d;\n\nfn far() -> int {\n    2\n}\n\nfn farther() -> int {\n    3\n}\n\nfn helper() -> int {\n    4\n}\n";
    let mut h = V1Harness::new(vec![v1("a.rock", A_BUMP), v1("c.rock", c), v1("d.rock", UNRELATED)]);

    let r = h.edit("d.rock", d1).expect("accepted");
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 5 + WARM_BODIES, "target: 3 (d only), 0 outside d; per-body: 1");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("a.rock", 1), ("c.rock", 1), ("d.rock", 3)]),
        "target: {{d.rock: 3}}"
    );
}

/// Gate (e): a module header renamed. b's definitions change DefKey, and the
/// fragment that named `super::b::depth` by path loses it.
///
/// PINNED OVER-INVALIDATION: 3 user + 2 warm bodies re-infer (c's `pick`
/// now fails to LOWER, so it is in neither count nor `relowered`). Target,
/// fragment-level: b (its keys moved, 1 body) and c (its path walked node
/// `b`; it must re-lower to report the miss) -- 1 re-inferred; d and the warm
/// stdlib 0.
#[test]
fn gate_e_module_header_changed() {
    let b0 = "mod b;\n\npub fn depth() -> int {\n    1\n}\n";
    let b1 = "mod bee;\n\npub fn depth() -> int {\n    1\n}\n";
    let c = "mod c;\n\nfn pick() -> int {\n    super::b::depth()\n}\n";
    let mut h = V1Harness::new(vec![v1("b.rock", b0), v1("c.rock", c), v1("d.rock", UNRELATED)]);
    assert_eq!(h.call_keys("c.rock"), vec![(key("b::depth"), None)]);

    let r = h.edit("b.rock", b1).expect("accepted");
    assert!(h.diagnostics().iter().any(|d| d.contains("did not resolve")), "{:?}", h.diagnostics());
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 3 + WARM_BODIES, "target: 1 (b; c fails to lower)");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("b.rock", 1), ("d.rock", 2)]),
        "target: {{b.rock: 1}}, plus c's refreshed lowering error"
    );
}

/// Gate (f): a `pub use` re-export edited in `facade`, reaching `user`
/// through a glob chain: user globs hub, hub glob-re-exports facade, and
/// facade's glob re-export is re-pointed from `a` to `b`
/// (`resolve::members_of` follows the chain to a fixed point).
///
/// PINNED OVER-INVALIDATION: 5 user + 2 warm bodies. Target, fragment-level:
/// facade (edited, no bodies) and user (its bare `bump` walked hub -> facade,
/// 1 body) -- 1; a, b, d and the warm stdlib 0. Per-body: the same 1.
///
/// The chain is glob-to-glob because a glob re-export of a module that binds
/// the name by a NAMED re-export does not resolve today; see
/// `sentinel_glob_reexport_of_a_named_reexport_reads_as_a_cycle`.
#[test]
fn gate_f_reexport_edit_reaches_a_globber_through_a_glob_chain() {
    let facade0 = "mod facade;\n\npub use super::a::*;\n";
    let facade1 = "mod facade;\n\npub use super::b::*;\n";
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
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 5 + WARM_BODIES, "target: 1 (user only)");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("a.rock", 1), ("b.rock", 1), ("d.rock", 2), ("user.rock", 1)]),
        "target: {{user.rock: 1}}"
    );
}

/// PINNED BUG (resolver, not incremental: a fresh build agrees). A glob
/// re-export of a module that binds the name through a NAMED re-export does
/// not resolve. `hub` has `pub use super::facade::*;` and `facade` has
/// `pub use super::a::bump;`: `super::hub::bump` reports a cycle and a
/// fragment globbing `hub` reports `bump` unresolved. A fragment globbing
/// `facade` directly resolves it. Mechanism (read from
/// `crates/bone-lower/src/resolve.rs`): `glob_export_members` pushes
/// `(pkg, facade, bump)` onto `visiting` before `members_of(facade, bump)`,
/// and `explicit_members` then keys facade's named export with the SAME
/// tuple, finds it already visited and returns `cycle` with nothing found.
/// The glob-opening guard and the export-binding guard share one key space.
///
/// This passes today because the bug exists. When the resolver is fixed,
/// both lookups resolve to `livev1::a::bump` with no diagnostic: flip the
/// assertions to that, and gate (f) may then use this named-export chain.
#[test]
fn sentinel_glob_reexport_of_a_named_reexport_reads_as_a_cycle() {
    let facade = "mod facade;\n\npub use super::a::bump;\n";
    let hub = "mod hub;\n\npub use super::facade::*;\n";
    let by_path = "mod user;\n\nfn pick() -> int {\n    super::hub::bump(1)\n}\n";
    let by_glob = "mod user;\n\nuse super::hub::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let direct = "mod user;\n\nuse super::facade::*;\n\nfn pick() -> int {\n    bump(1)\n}\n";
    let build = |user: &str| {
        V1Harness::new(vec![v1("a.rock", A_BUMP), v1("facade.rock", facade), v1("hub.rock", hub), v1("user.rock", user)])
    };

    let h = build(direct);
    assert!(h.diagnostics().is_empty(), "{:?}", h.diagnostics());
    assert_eq!(h.call_keys("user.rock"), vec![(key("a::bump"), None)]);

    let h = build(by_path);
    assert_eq!(h.diagnostics(), vec!["circular `use` binding with no defining target".to_string()], "BUG pinned");
    let h = build(by_glob);
    assert_eq!(h.diagnostics(), vec!["`bump` did not resolve to any definition".to_string()], "BUG pinned");
}

/// Gate (g): visibility narrowed on an item another module names by a
/// qualified path. The access check is baked in at lowering, so the user
/// must re-LOWER; re-inference alone would keep the stale verdict.
///
/// PINNED OVER-INVALIDATION: 3 user + 2 warm bodies re-infer (c's `pick`
/// now fails to LOWER with the access error, so it is in neither count nor
/// `relowered`). Target, fragment-level: a (edited, 1 body) and c (its path
/// resolved `a::bump`; it must re-lower) -- 1 re-inferred; d and the warm
/// stdlib 0.
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
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 3 + WARM_BODIES, "target: 1 (a; c fails to lower)");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("a.rock", 1), ("d.rock", 2)]),
        "target: {{a.rock: 1}}, plus c's refreshed lowering error"
    );
}

/// Gate (h): a legacy definer adds a callable the v1 package also projects to
/// legacy files, so a legacy caller's bare `bump` becomes one merged overload
/// family (`CallFamily::LegacyMerged`), decided at lowering. The two members
/// have the same signature, so the call is an overload tie at inference.
///
/// PINNED OVER-INVALIDATION: 6 user + 2 warm bodies re-infer. Target:
/// legacy_def (edited, 2 bodies) and legacy_use (its bare `bump` read the
/// projection alone; it must re-lower into the merged family) -- 3
/// re-inferred; the v1 files, legacy_far and the warm stdlib 0. Per-body: the
/// new `bump` plus `use_it`'s re-lower.
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
    assert_eq!(r.tier, UpdateTier::FullUser, "target: a scope-epoch tier, not the full rebuild");
    assert_eq!(h.reinferred_body_count(), 6 + WARM_BODIES, "target: 3 (legacy_def and legacy_use)");
    assert_eq!(
        h.functions_per_file(&r.relowered),
        per_file(&[("a.rock", 1), ("legacy_def.rock", 2), ("legacy_far.rock", 2), ("legacy_use.rock", 1)]),
        "target: {{legacy_def.rock: 2, legacy_use.rock: 1}}"
    );
}

