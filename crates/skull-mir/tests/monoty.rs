//! Stage 0c properties for the total logical type.
//!
//! The generated composition is the load-bearing one: the partial tree MonoTy
//! replaced grew a `string` arm one observed symptom at a time (plan S 19b), and
//! handwritten examples are exactly what let that happen. Every constructor is composed to a
//! bounded depth here so a missing arm is found by the test rather than by a
//! miscompile.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use bone_semantic::hir::BodyOwner;
use bone_semantic::inference::Workspace;
use bone_semantic::prelude::ModuleId;
use skull_mir::monoty::{MonoArena, MonoCtor, MonoSubst, MonoTy, MonoTyReason};
use ustr::Ustr;

/// The stdlib, parsed once. Needed for anything that resolves real types.
static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

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

/// Every inferred expr + binding type of every body in the user module,
/// converted. Returns `(converted, failures)`.
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

// -- totality --------------------------------------------------------------

/// A body naming every valid constructor converts with ZERO failures. The
/// `unit`, `any`, `never` and `fn` arms are the ones the partial tree answered
/// `None` for, so this is the direct falsification of it.
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
    assert!(
        !ok.is_empty(),
        "nothing was converted -- the walk found no types"
    );
    let unique: BTreeSet<&str> = ok.iter().map(String::as_str).collect();
    let expected: BTreeSet<&str> = [
        "(int, string)",
        "Holder",
        "Map<string, int>",
        "Vec<int>",
        "any",
        "bool",
        "float",
        "fn(int) -> int",
        "fn(fn(int) -> int) -> int",
        "int",
        "int?",
        "never",
        "string",
        "uint",
        "unit",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        unique, expected,
        "valid-type denominator or membership changed"
    );
    for want in [
        "int", "float", "bool", "string", "any", "int?", "Vec<int>", "Holder",
    ] {
        assert!(
            ok.iter().any(|r| r == want),
            "missing `{want}` among {ok:?}"
        );
    }
    assert!(
        ok.iter().any(|r| r.starts_with("fn(")),
        "no function type converted -- the `Function` arm is what the partial tree lacked"
    );
}

/// Unit and never reach the converter through declared positions rather than
/// through a `let`, so they get their own body.
#[test]
fn unit_and_never_convert() {
    let mut a = MonoArena::new();
    let u = a.intern(MonoTy::Unit);
    let n = a.intern(MonoTy::Never);
    assert_eq!(a.render(u), "unit");
    assert_eq!(a.render(n), "never");
    assert_ne!(u, n, "unit and never must not share an id");
}

// -- rejection, with a reason ----------------------------------------------

/// Recovery poison rejects rather than becoming a key slot. Constructed
/// directly: a program that produces `BuiltinTy::Error` does not reach a
/// successful `semantic_pass`.
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

/// An unbound generic is its OWN bucket -- census data about shared generic
/// bodies, not an error folded in with the invalid ones.
#[test]
fn unbound_generic_is_a_distinct_reason() {
    let (ws, mid) = check("fn id<T>(x: T) -> T { return x }\nfn main() { let y = id(1) }");
    let ir = ws.inference(mid).expect("inference");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "id")
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
}

/// The same generic under a substitution converts -- the reason above is about
/// the missing binding, not about generics being unconvertible.
#[test]
fn a_bound_generic_converts_to_its_binding() {
    let (ws, mid) = check("fn id<T>(x: T) -> T { return x }\nfn main() { let y = id(1) }");
    let ir = ws.inference(mid).expect("inference");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "id")
        .expect("id");
    let inf = ir
        .per_fn
        .get(&BodyOwner::Function(fid))
        .expect("id body inference");
    let generic = ws.module(mid).functions[fid].generics[0];
    let mut a = MonoArena::new();
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

// -- injectivity -----------------------------------------------------------

/// S 20g.2, the same-shape nominal adversary: two structs with identical leaves
/// keep distinct ids. A shape-kind key would merge these, which is the whole
/// argument of plan S 4.
#[test]
fn same_shape_nominals_stay_distinct() {
    let mut a = MonoArena::new();
    let f = a.intern(MonoTy::Float);
    let vec2 = a.intern(MonoTy::App {
        ctor: MonoCtor::Nominal(Ustr::from("Vec2")),
        params: vec![],
    });
    let point = a.intern(MonoTy::App {
        ctor: MonoCtor::Nominal(Ustr::from("Point")),
        params: vec![],
    });
    assert_ne!(
        vec2, point,
        "two same-layout nominals must not share a MonoTyId"
    );
    assert_ne!(a.render(vec2), a.render(point));
    assert_ne!(a.structural_hash(vec2), a.structural_hash(point));

    // ...and the container over each stays distinct too.
    let v1 = a.app(MonoCtor::Array, &[vec2]);
    let v2 = a.app(MonoCtor::Array, &[point]);
    assert_ne!(v1, v2);
    let _ = f;
}

