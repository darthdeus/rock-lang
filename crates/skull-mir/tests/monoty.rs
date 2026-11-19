//! Totality and identity of the logical type `MonoTy`.

use crate::common::{STDLIB, key};
use std::collections::BTreeSet;

use bone_core::hir::BodyOwner;
use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_mir::monoty::{MonoArena, MonoCtor, MonoSubst, MonoTy, MonoTyReason};
use ustr::Ustr;

fn check(src: &str) -> (Workspace, ModuleId) {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws
        .module_ids
        .get(&Ustr::from("<test>"))
        .expect("user module id");
    (ws, mid)
}

fn convert_module(
    ws: &Workspace,
    mid: ModuleId,
    arena: &mut MonoArena,
) -> (Vec<String>, Vec<MonoTyReason>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    let Some(ir) = ws.inference(mid) else {
        return (ok, bad);
    };
    let subst = MonoSubst::default();
    for inf in ir.per_fn.values() {
        let tys = inf
            .expr_types
            .iter()
            .map(|(_, t)| *t)
            .chain(inf.binding_types.iter().map(|(_, t)| *t));
        for t in tys {
            match arena.convert(ws, inf, t, &subst) {
                Ok(id) => ok.push(arena.render(id)),
                Err(r) => bad.push(r),
            }
        }
    }
    (ok, bad)
}

#[test]
fn every_valid_constructor_converts() {
    let (ws, mid) = check(
        r#"
struct Holder { n: int }
fn takes_fn(f: fn(int) -> int) -> int { return f(1) }
fn ret_unit() { }
fn main() {
    let i: int = 1
    let f: float = 1.0
    let b: bool = true
    let s: string = "s"
    let u: uint = 1 as uint
    let a: any = 1
    let n: int? = 1
    let t: (int, string) = (1, "s")
    let v: [int] = [1]
    let m: Map<string, int> = map_new()
    let h: Holder = Holder { n: 1 }
    let g: fn(int) -> int = takes_fn
    ret_unit()
}
"#,
    );
    let mut arena = MonoArena::new();
    let (ok, bad) = convert_module(&ws, mid, &mut arena);
    assert!(
        bad.is_empty(),
        "expected total conversion, got failures: {bad:?}"
    );
    let unique: BTreeSet<String> = ok.iter().cloned().collect();
    let expected: BTreeSet<String> = [
        "(int, string)".to_string(),
        key(&ws, "Holder").to_string(),
        "Map<string, int>".to_string(),
        "Vec<int>".to_string(),
        "any".to_string(),
        "bool".to_string(),
        "float".to_string(),
        "fn(int) -> int".to_string(),
        "fn(fn(int) -> int) -> int".to_string(),
        "int".to_string(),
        "int?".to_string(),
        "never".to_string(),
        "string".to_string(),
        "uint".to_string(),
        "unit".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        unique, expected,
        "valid-type denominator or membership changed"
    );
}

/// A program that produces `BuiltinTy::Error` does not pass semantic checks, so
/// the poison type is converted directly.
#[test]
fn recovery_error_rejects_with_its_own_reason() {
    let (ws, mid) = check("fn main() { let x: int = 1 }");
    let ir = ws.inference(mid).expect("inference");
    let (_, inf) = ir.per_fn.iter().next().expect("a body");
    let mut a = MonoArena::new();
    let err = a
        .convert(&ws, inf, ws.builtins.error(), &MonoSubst::default())
        .expect_err("recovery poison must reject");
    assert_eq!(err, MonoTyReason::RecoveryError);
    assert_eq!(err.bucket(), "recovery-error");
}

#[test]
fn a_generic_converts_only_under_a_binding() {
    let (ws, mid) = check("fn id<T>(x: T) -> T { return x }\nfn main() { let y = id(1) }");
    let ir = ws.inference(mid).expect("inference");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str().rsplit("::").next() == Some("id"))
        .expect("id");
    let inf = ir
        .per_fn
        .get(&BodyOwner::Function(fid))
        .expect("id body inference");
    let generic = ws.module(mid).functions[fid].generics[0];
    let mut a = MonoArena::new();

    let err = a
        .convert(&ws, inf, generic, &MonoSubst::default())
        .expect_err("an unbound generic cannot convert");
    assert_eq!(err.bucket(), "unbound-generic");
    assert!(matches!(err, MonoTyReason::UnboundGeneric(n) if n.as_str() == "T"));

    let int = a.intern(MonoTy::Int);
    let var = skull_mir::monoty::generic_var_of(&ws, inf, generic)
        .expect("the declared generic is a Generic node");
    let mut subst = MonoSubst::default();
    subst.insert(var, MonoTy::Int);
    let got = a
        .convert(&ws, inf, generic, &subst)
        .expect("bound generic converts");
    assert_eq!(got, int);
}

