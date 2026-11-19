//! [`Instance`] identity over the shared [`MonoInterner`].

use crate::common::{decl, key, ws_of};
use skull_mir::instance::Instance;
use skull_mir::monoty::{MonoCtor, MonoInterner, MonoTy};

const SRC: &str = r#"
value V2 { x: float, y: float }

fn ident<T>(x: T) -> T { return x }
fn pair<A, B>(a: A, b: B) -> A { return a }
fn plain(x: int) -> int { return x + 1 }

fn main() {
    print("{ident(1)} {plain(2)}")
}
"#;

fn v2(ws: &bone_core::workspace::Workspace) -> MonoTy {
    MonoTy::App {
        ctor: MonoCtor::Nominal(key(ws, "V2")),
        params: vec![],
    }
}

fn arg_shapes(ws: &bone_core::workspace::Workspace) -> Vec<(&'static str, MonoTy)> {
    vec![
        ("scalar", MonoTy::Int),
        ("unsigned scalar", MonoTy::Uint),
        ("value struct", v2(ws)),
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
        ("nested app", MonoTy::Nullable(Box::new(v2(ws)))),
    ]
}

#[test]
fn interning_an_argument_list_round_trips_and_shares_ids_by_type() {
    let ws = ws_of(SRC);
    let ident = decl(&ws, "ident");
    let interner = MonoInterner::new();
    let shapes = arg_shapes(&ws);

    for (what, ty) in &shapes {
        let inst = Instance::intern(&interner, ident, vec![ty.clone()]);
        assert_eq!(inst.args.len(), 1, "{what}: one argument in, one id out");
        assert_eq!(
            &inst.arg_types(&interner),
            &vec![ty.clone()],
            "{what}: read back unchanged"
        );
        assert_eq!(
            inst.subst(&ws, &interner).values().collect::<Vec<_>>(),
            vec![ty],
            "{what}: substitution binds the argument unchanged"
        );
    }

    for (what, ty) in &shapes {
        let a = Instance::intern(&interner, ident, vec![ty.clone()]);
        let b = Instance::intern(&interner, ident, vec![ty.clone()]);
        assert_eq!(a, b, "{what}: one type is one id, so one instance");
    }
    assert_eq!(interner.len(), shapes.len(), "the arena shares ids by type");
}

/// `int` and `string` are both one boxed slot, so only the identity tells
/// their instances apart.
#[test]
fn distinct_arguments_are_distinct_instances_with_distinct_symbols() {
    let ws = ws_of(SRC);
    let ident = decl(&ws, "ident");
    let pair = decl(&ws, "pair");
    let interner = MonoInterner::new();
    let one = |ty| Instance::intern(&interner, ident, vec![ty]);
    let two = |a, b| Instance::intern(&interner, pair, vec![a, b]);
    for (what, x, y) in [
        ("int vs string", one(MonoTy::Int), one(MonoTy::String)),
        ("int vs uint", one(MonoTy::Int), one(MonoTy::Uint)),
        (
            "argument order",
            two(MonoTy::Int, MonoTy::String),
            two(MonoTy::String, MonoTy::Int),
        ),
    ] {
        assert_ne!(x, y, "{what}");
        assert_ne!(x.symbol(&ws, &interner), y.symbol(&ws, &interner), "{what}");
    }
    assert_eq!(
        two(MonoTy::Int, MonoTy::String).arg_types(&interner),
        vec![MonoTy::Int, MonoTy::String]
    );
}

/// Guards the tests above against an encoder that suffixes every symbol.
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
        "a non-generic instance uses the base mangling"
    );
    assert!(
        inst.subst(&ws, &interner).is_empty(),
        "nothing to substitute"
    );
}

/// A reversed zip binds the same multiset of values, so each declared generic's
/// own `VarId` is checked.
#[test]
fn an_instance_substitution_binds_each_declared_generic_to_its_own_argument() {
    let ws = ws_of(SRC);
    let pair = decl(&ws, "pair");
    let interner = MonoInterner::new();
    let inst = Instance::intern(&interner, pair, vec![MonoTy::Int, v2(&ws)]);

    let subst = inst.subst(&ws, &interner);
    assert_eq!(subst.len(), 2, "one binding per declared generic");
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
    assert_eq!(subst.get(&vars[1]), Some(&v2(&ws)), "`B` takes the second");
}
