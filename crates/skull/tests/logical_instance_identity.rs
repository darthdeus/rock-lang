//! Controls for the identity a call records about its own instantiation
//! (plan S 9 / Stage 3, phase A3).
//!
//! `MirFunc::callee_instances` is the call's CLAIM about which concrete
//! instance it targets, and `skull::site_join` compares it against the graph's
//! own substitution. Both derive it from ONE oracle,
//! `skull_mir::monoty::convert`. A second, narrower converter is the failure
//! this guards: one that declines a function type gives `unwrap<fn()>` a graph
//! instance and no recorded claim, which the census reports as
//! `callee-no-instance`.
//!
//! Every fixture below pairs a shape a narrower converter would DECLINE with one
//! it would accept, and asserts the two stay DISTINCT identities -- a converter
//! that answered `any` for everything would satisfy "it converted" and lose the
//! property the identity exists for.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, InstanceGraph};
use skull::site_join::{self, SiteJoinCensus, Unjoined};
use skull_mir::lower::ScalarCtx;
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

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn graph_of(ws: &Workspace) -> InstanceGraph {
    let roots = instance_graph::emission_roots(ws, None);
    instance_graph::build(ws, roots, false)
}

fn census_of(src: &str) -> SiteJoinCensus {
    let ws = check(src);
    let mut g = graph_of(&ws);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let scx = ScalarCtx::new(&ws);
    site_join::census(&ws, &scx, &g, &bodies)
}

/// The rendered instantiations of every instance of `<test>`'s fn `name`.
fn instantiations_of(ws: &Workspace, g: &InstanceGraph, name: &str) -> Vec<String> {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let mut out: Vec<String> = g
        .instances
        .iter()
        .filter(|i| {
            i.func.module == mid && ws.module(mid).functions[i.func.func].name.as_str() == name
        })
        .map(|i| {
            i.args.iter().map(|a| g.interner().get(*a).render()).collect::<Vec<_>>().join(", ")
        })
        .collect();
    out.sort();
    out
}

fn unjoined(c: &SiteJoinCensus, cause: Unjoined) -> usize {
    c.unjoined.get(&cause).copied().unwrap_or(0)
}

/// Every `callee-no-instance == 0` below is measured over the bodies that were
/// actually LOWERED. A rejection in the identity oracle skips the body as
/// `lower-failed`, which empties the population and leaves the zero true and
/// meaningless -- measured: narrowing the oracle killed only the fixture that
/// asserts on it directly until this guard existed.
fn assert_population(c: &SiteJoinCensus) {
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    let failed = c
        .bodies_skipped
        .get(&skull::site_join::BodySkip::LowerFailed)
        .copied()
        .unwrap_or(0);
    assert_eq!(failed, 0, "a body did not lower, so the counts below are over a hole: {c:?}");
    assert!(c.joined > 0, "the fixture joined nothing: {c:?}");
}

/// A generic instantiated at a FUNCTION type records its instance, and the four
/// neighbouring shapes stay distinct from it and from each other.
///
/// `fn() -> unit` is the shape a narrower converter declines. The other four are
/// the admission control: a converter that lost structure would collapse
/// `(int, int)` and `[int]` and `int?` onto one identity while still reporting
/// "converted".
#[test]
fn a_function_typed_instantiation_records_a_distinct_instance() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }
fn nudge(a: int) -> int { return a + 1 }

fn main() {
    let g: fn(int) -> int = nudge
    let f = ident(g)
    let t = ident((1, 2))
    let a = ident([1, 2])
    let n: int? = 3
    let o = ident(n)
    let m = ident(1)
    print("{t} {a} {o} {m} {f(1)}")
}
"#;
    let ws = check(src);
    let g = graph_of(&ws);
    let insts = instantiations_of(&ws, &g, "ident");
    assert_eq!(
        insts.len(),
        5,
        "expected five distinct `ident` instances, got {insts:?}"
    );
    assert!(
        insts.iter().any(|s| s.contains("fn(")),
        "no function-typed instance was recorded: {insts:?}"
    );
    let unique: std::collections::BTreeSet<&String> = insts.iter().collect();
    assert_eq!(unique.len(), insts.len(), "two shapes share one identity: {insts:?}");

    let c = census_of(src);
    assert_population(&c);
    assert_eq!(
        unjoined(&c, Unjoined::CalleeNoInstance),
        0,
        "a call recorded no instance for its own instantiation: {c:?}"
    );
    assert_eq!(unjoined(&c, Unjoined::CalleeInstanceMismatch), 0, "{c:?}");
}

/// A generic instantiated at a COROUTINE type, and at a container of one,
/// records its instance.
///
/// bitgun's remaining three cases were `tick`/`start` over a coroutine payload
/// and `clear` over a `Map` of them -- the coroutine's yield type is a function
/// type one level down, which is why the container arm is not redundant with
/// the bare one.
#[test]
fn a_coroutine_typed_instantiation_records_a_distinct_instance() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }

