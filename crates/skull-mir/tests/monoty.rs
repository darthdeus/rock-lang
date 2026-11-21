//! Totality and identity of the logical type `MonoTy`.

use crate::common::{STDLIB, key};
use std::collections::BTreeSet;

use bone_core::hir::BodyOwner;
use bone_core::prelude::ModuleId;
use bone_core::workspace::Workspace;
use skull_mir::monoty::{MonoCtor, MonoInterner, MonoSubst, MonoTy, MonoTyReason};
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
    arena: &MonoInterner,
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
    let arena = MonoInterner::new();
    let (ok, bad) = convert_module(&ws, mid, &arena);
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
    let a = MonoInterner::new();
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
    let a = MonoInterner::new();

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

fn app(ctor: MonoCtor, params: impl Into<Vec<MonoTy>>) -> MonoTy {
    MonoTy::App {
        ctor,
        params: params.into(),
    }
}

fn function(params: impl Into<Vec<MonoTy>>, ret: MonoTy) -> MonoTy {
    MonoTy::Function {
        params: params.into(),
        ret: Box::new(ret),
    }
}

/// Equal structure shares an id; every axis that should separate types does.
#[test]
fn structural_equality_is_the_identity() {
    let a = MonoInterner::new();
    use MonoTy::{Float, Int};
    let types = [
        Int,
        Float,
        MonoTy::Unit,
        MonoTy::Never,
        MonoTy::nullable(Int),
        MonoTy::Tuple(vec![Int, Float]),
        MonoTy::Tuple(vec![Float, Int]),
        app(MonoCtor::Array, [Int]),
        app(MonoCtor::Map, [Int]),
        function([Int], Float),
        function([Float], Int),
    ];
    let all = a.intern_all(types);
    assert_eq!(all[0], a.intern(Int), "interning must share");
    for (i, id) in all.iter().enumerate() {
        assert!(!all[..i].contains(id), "distinct types collided at {i}");
    }
    assert_ne!(a.render(all[5]), a.render(all[6]));
    assert_ne!(a.render(all[9]), a.render(all[10]));
}

/// Pairs of distinct types whose display spellings are easy to confuse.
#[test]
fn distinct_types_cannot_share_a_cross_process_identity() {
    let a = MonoInterner::new();
    let int = MonoTy::Int;
    let fn_i_i = function([int.clone()], int.clone());
    let nullable_fn = MonoTy::nullable(fn_i_i.clone());
    let fn_i_nullable = function([int.clone()], MonoTy::nullable(int.clone()));
    let tuple_1 = MonoTy::Tuple(vec![int.clone()]);
    let nullable_tuple_1_fn = MonoTy::nullable(MonoTy::Tuple(vec![fn_i_i]));
    let vec_int = app(MonoCtor::Array, [int.clone()]);
    let nominal_vec_int = app(MonoCtor::Nominal(Ustr::from("Vec")), [int.clone()]);
    let map_int = app(MonoCtor::Map, [int.clone()]);
    let nominal_map_int = app(MonoCtor::Nominal(Ustr::from("Map")), [int.clone()]);
    let vec2 = app(MonoCtor::Nominal(Ustr::from("Vec2")), []);
    let point = app(MonoCtor::Nominal(Ustr::from("Point")), []);
    let vec_of_vec2 = app(MonoCtor::Array, [vec2.clone()]);
    let vec_of_point = app(MonoCtor::Array, [point.clone()]);
    let pairs = [
        (
            "nullable fn vs fn returning nullable",
            nullable_fn.clone(),
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
        let (x, y) = (a.intern(x), a.intern(y));
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
    let a = MonoInterner::new();
    let b = MonoInterner::new();
    a.intern_all([MonoTy::Int, MonoTy::Float]);
    b.intern_all([
        MonoTy::String,
        MonoTy::Bool,
        MonoTy::Any,
        MonoTy::Never,
        MonoTy::Float,
        MonoTy::Int,
    ]);
    let composite = app(
        MonoCtor::Array,
        [MonoTy::Tuple(vec![MonoTy::Int, MonoTy::Float])],
    );
    let acomposite = a.intern(composite.clone());
    let bcomposite = b.intern(composite);

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
    fn grow(kids: &[MonoTy]) -> Vec<MonoTy> {
        let mut out = Vec::new();
        for k in kids {
            out.extend([
                MonoTy::nullable(k.clone()),
                app(MonoCtor::Array, [k.clone()]),
                function([k.clone()], k.clone()),
            ]);
        }
        for x in kids.iter().take(4) {
            for y in kids.iter().take(4) {
                let pair = [x.clone(), y.clone()];
                out.extend([
                    MonoTy::Tuple(pair.to_vec()),
                    app(MonoCtor::Map, pair.clone()),
                    app(MonoCtor::Nominal(Ustr::from("Pair")), pair.clone()),
                    function(pair, x.clone()),
                ]);
            }
        }
        out
    }

    let a = MonoInterner::new();
    let mut level = vec![
        MonoTy::Int,
        MonoTy::Float,
        MonoTy::Bool,
        MonoTy::String,
        MonoTy::Any,
        MonoTy::Unit,
        MonoTy::Never,
        app(MonoCtor::Nominal(Ustr::from("Vec2")), []),
    ];
    let mut every = a.intern_all(level.iter().cloned());
    for _ in 0..3 {
        level = grow(&level);
        every.extend(a.intern_all(level.iter().cloned()));
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
    let arena = MonoInterner::new();
    let (ok, bad) = convert_module(&ws, mid, &arena);
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
