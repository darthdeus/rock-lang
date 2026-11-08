//! [`Instance`] is the ONE concrete-body identity, and [`MonoInterner`] is the
//! one arena that makes it one.
//!
//! `spec_key_subst_roundtrip.rs` establishes that `(FuncRef, SpecKey)` and
//! `(FuncRef, MonoSubst)` carry the same information. This file is the other
//! half: that going through the ARENA -- which is what turns those into an
//! identity with cheap equality -- loses nothing either, and that the symbol an
//! instance publishes is the same encoder emission already spells a body with.
//!
//! Every arm here asserts its own population. A bridge that answered nothing,
//! or a shape table that was empty, would otherwise read as a pass.

use std::sync::LazyLock;

use skull_mir::instance::Instance;
use skull_mir::mir::FuncRef;
use skull_mir::monoty::{MonoCtor, MonoInterner, MonoTy};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn ws_of(src: &str) -> bone_core::workspace::Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

const SRC: &str = r#"
value V2 { x: float, y: float }

fn ident<T>(x: T) -> T { return x }
fn pair<A, B>(a: A, b: B) -> A { return a }
fn plain(x: int) -> int { return x + 1 }

fn main() {
    print("{ident(1)} {plain(2)}")
}
"#;

fn decl(ws: &bone_core::workspace::Workspace, name: &str) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .map(|(fid, _)| fid)
        .unwrap_or_else(|| panic!("no fn `{name}`"));
    FuncRef {
        module: mid,
        func: fid,
    }
}

fn v2() -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(Ustr::from("V2")),
        params: vec![],
    }
}

/// The same shape table `spec_key_subst_roundtrip.rs` uses, so the two halves of
/// the bridge are measured over one population rather than over whichever shapes
/// each file happened to think of.
fn arg_shapes() -> Vec<(&'static str, MonoTy)> {
    vec![
        ("scalar", MonoTy::Int),
        ("unsigned scalar", MonoTy::Uint),
        ("value struct", v2()),
        ("tuple", MonoTy::Tuple(vec![MonoTy::Int, MonoTy::Float])),
        ("nullable", MonoTy::Nullable(Box::new(MonoTy::Int))),
        ("unit", MonoTy::Unit),
        ("any", MonoTy::Any),
        (
            "function",
            MonoTy::Function {
                params: vec![MonoTy::Int],
                ret: Box::new(MonoTy::Bool),
            },
        ),
        ("nested app", MonoTy::Nullable(Box::new(v2()))),
    ]
}

/// Interning and reading back is the identity, over every shape an argument
/// takes.
///
/// The arm that matters is the LAST one: equal ids for equal types is what makes
/// `Instance`'s `Eq` a type equality rather than an insertion-order accident, and
/// a bridge that appended unconditionally would satisfy the round trip and fail
/// this.
#[test]
fn interning_an_argument_list_round_trips_and_shares_ids_by_type() {
    let ws = ws_of(SRC);
    let ident = decl(&ws, "ident");
    let interner = MonoInterner::new();
    let shapes = arg_shapes();
    assert_eq!(
        shapes.len(),
        9,
        "the shape table is the population under test"
    );

    for (what, ty) in &shapes {
        let inst = Instance::intern(&interner, ident, vec![ty.clone()]);
        assert_eq!(inst.args.len(), 1, "{what}: one argument in, one id out");
        assert_eq!(
            &inst.arg_types(&interner),
            &vec![ty.clone()],
            "{what}: read back unchanged"
        );
    }

    for (what, ty) in &shapes {
        let a = Instance::intern(&interner, ident, vec![ty.clone()]);
        let b = Instance::intern(&interner, ident, vec![ty.clone()]);
        assert_eq!(a, b, "{what}: one type is one id, so one instance");
    }
    assert_eq!(
        interner.len(),
        shapes.len(),
        "9 distinct argument types over 18 interns -- the arena shares by type"
    );
}

