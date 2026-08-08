//! `SpecKey` and `SpecSubst` are one fact, which is the E1 architectural cut's
//! premise (`docs/monomorphization-plan.md` cut C+D, step E1).
//!
//! `EmittedBodyId` is keyed by `(FuncRef, SpecKey)` and the mono graph keys an
//! instance by `(FuncRef, MonoSubst)`. Moving emission identity onto `Instance`
//! is only a rename of today's population if those two carry the same
//! information for every body emission WRITES -- otherwise the cut silently
//! changes which definitions exist.
//!
//! It does, and E1 is what made it true. `compute_spec_from_bindings` builds the
//! key and the substitution in ONE loop from one `reify_type`, and since E1 a
//! PARTIAL key names no body at all: "EVERY complete key specializes. There is
//! no shared generic body to decline to any more", so a `None` slot is an
//! incomplete instantiation owned by the Stage-2 `ConcreteTypeUnavailable`
//! rejection rather than a body identity.
//!
//! Both reasons `abi.rs` gave for keeping them separate are therefore spent: the
//! tuple reason was stale (`MonoTy::Tuple` has existed since D-2), and the
//! partial-key reason no longer describes a body.

use std::sync::LazyLock;

use skull_mir::lower::{spec_key_from_subst, spec_subst_from_key, EmittedBodyId, SpecKey};
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoTy};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn ws_of(src: &str) -> bone_semantic::inference::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

const SRC: &str = r#"
value V2 { x: float, y: float }

fn ident<T>(x: T) -> T { return x }
fn pair<A, B>(a: A, b: B) -> A { return a }

fn main() {
    print("{ident(1)}")
}
"#;

fn decl(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .map(|(fid, _)| fid)
        .unwrap_or_else(|| panic!("no fn `{name}`"));
    FuncRef { module: mid, func: fid }
}

/// Every key shape a body identity can take, including the ones the two
/// directions could plausibly disagree on: a tuple (the stale reason `abi.rs`
/// gave), a nullable, a nested application, and a MULTI-generic declaration
/// where positional order is the only thing a substitution does not carry.
fn key_shapes() -> Vec<(&'static str, SpecKey)> {
    let v2 = MonoTy::App { ctor: MonoCtor::Nominal(Ustr::from("V2")), params: vec![] };
    vec![
        ("scalar", vec![Some(MonoTy::Int)]),
        ("value struct", vec![Some(v2.clone())]),
        ("tuple", vec![Some(MonoTy::Tuple(vec![MonoTy::Int, MonoTy::Float]))]),
        ("nullable", vec![Some(MonoTy::Nullable(Box::new(MonoTy::Int)))]),
        ("unit", vec![Some(MonoTy::Unit)]),
        ("any", vec![Some(MonoTy::Any)]),
        (
            "function",
            vec![Some(MonoTy::Function {
                params: vec![MonoTy::Int],
                ret: Box::new(MonoTy::Bool),
            })],
        ),
        ("nested app", vec![Some(MonoTy::Nullable(Box::new(v2)))]),
    ]
}

/// key -> subst -> key is the identity, for every shape a body identity takes.
///
/// The population is asserted, because this is exactly the test that reads as a
/// pass over an empty set: a `key_shapes()` that returned nothing, or a rebuild
/// that answered `None` everywhere and was compared against `None`, would both
/// look green.
#[test]
fn a_complete_key_survives_the_round_trip_through_its_substitution() {
    let ws = ws_of(SRC);
    let ident = decl(&ws, "ident");
    let shapes = key_shapes();
    assert_eq!(shapes.len(), 8, "the shape table is the population under test");
    for (what, key) in shapes {
        let subst = spec_subst_from_key(&ws, ident, &key);
        assert_eq!(
            subst.len(),
            1,
            "{what}: a one-generic declaration must bind exactly one var, or the \
             round trip below is measuring an empty substitution"
        );
        let back = spec_key_from_subst(&ws, ident, &subst)
            .unwrap_or_else(|| panic!("{what}: a complete substitution must rebuild its key"));
        assert_eq!(back, key, "{what}: the key did not survive the round trip");
        // The body identity is what actually has to be stable -- two keys that
        // compare equal but build different ids would defeat the point.
        assert_eq!(
            EmittedBodyId::new(ident, back),
            EmittedBodyId::new(ident, key),
            "{what}: the rebuilt key names a different emitted body"
        );
    }
}

/// ORDER is the one thing a substitution does not carry, so a multi-generic
/// declaration is the case that would expose a rebuild reading `f.generics` the
/// wrong way round. `(int, string)` and `(string, int)` are distinct bodies.
#[test]
fn a_multi_generic_key_keeps_its_positions() {
    let ws = ws_of(SRC);
    let pair = decl(&ws, "pair");
    let ab: SpecKey = vec![Some(MonoTy::Int), Some(MonoTy::String)];
    let ba: SpecKey = vec![Some(MonoTy::String), Some(MonoTy::Int)];
    let ab_back = spec_key_from_subst(&ws, pair, &spec_subst_from_key(&ws, pair, &ab))
        .expect("complete substitution rebuilds");
    let ba_back = spec_key_from_subst(&ws, pair, &spec_subst_from_key(&ws, pair, &ba))
        .expect("complete substitution rebuilds");
    assert_eq!(ab_back, ab, "positions swapped on the round trip");
    assert_eq!(ba_back, ba, "positions swapped on the round trip");
    assert_ne!(
        EmittedBodyId::new(pair, ab_back),
        EmittedBodyId::new(pair, ba_back),
        "two orderings of one generic list must be two emitted bodies"
    );
}

/// A PARTIAL substitution rebuilds no key, rather than a short or padded one.
///
/// Since E1 a partial key names no body, so the honest answer is `None`. A
/// rebuild that padded the gap would mint an identity for a body that does not
/// exist -- and one that silently truncated would make two distinct
/// instantiations share an id.
#[test]
fn an_incomplete_substitution_rebuilds_no_key() {
    let ws = ws_of(SRC);
    let pair = decl(&ws, "pair");
    // Bind only the FIRST generic, by round-tripping a key that names one and
    // dropping the other's binding.
    let full: SpecKey = vec![Some(MonoTy::Int), Some(MonoTy::String)];
    let mut subst = spec_subst_from_key(&ws, pair, &full);
    assert_eq!(subst.len(), 2, "the fixture must start complete");
    let victim = *subst.keys().next().expect("a bound var");
    subst.remove(&victim);
    assert_eq!(subst.len(), 1, "exactly one binding removed");
    assert!(
        spec_key_from_subst(&ws, pair, &subst).is_none(),
        "a substitution missing a declared generic must rebuild NO key"
    );
}

/// The arm of `EmittedBodyId::new`'s totality assertion. A partial key names
/// neither the shared body nor one instantiation, and the corpus produces none
/// (267,671 total / 756 all-`None` / 0 partial over the 18 bin projects plus
/// `tests/`) -- so without this the assertion could be vacuous and nothing
/// would say.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "total or empty")]
fn a_partial_specialization_key_names_no_body() {
    let ws = ws_of(SRC);
    let partial: SpecKey = vec![Some(skull_mir::monoty::MonoTy::Int), None];
    let _ = EmittedBodyId::new(decl(&ws, "pair"), partial);
}
