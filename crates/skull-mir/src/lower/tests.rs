use super::*;

// -- Stage 0b route-table invariants (docs/monomorphization-plan.md S 2b).
//
// No Rock program can reach these states, which is why they are rejections
// rather than diagnostics: each one is constructed by handing the oracle a
// compiler state bone never produces.

fn spec_ws(src: &str) -> (Workspace, ModuleId) {
    let mut modules =
        vec![skull_parser::parse_module(src, ustr::Ustr::from("<test>")).expect("parse")];
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("user module id");
    (ws, mid)
}
fn spec_fn(ws: &Workspace, mid: ModuleId, name: &str) -> FuncRef {
    let (func, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("function by name");
    FuncRef { module: mid, func }
}

fn spec_inf<'a>(ws: &'a Workspace, mid: ModuleId, name: &str) -> &'a BodyInference {
    let fref = spec_fn(ws, mid, name);
    ws.inference(mid)
        .expect("inference")
        .per_fn
        .get(&BodyOwner::Function(fref.func))
        .expect("body inference")
}

/// A resolved generic direct call with no `call_generic_bindings` entry.
/// Inference records one for every such call, so an absent record means the
/// resolution that produced the `FuncRef` and the inference that recorded the
/// bindings disagree. The state is built by asking for the spec of a generic
/// callee at an expr that is not that call.
#[test]
fn missing_call_generic_bindings_rejects() {
    let (ws, mid) = spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y = 1 }");
    let inf = spec_inf(&ws, mid, "main");
    let body = ws.module(mid).bodies.get(&BodyOwner::Function(spec_fn(&ws, mid, "main").func))
        .expect("main body");
    let e = body.expr_arena.iter().next().expect("an expr in main").0;
    assert!(!inf.call_generic_bindings.contains_key(&e), "the probe expr must have no record");
    let err = compute_call_spec(
        &ws,
        inf,
        e,
        spec_fn(&ws, mid, "id"),
        &SpecSubst::default(),
    )
    .expect_err("a generic callee with no recorded bindings must reject");
    assert!(err.message.contains("no recorded generic bindings"), "{}", err.message);
}

/// A binding list whose length differs from the callee's generic count. The
/// silent `None` left the instantiation unspecialized; the two lists are
/// built per callee, so a mismatch is compiler state, never a program.
#[test]
fn spec_binding_count_mismatch_rejects() {
    let (ws, mid) = spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y = 1 }");
    let err = crate::lower::shape::compute_spec_from_bindings(
        &ws,
        spec_inf(&ws, mid, "main"),
        spec_fn(&ws, mid, "id"),
        &[],
        &SpecSubst::default(),
    )
    .expect_err("0 bindings against a 1-generic callee must reject");
    assert!(err.message.contains("declares 1 generic"), "{}", err.message);
}

/// A declared entry of `Function::generics` that is not `TypeExpr::Generic`.
/// Constructed by repointing the declaration at a builtin node: the silent
/// route pushed `None` into the key and left the generic out of the
/// `SpecSubst` while the key still claimed the instantiation.
#[test]
fn non_generic_declared_generic_rejects() {
    let (mut ws, mid) =
        spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y: int = 1 }");
    let int_ty = ws.builtins.int();
    let fref = spec_fn(&ws, mid, "id");
    ws.modules[mid.index()].functions[fref.func].generics[0] = int_ty;
    let err = crate::lower::shape::compute_spec_from_bindings(
        &ws,
        spec_inf(&ws, mid, "main"),
        fref,
        &[int_ty],
        &SpecSubst::default(),
    )
    .expect_err("a non-generic declared generic must reject");
    assert!(err.message.contains("is not a generic type variable"), "{}", err.message);
}

// --- Stage 0g: positive fixtures for the ablation's `compiler-invariant`
// detectors.
//
// Both surfaces measured ZERO on Lua and JS (`docs/mono-stage0e-ablation.md`).
// A zero from a detector that has never fired is not evidence, and these two
// arms cannot be reached from SOURCE -- they require bone and skull to
// disagree, which a valid program cannot express. They are reachable by
// constructing the violating state directly, exactly as the Stage-0b tests
// above do, which is what turns the corpus zero into a measurement.