/// Two instances of one declaration whose ABI WIDTHS are equal are two
/// identities.
///
/// This is the property declaration ownership got wrong, taken at the identity
/// layer rather than at the emitted text: `int` and `string` are one boxed slot
/// each, so nothing about the wire distinguishes them and only the identity
/// does.
#[test]
fn equal_abi_arguments_are_distinct_instances_with_distinct_symbols() {
    let ws = ws_of(SRC);
    let ident = decl(&ws, "ident");
    let interner = MonoInterner::new();

    let i = Instance::intern(&interner, ident, vec![MonoTy::Int]);
    let u = Instance::intern(&interner, ident, vec![MonoTy::Uint]);
    let s = Instance::intern(&interner, ident, vec![MonoTy::String]);
    assert_ne!(
        i, s,
        "two concrete types are two instances whatever they splat to"
    );
    assert_ne!(
        i, u,
        "signed and unsigned scalars are distinct concrete instances"
    );
    assert_ne!(
        i.symbol(&ws, &interner),
        s.symbol(&ws, &interner),
        "and two instances are two symbols"
    );
    assert_ne!(
        i.symbol(&ws, &interner),
        u.symbol(&ws, &interner),
        "signed and unsigned scalars have distinct symbols"
    );
}

/// Argument ORDER is part of the identity. A multi-generic declaration is the
/// only place a positional list can be scrambled without changing its contents,
/// and a set-shaped or sorted encoding would give `pair<int, string>` and
/// `pair<string, int>` one body.
#[test]
fn argument_order_is_part_of_the_identity() {
    let ws = ws_of(SRC);
    let pair = decl(&ws, "pair");
    let interner = MonoInterner::new();

    let ab = Instance::intern(&interner, pair, vec![MonoTy::Int, MonoTy::String]);
    let ba = Instance::intern(&interner, pair, vec![MonoTy::String, MonoTy::Int]);
    assert_ne!(ab, ba);
    assert_ne!(ab.symbol(&ws, &interner), ba.symbol(&ws, &interner));
    assert_eq!(
        ab.arg_types(&interner),
        vec![MonoTy::Int, MonoTy::String],
        "read back in declaration order"
    );
}

/// A non-generic declaration's instance is its ordinary symbol, and its
/// substitution is empty.
///
/// ADMISSION CONTROL for the arms above: "distinct instances get distinct
/// symbols" is equally satisfied by an encoder that suffixed everything, which
/// would rename every non-generic function in the program.
#[test]
fn a_non_generic_instance_is_the_declarations_own_symbol() {
    let ws = ws_of(SRC);
    let plain = decl(&ws, "plain");
    let interner = MonoInterner::new();

    let inst = Instance::base(plain);
    assert!(inst.args.is_empty());
    assert!(!inst.is_generic(&ws), "`plain` declares no generics");
    let f = &ws.module(plain.module).functions[plain.func];
    assert_eq!(
        inst.symbol(&ws, &interner),
        skull_mir::mangle::mangle_fn(&ws, f),
        "a non-generic instance IS the base mangling, not a suffixed derivation of it"
    );
    assert!(
        inst.subst(&ws, &interner).is_empty(),
        "nothing to substitute"
    );
}

/// An instance's substitution binds the declaration's own generic `VarId`s, in
/// declaration order, to the arguments it was interned with.
///
/// The population is the multi-generic case, because that is the only one where
/// a zip against the wrong order stays the right SIZE.
#[test]
fn an_instance_substitution_binds_each_declared_generic_to_its_own_argument() {
    let ws = ws_of(SRC);
    let pair = decl(&ws, "pair");
    let interner = MonoInterner::new();
    let inst = Instance::intern(&interner, pair, vec![MonoTy::Int, v2()]);

    let subst = inst.subst(&ws, &interner);
    assert_eq!(subst.len(), 2, "one binding per declared generic");
    // Resolve each declared generic's own `VarId` and check IT, rather than
    // checking the value multiset -- a reversed zip has the same values.
    let f = &ws.module(pair.module).functions[pair.func];
    let vars: Vec<_> = f
        .generics
        .iter()
        .map(|g| {
            match ws.module(pair.module).ty_arena[match g {
                bone_core::prelude::TyId::Static(_, idx) => *idx,
                other => panic!("declared generic is not static: {other:?}"),
            }] {
                bone_core::hir::TypeExpr::Generic(var, _) => var,
                ref other => panic!("declared generic is not a Generic: {other:?}"),
            }
        })
        .collect();
    assert_eq!(vars.len(), 2);
    assert_eq!(
        subst.get(&vars[0]),
        Some(&MonoTy::Int),
        "`A` takes the first argument"
    );
    assert_eq!(subst.get(&vars[1]), Some(&v2()), "`B` takes the second");
}