/// Equal structure shares an id; every axis that should separate types does.
#[test]
fn structural_equality_is_the_identity() {
    let mut a = MonoArena::new();
    let i1 = a.intern(MonoTy::Int);
    let i2 = a.intern(MonoTy::Int);
    assert_eq!(i1, i2, "interning must share");

    let f = a.intern(MonoTy::Float);
    let unit = a.intern(MonoTy::Unit);
    let never = a.intern(MonoTy::Never);
    let nullable_int = a.nullable(i1);
    let tup_if = a.tuple(&[i1, f]);
    let tup_fi = a.tuple(&[f, i1]);
    let vec_int = a.app(MonoCtor::Array, &[i1]);
    let map_int = a.app(MonoCtor::Map, &[i1]);
    let fn_i_f = a.function(&[i1], f);
    let fn_f_i = a.function(&[f], i1);

    let all = [
        i1,
        f,
        unit,
        never,
        nullable_int,
        tup_if,
        tup_fi,
        vec_int,
        map_int,
        fn_i_f,
        fn_f_i,
    ];
    for (x, a_id) in all.iter().enumerate() {
        for (y, b_id) in all.iter().enumerate() {
            if x != y {
                assert_ne!(a_id, b_id, "distinct types collided at {x}/{y}");
            }
        }
    }
    assert_ne!(a.render(tup_if), a.render(tup_fi));
    assert_ne!(a.render(fn_i_f), a.render(fn_f_i));
}

/// Pairs of distinct types whose display spellings are easy to confuse.
#[test]
fn distinct_types_cannot_share_a_cross_process_identity() {
    let mut a = MonoArena::new();
    let int = a.intern(MonoTy::Int);
    let nullable_int = a.nullable(int);

    let fn_i_i = a.function(&[int], int);
    let nullable_fn = a.nullable(fn_i_i);
    let fn_i_nullable = a.function(&[int], nullable_int);

    let tuple_1 = a.tuple(&[int]);
    let tuple_1_fn = a.tuple(&[fn_i_i]);
    let nullable_tuple_1_fn = a.nullable(tuple_1_fn);

    let vec_int = a.app(MonoCtor::Array, &[int]);
    let nominal_vec_int = a.app(MonoCtor::Nominal(Ustr::from("Vec")), &[int]);
    let map_int = a.app(MonoCtor::Map, &[int]);
    let nominal_map_int = a.app(MonoCtor::Nominal(Ustr::from("Map")), &[int]);
    let vec2 = a.app(MonoCtor::Nominal(Ustr::from("Vec2")), &[]);
    let point = a.app(MonoCtor::Nominal(Ustr::from("Point")), &[]);
    let vec_of_vec2 = a.app(MonoCtor::Array, &[vec2]);
    let vec_of_point = a.app(MonoCtor::Array, &[point]);

    let pairs = [
        (
            "nullable fn vs fn returning nullable",
            nullable_fn,
            fn_i_nullable,
        ),
        (
            "nullable tuple-of-fn vs nullable fn",
            nullable_tuple_1_fn,
            nullable_fn,
        ),
        ("one-tuple vs its element", tuple_1, int),
        ("Vec ctor vs a nominal named Vec", vec_int, nominal_vec_int),
        ("Map ctor vs a nominal named Map", map_int, nominal_map_int),
        ("same-layout nominals", vec2, point),
        ("arrays of same-layout nominals", vec_of_vec2, vec_of_point),
    ];
    for (what, x, y) in pairs {
        assert_ne!(x, y, "{what}: interning merged two distinct types");
        assert_ne!(
            a.canonical(x),
            a.canonical(y),
            "{what}: two distinct types share one canonical identity"
        );
    }
}

