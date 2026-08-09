//! The call-side scalar-ABI arity gate (`check_scalar_abi`).
//!
//! A value-struct argument scalarizes to N leaf values at the call while a
//! co-emitted callee that uses the param as a WHOLE value keeps one MIR param,
//! so `argc > arity` silently drops leaves and must reject. `argc` BELOW the
//! arity is the legitimate omitted-defaulted-argument shape and must be
//! admitted -- and the omittable amount is a LEAF WIDTH, not one per param,
//! since a defaulted value-struct param explodes to several slots.
//!
//! The gate asks that question of the EMITTED BODY, which is a shared body or a
//! specialized one. Both arms exist because a specialized callee takes the
//! scalar ABI its own instantiation implies: measuring a spec-bound call against
//! the shared body's arity reads every correct specialized call as a bug.

use skull_mir::lower::ScalarCtx;
use skull_mir::mir::FuncRef;

use crate::common::ws_with;

fn emit_res(src: &str, root: &str) -> Result<String, String> {
    let ws = ws_with(src);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == root)
        .map(|(fid, _)| fid)
        .expect("root fn");
    skull_codegen_js::emit::emit_with_closure(
        &ws,
        &[(FuncRef { module: mid, func: fid }, root.to_string())],
    )
    .map_err(|e| e.message)
}

/// A generic fn with a trailing defaulted scalar, called at a VALUE-STRUCT
/// instantiation so the callee is served by a specialized copy: `v` becomes two
/// leaf slots, so the specialized body has three params and the call passes two.
const OMITTED_DEFAULT_AT_SPEC: &str = r#"
value V2 { x: float, y: float }

fn take<T>(v: T, k: float = 2.0) -> float { return k }

fn main() {
    let a = V2 { x: 1.0, y: 2.0 }
    print("{take(a)}")
}
"#;

/// The same shape with the default PASSED. The control for the test below: it
/// shares every other property, so a rejection there cannot be blamed on
/// generics, value structs or specialization.
const EXPLICIT_ARG_AT_SPEC: &str = r#"
value V2 { x: float, y: float }

fn take<T>(v: T, k: float = 2.0) -> float { return k }

fn main() {
    let a = V2 { x: 1.0, y: 2.0 }
    print("{take(a, 3.0)}")
}
"#;

/// An omitted defaulted argument is admitted at a SPECIALIZED callee.
///
/// The shared-body arm allowed `required..=arity` and the specialized arm
/// demanded `argc == arity`, so the one shape that only exists at a
/// specialization -- a defaulted param behind a scalarized value-struct one --
/// rejected. luajit compiled and ran the same source correctly, which is what a
/// rejection on one emitter and not its sibling always looks like.
#[test]
fn an_omitted_defaulted_arg_is_admitted_at_a_specialized_callee() {
    if let Err(m) = emit_res(OMITTED_DEFAULT_AT_SPEC, "main") {
        panic!("an omitted defaulted arg must be admitted at a specialization: {m}");
    }
}

/// The admission control: the same call with the argument supplied. Without it
/// the test above could pass because the gate stopped rejecting anything.
#[test]
fn the_same_call_with_the_default_supplied_is_admitted() {
    if let Err(m) = emit_res(EXPLICIT_ARG_AT_SPEC, "main") {
        panic!("a fully-applied specialized call must be admitted: {m}");
    }
}

/// What bounds the widening: the window is `required..=arity`, and `required`
/// is the arity MINUS the omittable defaults, so a callee with NO defaults still
/// demands an exact count.
///
/// This is the input that could now pass incorrectly. Before the change the
/// specialized arm demanded `argc == arity`; after it, `argc` below the arity is
/// admitted -- but only as far as the defaults reach. `pair` has none, so its
/// window is a single value and a short call is still a dropped-leaf bug.
#[test]
fn a_specialized_callee_without_defaults_still_demands_an_exact_count() {
    let ws = ws_with(
        r#"
value V2 { x: float, y: float }

fn pair<T>(v: T, k: float) -> float { return k }

fn main() {
    let a = V2 { x: 1.0, y: 2.0 }
    print("{pair(a, 1.0)}")
}
"#,
    );
    let scx = ScalarCtx::new(&ws).with_target(bone_semantic::hir::FnBackend::Js);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "pair")
        .map(|(fid, _)| fid)
        .expect("pair");
    // The window is read off the ABI rather than off a rejection, because the
    // rejecting input is not constructible from source: a short call is a type
    // error bone refuses long before emission, so a fixture "proving" the
    // rejection would be measuring bone. What IS checkable here is that the
    // admission window did not widen for a callee with nothing to omit.
    let fr = FuncRef { module: mid, func: fid };
    let body = skull_mir::instance::Instance::base(fr);
    // A control on the fixture itself: `take` from the tests above HAS a default,
    // so its window is strictly wider at the same arity. Without this the
    // assertion below is satisfied by a `required_arg_count` that returns its
    // argument for every input.
    for arity in [1usize, 3, 5] {
        assert_eq!(
            skull_codegen_js::emit::required_arg_count(&ws, &scx, body.clone(), arity),
            arity,
            "a callee with no defaulted params must have a single-valued arity window"
        );
    }
    let with_default = ws_with(OMITTED_DEFAULT_AT_SPEC);
    let dmid = *with_default.module_ids.get(&ustr::Ustr::from("<test>")).expect("module");
    let dfid = with_default
        .module(dmid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "take")
        .map(|(fid, _)| fid)
        .expect("take");
    let dscx = ScalarCtx::new(&with_default).with_target(bone_semantic::hir::FnBackend::Js);
    assert!(
        skull_codegen_js::emit::required_arg_count(
            &with_default,
            &dscx,
            skull_mir::instance::Instance::base(FuncRef { module: dmid, func: dfid }),
            3,
        ) < 3,
        "a callee WITH a default must have a wider window, or the point above is \
         not a property of defaults at all"
    );
}
