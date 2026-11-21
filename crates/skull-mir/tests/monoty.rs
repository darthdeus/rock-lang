use crate::common::STDLIB;

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