co fn counter() -> int {
    yield 1
    return 2
}

fn main() {
    let c = counter()
    let a = ident(c)
    let m: Map<string, Coroutine<int, int>> = {}
    let b = ident(m)
    print("{b}")
    let _ = a
}
"#;
    let ws = check(src);
    let g = graph_of(&ws);
    let insts = instantiations_of(&ws, &g, "ident");
    assert_eq!(insts.len(), 2, "expected two `ident` instances, got {insts:?}");
    assert_ne!(insts[0], insts[1], "the bare and contained coroutine share one identity");

    let c = census_of(src);
    assert_population(&c);
    assert_eq!(unjoined(&c, Unjoined::CalleeNoInstance), 0, "{c:?}");
}

/// A NESTED application keeps both levels: `Vec<Vec<int>>` and `Vec<int>` are
/// two identities, not one erased `Vec<..>`.
///
/// The Stage-1 census recorded that 29.6% of substituted reads carry a tuple, a
/// nullable or an application WITH ARGUMENTS whose children the lossy adapter
/// erases. This is the arm that would go red if the identity path ever went
/// back through it.
#[test]
fn a_nested_application_keeps_both_levels_of_its_identity() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }

fn main() {
    let inner: [int] = [1]
    let outer: [[int]] = [[1]]
    let a = ident(inner)
    let b = ident(outer)
    print("{a} {b}")
}
"#;
    let ws = check(src);
    let g = graph_of(&ws);
    let insts = instantiations_of(&ws, &g, "ident");
    assert_eq!(insts.len(), 2, "expected two `ident` instances, got {insts:?}");
    assert_ne!(
        insts[0], insts[1],
        "`[int]` and `[[int]]` collapsed onto one identity: {insts:?}"
    );

    let c = census_of(src);
    assert_population(&c);
    assert_eq!(unjoined(&c, Unjoined::CalleeNoInstance), 0, "{c:?}");
}

/// A binding whose type does not convert at all is a spanned REJECTION, not a
/// silent absent claim.
///
/// Asserted directly on the oracle, because the corpus has no program that
/// reaches a recovery type here -- `semantic_pass` rejects first -- and a
/// control that never fires proves nothing about the arm. `UnboundGeneric` is
/// the one reason that stays a legitimate `None`: a shared generic body decides
/// no instantiation, and that IS what `callee-no-instance` means.
#[test]
fn a_shared_generic_body_decides_no_instantiation_and_says_so() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }
fn pass<U>(y: U) -> U { return ident(y) }
fn main() { print("{pass(1)}") }
"#;
    let ws = check(src);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (pass_fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "pass")
        .expect("pass");
    let (ident_fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "ident")
        .expect("ident");
    let inf = ws
        .inference(mid)
        .and_then(|ir| ir.per_fn.get(&bone_semantic::hir::BodyOwner::Function(pass_fid)))
        .expect("pass has inference");
    let call = *inf
        .resolved_overload_calls
        .iter()
        .find(|(_, (m, f))| *m == mid && *f == ident_fid)
        .map(|(e, _)| e)
        .expect("pass calls ident");
    let empty = skull_mir::lower::SpecSubst::default();
    let got = skull_mir::lower::callee_instance_subst(
        &ws,
        inf,
        call,
        skull_mir::mir::FuncRef { module: mid, func: ident_fid },
        &empty,
    )
    .expect("an unbound declared generic is not a rejection");
    assert!(
        got.is_none(),
        "a shared generic body claimed an instantiation it cannot know: {got:?}"
    );

    // CONTROL: under a substitution that binds `U`, the SAME call resolves.
    let u_var = skull_mir::monoty::generic_var_of(
        &ws,
        inf,
        ws.module(mid).functions[pass_fid].generics[0],
    )
    .expect("`pass`'s declared generic is a type variable");
    let mut bound = skull_mir::lower::SpecSubst::default();
    bound.insert(u_var, skull_mir::monoty::MonoTy::Int);
    let got = skull_mir::lower::callee_instance_subst(
        &ws,
        inf,
        call,
        skull_mir::mir::FuncRef { module: mid, func: ident_fid },
        &bound,
    )
    .expect("a bound generic converts")
    .expect("a bound generic names an instance");
    assert_eq!(
        got.values().map(|t| t.render()).collect::<Vec<_>>(),
        vec!["int".to_string()],
        "the substitution did not reach the callee's instantiation"
    );
}

// ---------------------------------------------------------------------------
// Declaration selection (phase A4)
// ---------------------------------------------------------------------------