/// Equal structure shares an id; every axis that should separate types does.
#[test]
fn structural_equality_is_the_identity() {
    let mut a = MonoArena::new();
    let i1 = a.intern(MonoTy::Int);
    let i2 = a.intern(MonoTy::Int);
    assert_eq!(i1, i2, "interning must share");

    let f = a.intern(MonoTy::Float);
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
    // Order inside a tuple and direction of a fn arrow are both identity.
    assert_ne!(a.render(tup_if), a.render(tup_fi));
    assert_ne!(a.render(fn_i_f), a.render(fn_f_i));
}

/// The adversarial pairs a COMPOSED generator does not reach, because each pair
/// needs two specific constructors nested a specific way round. Every pair here
/// is two genuinely distinct logical types whose cross-process identity must
/// differ; interning separates them by construction, so the load-bearing
/// assertion is the one on the identity STRING and its hash.
#[test]
fn distinct_types_cannot_share_a_cross_process_identity() {
    let mut a = MonoArena::new();
    let int = a.intern(MonoTy::Int);
    let nullable_int = a.nullable(int);

    // `(fn(int) -> int)?` vs `fn(int) -> int?`. A postfix `?` on an unbracketed
    // arrow type is the ambiguity: both spell "fn(int) -> int" then "?".
    let fn_i_i = a.function(&[int], int);
    let nullable_fn = a.nullable(fn_i_i);
    let fn_i_nullable = a.function(&[int], nullable_int);

    // `(int)` the one-tuple vs `int` in brackets -- and the same one round of
    // nesting further out, which is where a naive bracket fix collides again.
    let tuple_1 = a.tuple(&[int]);
    let tuple_1_fn = a.tuple(&[fn_i_i]);
    let nullable_tuple_1_fn = a.nullable(tuple_1_fn);

    // A NOMINAL that happens to be spelled like a builtin container ctor is a
    // different type from the ctor itself.
    let vec_int = a.app(MonoCtor::Array, &[int]);
    let nominal_vec_int = a.app(MonoCtor::Nominal(Ustr::from("Vec")), &[int]);
    let map_int = a.app(MonoCtor::Map, &[int]);
    let nominal_map_int = a.app(MonoCtor::Nominal(Ustr::from("Map")), &[int]);

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
    ];
    for (what, x, y) in pairs {
        assert_ne!(x, y, "{what}: interning merged two distinct types");
        assert_ne!(
            a.canonical(x),
            a.canonical(y),
            "{what}: two distinct types share one canonical identity"
        );
        assert_ne!(
            a.structural_hash(x),
            a.structural_hash(y),
            "{what}: two distinct types share one structural hash"
        );
    }
}

// -- order independence (S 20g.4) ------------------------------------------

/// Insertion order changes the integer ids and NOTHING else. Built here as two
/// arenas that intern the same composite type through opposite orders.
#[test]
fn insertion_order_does_not_affect_identity() {
    // Arena A: leaves first, then the composite.
    let mut a = MonoArena::new();
    let ai = a.intern(MonoTy::Int);
    let af = a.intern(MonoTy::Float);
    let at = a.tuple(&[ai, af]);
    let acomposite = a.app(MonoCtor::Array, &[at]);

    // Arena B: a pile of unrelated types first, then the same composite, so the
    // ids cannot line up.
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
    assert_eq!(a.structural_hash(acomposite), b.structural_hash(bcomposite));
}

// -- generated composition (S 20g.3 / S 15a) -------------------------------

/// Every constructor composed to a bounded depth, converted through the arena.
/// This is what catches a missing arm; the handwritten tests above only catch
/// the arms someone thought of.
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

    /// Every one-level constructor applied over `kids`.
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

    // Every generated type has a non-empty display spelling, and its CANONICAL
    // encoding round-trips to the same id -- i.e. the identity is injective over
    // what we built. The display spelling is deliberately not asserted injective:
    // it is a pretty type syntax, and `distinct_types_cannot_share_a_cross_process_identity`
    // pins the pairs where it collides.
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
    // Every constructor must actually appear -- a count alone would pass while
    // one arm silently produced nothing, which is the failure this test exists
    // to catch.
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
    // Distinct renderings must mean distinct ids, which is interning's contract.
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

// -- what the corpus census found ------------------------------------------

/// A bare `null` under a `::` ascription is bound to the ascribed type, so the
/// literal converts like any other expression. This is the whole
/// `unresolved-infer` bucket the closed-world census once reported; plan Stage 1
/// ("resolve every `Infer` before conversion; reject any unresolved variable")
/// therefore rejects no valid program on this construct.
#[test]
fn null_in_a_cast_binds_to_the_ascribed_type() {
    let (ws, mid) = check("struct Prefab { n: int }\nfn main() { let e = (null :: Prefab?) }");
    let mut arena = MonoArena::new();
    let (ok, bad) = convert_module(&ws, mid, &mut arena);
    assert!(
        bad.is_empty(),
        "expected total conversion, got failures: {bad:?}"
    );
    assert!(
        ok.iter().any(|r| r == "Prefab?"),
        "expected the null literal to convert as `Prefab?`, got {ok:?}"
    );
}