#[test]
fn insertion_order_does_not_affect_identity() {
    let mut a = MonoArena::new();
    let ai = a.intern(MonoTy::Int);
    let af = a.intern(MonoTy::Float);
    let at = a.tuple(&[ai, af]);
    let acomposite = a.app(MonoCtor::Array, &[at]);

    let mut b = MonoArena::new();
    b.intern(MonoTy::String);
    b.intern(MonoTy::Bool);
    b.intern(MonoTy::Any);
    b.intern(MonoTy::Never);
    let bf = b.intern(MonoTy::Float);
    let bi = b.intern(MonoTy::Int);
    let bt = b.tuple(&[bi, bf]);
    let bcomposite = b.app(MonoCtor::Array, &[bt]);

    assert_ne!(
        acomposite.index(),
        bcomposite.index(),
        "the test is vacuous unless the integer ids actually differ"
    );
    assert_eq!(a.render(acomposite), b.render(bcomposite));
    assert_eq!(a.canonical(acomposite), b.canonical(bcomposite));
}

#[test]
fn generated_composition_to_depth_three_is_total() {
    fn leaves(a: &mut MonoArena) -> Vec<skull_mir::monoty::MonoTyId> {
        vec![
            a.intern(MonoTy::Int),
            a.intern(MonoTy::Float),
            a.intern(MonoTy::Bool),
            a.intern(MonoTy::String),
            a.intern(MonoTy::Any),
            a.intern(MonoTy::Unit),
            a.intern(MonoTy::Never),
            a.app(MonoCtor::Nominal(Ustr::from("Vec2")), &[]),
        ]
    }

    fn grow(
        a: &mut MonoArena,
        kids: &[skull_mir::monoty::MonoTyId],
    ) -> Vec<skull_mir::monoty::MonoTyId> {
        let mut out = Vec::new();
        for &k in kids {
            out.push(a.nullable(k));
            out.push(a.app(MonoCtor::Array, &[k]));
            out.push(a.function(&[k], k));
        }
        for &x in kids.iter().take(4) {
            for &y in kids.iter().take(4) {
                out.push(a.tuple(&[x, y]));
                out.push(a.app(MonoCtor::Map, &[x, y]));
                out.push(a.app(MonoCtor::Nominal(Ustr::from("Pair")), &[x, y]));
                out.push(a.function(&[x, y], x));
            }
        }
        out
    }

    let mut a = MonoArena::new();
    let mut level = leaves(&mut a);
    let mut every = level.clone();
    for _ in 0..3 {
        level = grow(&mut a, &level);
        every.extend(level.iter().copied());
    }

    // The display spelling is not injective; the canonical encoding must be.
    let mut seen: std::collections::HashMap<String, skull_mir::monoty::MonoTyId> =
        std::collections::HashMap::new();
    for id in &every {
        assert!(
            !a.render(*id).is_empty(),
            "a generated type displayed empty"
        );
        let c = a.canonical(*id);
        if let Some(prev) = seen.insert(c.clone(), *id) {
            assert_eq!(
                prev, *id,
                "two distinct ids share one canonical encoding: `{c}`"
            );
        }
    }
    let rendered: Vec<String> = every.iter().map(|id| a.render(*id)).collect();
    for (what, hit) in [
        ("nullable", rendered.iter().any(|r| r.ends_with('?'))),
        ("tuple", rendered.iter().any(|r| r.starts_with('('))),
        ("array", rendered.iter().any(|r| r.starts_with("Vec<"))),
        ("map", rendered.iter().any(|r| r.starts_with("Map<"))),
        (
            "nominal+params",
            rendered.iter().any(|r| r.starts_with("Pair<")),
        ),
        ("function", rendered.iter().any(|r| r.starts_with("fn("))),
        ("never", rendered.iter().any(|r| r == "never")),
        ("unit", rendered.iter().any(|r| r == "unit")),
        ("any", rendered.iter().any(|r| r == "any")),
    ] {
        assert!(hit, "the generator produced no `{what}` type");
    }
    assert!(
        every.len() > 1000,
        "the generator collapsed -- only {} types",
        every.len()
    );
    assert_eq!(
        seen.len(),
        every
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "the canonical encoding is not injective over the generated set"
    );
}

#[test]
fn null_in_a_cast_binds_to_the_ascribed_type() {
    let (ws, mid) = check("struct Prefab { n: int }\nfn main() { let e = (null : Prefab?) }");
    let mut arena = MonoArena::new();
    let (ok, bad) = convert_module(&ws, mid, &mut arena);
    assert!(
        bad.is_empty(),
        "expected total conversion, got failures: {bad:?}"
    );
    let prefab = format!("{}?", key(&ws, "Prefab"));
    assert!(
        ok.iter().any(|r| r == &prefab),
        "expected the null literal to convert as `{prefab}`, got {ok:?}"
    );
}