/// A game's `inline(lua)` override of an engine's portable `@nomangle` body is
/// ONE declaration in the language's own grouping, and the join says so.
///
/// The graph is TARGET-NEUTRAL and records bone's collapsed group
/// REPRESENTATIVE -- the portable `Any` body -- while MIR calls the sibling that
/// serves the backend being lowered. Raw `FuncRef` equality reported that
/// correct selection as `callee-decl-mismatch`; `games/sample-game`'s
/// `file_append` was the corpus's single case, one call of 171,738.
///
/// The comparison asks the EMITTER's own rule (`backend_variant`), so the
/// control below is not redundant: a pair the language does NOT group must
/// still mismatch.
#[test]
fn a_backend_sibling_of_the_representative_is_not_a_declaration_mismatch() {
    let c = census_of(
        r#"
@nomangle fn portable(a: string) -> bool { return a == "" }
inline(lua) fn portable(a: string) -> bool = """
    return a == ""
"""

fn main() { print("{portable("x")}") }
"#,
    );
    assert_population(&c);
    assert_eq!(
        unjoined(&c, Unjoined::CalleeMismatch),
        0,
        "a cross-backend sibling of the representative was called a mismatch: {c:?}"
    );
}

/// TWO declarations sharing ONE NAME and ONE RENDERED SIGNATURE that the
/// language does NOT group stay distinct AT THE CONSUMER.
///
/// This drives `check_callee` itself, not just the grouping oracle, and the
/// pair is deliberately name-and-signature IDENTICAL: a join that became
/// permissive by name, or by rendered signature, would accept the wrong callee
/// while a test of `backend_variant_group` alone stayed green. Two
/// `inline(lua)` declarations of one signature are a REPEATED backend, which
/// bone refuses to collapse, so neither is a sibling of the other.
///
/// Synthetic because bone rejects the ambiguous CALL before a census could
/// reach it -- the instances are built directly and fed to the comparison.
#[test]
fn a_declaration_the_language_does_not_group_is_refused_by_the_join() {
    let ws = check(
        r#"
inline(lua) fn twice(a: string) -> bool = """
    return a == ""
"""
inline(lua) fn twice(a: string) -> bool = """
    return a ~= ""
"""

fn main() { print("x") }
"#,
    );
    let g = graph_of(&ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let decls: Vec<skull_mir::mir::FuncRef> = ws
        .module(mid)
        .functions
        .iter()
        .filter(|(_, f)| f.name.as_str() == "twice")
        .map(|(fid, _)| skull_mir::mir::FuncRef { module: mid, func: fid })
        .collect();
    assert_eq!(
        decls.len(),
        2,
        "the fixture needs two same-name same-signature declarations: {decls:?}"
    );
    // Neither groups with the other, which is what makes them two callees.
    for r in &decls {
        assert_eq!(skull_mir::lower::backend_variant_group(&ws, *r), vec![*r]);
    }
    let inst = |r: skull_mir::mir::FuncRef| skull::instance_graph::Instance {
        func: r,
        args: Vec::new(),
    };
    let lua = bone_semantic::hir::FnBackend::Lua;

    // CONTROL: an exact match joins, or the refusal below proves nothing.
    assert_eq!(
        skull::site_join::check_callee(
            &ws,
            &g,
            &skull::concrete_body::Callee::Rock(inst(decls[0])),
            decls[0],
            None,
            lua,
        )
        .outcome(),
        Ok(skull::site_join::Verified::NonGeneric),
    );

    // The defect: the FIRST declaration's instance offered for a call that
    // targets the SECOND. Same name, same rendered signature, different body.
    assert_eq!(
        skull::site_join::check_callee(
            &ws,
            &g,
            &skull::concrete_body::Callee::Rock(inst(decls[0])),
            decls[1],
            None,
            lua,
        )
        .outcome(),
        Err(Unjoined::CalleeMismatch),
        "a declaration the language does not group was accepted as the callee"
    );
}

/// The repeated-backend population this repo actually has, asserted on the
/// grouping oracle the consumer above delegates to.
///
/// `__op_eq` has six declarations, five of them `Lua`. bone refuses a repeated
/// backend, so none of them groups with another -- which is what makes the
/// consumer's refusal reachable for a real overload set rather than only for
/// the synthetic pair.
#[test]
fn a_repeated_backend_pair_is_not_one_declaration() {
    let ws = check("fn main() { print(\"x\") }");
    let all = ws.functions_by_name.get(&Ustr::from("__op_eq")).expect("declared");
    assert!(all.len() > 2, "the repeated-backend population is the point: {}", all.len());
    let ops = skull_mir::lower::dispatched_callee_decls(&ws, "__op_eq");
    assert_eq!(ops.len(), 1, "the callable `__op_eq` is unique: {ops:?}");
    for &(m, f) in all.iter() {
        let r = skull_mir::mir::FuncRef { module: m, func: f };
        let group = skull_mir::lower::backend_variant_group(&ws, r);
        assert_eq!(
            group,
            vec![r],
            "a repeated-backend declaration was grouped with a sibling: {group:?}"
        );
    }
}