/// A binding list whose length differs from the callee's declared generic
/// count classifies as `compiler-invariant`, never as a missing edge: the
/// two lists are built per callee, so a mismatch is compiler state rather
/// than a program the graph is missing a record for.
#[test]
fn ablation_classifies_an_arity_mismatch_as_a_compiler_invariant() {
    let (ws, mid) = spec_ws(
        "fn id<T>(x: T) -> T { x }\nfn main() { let y = id(1) }",
    );
    let inf = spec_inf(&ws, mid, "main");
    let body = ws
        .module(mid)
        .bodies
        .get(&BodyOwner::Function(spec_fn(&ws, mid, "main").func))
        .expect("main body");
    // The real call expr, so the bindings record EXISTS -- the defect under
    // test is its arity, not its absence.
    let (e, _) = body
        .expr_arena
        .iter()
        .find(|(e, _)| inf.call_generic_bindings.contains_key(e))
        .expect("a call with recorded bindings");
    let mut inf2 = inf.clone();
    inf2.call_generic_bindings.insert(e, Vec::new());
    let (category, detail) = crate::lower::shape::ablation_decline(
        &ws,
        &inf2,
        e,
        spec_fn(&ws, mid, "id"),
        &SpecSubst::default(),
    )
    .expect("a 0-binding list against a 1-generic callee must be classified");
    assert_eq!(category, crate::ablation::FatalCategory::CompilerInvariant, "{detail}");
    assert!(detail.contains("declares 1 generic"), "{detail}");
}

/// A declared entry of `Function::generics` that is not a type variable is
/// the other `compiler-invariant` arm. Same construction as
/// `non_generic_declared_generic_rejects`: repoint the declaration at a
/// builtin node.
#[test]
fn ablation_classifies_a_non_type_variable_generic_as_a_compiler_invariant() {
    let (mut ws, mid) = spec_ws(
        "fn id<T>(x: T) -> T { x }\nfn main() { let y = id(1) }",
    );
    let int_ty = ws.builtins.int();
    let fref = spec_fn(&ws, mid, "id");
    ws.modules[mid.index()].functions[fref.func].generics[0] = int_ty;
    let inf = spec_inf(&ws, mid, "main");
    let body = ws
        .module(mid)
        .bodies
        .get(&BodyOwner::Function(spec_fn(&ws, mid, "main").func))
        .expect("main body");
    let (e, _) = body
        .expr_arena
        .iter()
        .find(|(e, _)| inf.call_generic_bindings.contains_key(e))
        .expect("a call with recorded bindings");
    let (category, detail) =
        crate::lower::shape::ablation_decline(&ws, inf, e, fref, &SpecSubst::default())
            .expect("a non-type-variable declared generic must be classified");
    assert_eq!(category, crate::ablation::FatalCategory::CompilerInvariant, "{detail}");
    assert!(detail.contains("not a generic type variable"), "{detail}");
}

/// The BOUNDING control for both: a well-formed generic call classifies as
/// nothing at all. Without it the two above could be passing because
/// `ablation_decline` returns `CompilerInvariant` for everything.
#[test]
fn ablation_classifies_a_well_formed_generic_call_as_nothing() {
    let (ws, mid) = spec_ws("fn id<T>(x: T) -> T { x }\nfn main() { let y = id(1) }");
    let inf = spec_inf(&ws, mid, "main");
    let body = ws
        .module(mid)
        .bodies
        .get(&BodyOwner::Function(spec_fn(&ws, mid, "main").func))
        .expect("main body");
    let (e, _) = body
        .expr_arena
        .iter()
        .find(|(e, _)| inf.call_generic_bindings.contains_key(e))
        .expect("a call with recorded bindings");
    assert!(
        crate::lower::shape::ablation_decline(
            &ws,
            inf,
            e,
            spec_fn(&ws, mid, "id"),
            &SpecSubst::default()
        )
        .is_none(),
        "a well-formed generic call must not be classified as a fatal"
    );
}
