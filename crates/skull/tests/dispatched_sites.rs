//! Controls for the CONDITIONALLY-synthesized call family
//! (`skull_mir::lower::dispatch`, plan S 9 / Stage 3, phase A1).
//!
//! An interpolation hole and `==`/`!=` synthesize a call on SOME of their arms
//! and none on the others. The arm is a type decision and the source expression
//! names no callee, so `COMPILER_GENERATED_TRIGGERS`' name key cannot reach it.
//!
//! Every test here asserts BOTH sides of one conditional: the arm that emits
//! and joins, and the nearest arm that must emit no obligation at all. A test
//! that only checked the emitting arm would pass against a walk that recorded
//! an obligation for every hole.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, InstanceGraph};
use skull::site_join::{self, SiteJoinCensus, Unjoined};
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::{Inst, MirFunc};
use skull_mir::site::SiteOp;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
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

fn check_with_dependency(src: &str, dependency: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(
        skull_parser::parse_module_clean(dependency, Ustr::from("games/impostor.rock"))
            .expect("dependency parse"),
    );
    modules.push(skull_parser::parse_module_clean(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn graph_of(ws: &Workspace) -> InstanceGraph {
    let roots = skull::mir_emit::emission_roots(ws, None);
    instance_graph::build(ws, roots, false)
}

fn census_of(src: &str) -> SiteJoinCensus {
    let ws = check(src);
    let mut g = graph_of(&ws);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let scx = ScalarCtx::new(&ws).sharing(g.interner().clone());
    site_join::census(&ws, &scx, &g, &bodies)
}

/// Lower `<test>`'s fn `name` through emission's own transform sequence.
fn prepared_body(ws: &Workspace, name: &str) -> MirFunc {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (fid, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("fn not found");
    let fref = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let graph = graph_of(ws);
    let bodies = skull::concrete_body::build(ws, &graph);
    let scx = ScalarCtx::new(ws).sharing(graph.interner().clone());
    let instance = skull_mir::instance::Instance::base(fref);
    let plan = bodies.plans.get(&instance).expect("concrete plan");
    let mut mf =
        skull_mir::lower::lower_instance_with_plans(ws, &scx, plan, &bodies.plans).expect("lower");
    let mangled = skull_mir::mangle::mangle_fn(ws, f);
    let mut cache = skull_transform::InlineCache::default();
    skull::mir_emit::prepare_instance_body_with_plans(
        ws,
        &scx,
        fref,
        &mangled,
        &mut mf,
        &mut cache,
        &bodies.plans,
    )
    .expect("prepare");
    mf
}

fn lowered_body_in_mode(ws: &Workspace, name: &str, values_first_class: bool) -> MirFunc {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("fn not found");
    let fref = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let graph = graph_of(ws);
    let bodies = skull::concrete_body::build(ws, &graph);
    let scx = if values_first_class {
        ScalarCtx::new_values_first_class(ws)
    } else {
        ScalarCtx::new(ws)
    }
    .sharing(graph.interner().clone());
    let instance = skull_mir::instance::Instance::base(fref);
    let plan = bodies.plans.get(&instance).expect("concrete plan");
    skull_mir::lower::lower_instance_with_plans(ws, &scx, plan, &bodies.plans).expect("lower")
}

fn lowered_body(ws: &Workspace, name: &str) -> MirFunc {
    lowered_body_in_mode(ws, name, false)
}

fn lowered_values_first_class_body(ws: &Workspace, name: &str) -> MirFunc {
    lowered_body_in_mode(ws, name, true)
}

/// The names lowering stamped as `SiteOp::Dispatched` in `name`'s body.
fn dispatched_in(ws: &Workspace, name: &str) -> Vec<&'static str> {
    fn walk(mf: &MirFunc, out: &mut Vec<&'static str>) {
        for c in mf.call_sites.values() {
            if let SiteOp::Dispatched { name, .. } = c.site.op {
                out.push(name);
            }
        }
        for nf in &mf.nested_fns {
            walk(nf, out);
        }
    }
    let mut out = Vec::new();
    walk(&prepared_body(ws, name), &mut out);
    out.sort_unstable();
    out
}

fn dispatched_instance_names_in(ws: &Workspace, name: &str) -> Vec<(String, String)> {
    let mf = prepared_body(ws, name);
    let mut out = Vec::new();
    for (&id, site) in &mf.call_sites {
        let SiteOp::Dispatched { name, .. } = site.site.op else {
            continue;
        };
        let callee = mf
            .callee_instances
            .get(&id)
            .expect("a dispatched direct call must record its concrete callee");
        let function = &ws.module(callee.func.module).functions[callee.func.func];
        out.push((name.to_string(), function.name.to_string()));
    }
    out.sort_unstable();
    out
}

/// The obligations the WALK records for `name`'s (non-generic, empty-subst)
/// instance -- the other half of the cross-view check.
fn walk_dispatched_in(ws: &Workspace, name: &str) -> Vec<&'static str> {
    let g = graph_of(ws);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (fid, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("fn not found");
    let fref = skull_mir::mir::FuncRef {
        module: mid,
        func: fid,
    };
    let mut out: Vec<&'static str> = Vec::new();
    for (inst, sites) in g.call_sites() {
        if inst.func != fref {
            continue;
        }
        for site in sites.keys() {
            if let SiteOp::Dispatched { name, .. } = site.op {
                out.push(name);
            }
        }
    }
    out.sort_unstable();
    out
}

fn unjoined(c: &SiteJoinCensus, cause: Unjoined) -> usize {
    c.unjoined.get(&cause).copied().unwrap_or(0)
}

/// The `dispatched` operation must have a POPULATION in the census, or every
/// zero below is measured over nothing.
fn assert_dispatched_population(c: &SiteJoinCensus, at_least: usize) {
    let examined = c.examined_by_op.get("dispatched").copied().unwrap_or(0);
    assert!(
        examined >= at_least,
        "expected at least {at_least} dispatched obligations examined, got {examined}: {c:?}"
    );
    let joined = c.joined_by_op.get("dispatched").copied().unwrap_or(0);
    assert_eq!(
        joined, examined,
        "a dispatched obligation did not join: {c:?}"
    );
    assert_eq!(
        unjoined(c, Unjoined::SiteNotInBody),
        0,
        "a synthesized call reached MIR with no obligation recorded for it: {c:?}"
    );
}

// ---------------------------------------------------------------------------
// Interpolation: the float arm against its nearest non-float control
// ---------------------------------------------------------------------------

/// A FLOAT hole synthesizes `__rock_format_float` and it joins; an INT hole in
/// the same program synthesizes nothing.
///
/// The int arm is the load-bearing half. Recording an obligation for every hole
/// would satisfy the float assertion and break nothing else, and the census
/// would still balance -- what it would break is the emitted body's agreement
/// with the walk, which is what `dispatched_in` vs `walk_dispatched_in` reads.
#[test]
fn a_float_hole_joins_its_formatter_and_an_int_hole_has_no_obligation() {
    let ws = check(
        r#"
fn f_float(x: float) -> string { return "v={x}" }
fn f_int(x: int) -> string { return "v={x}" }
fn main() { print(f_float(1.5)) print(f_int(2)) }
"#,
    );
    assert_eq!(dispatched_in(&ws, "f_float"), vec!["__rock_format_float"]);
    assert_eq!(
        walk_dispatched_in(&ws, "f_float"),
        vec!["__rock_format_float"]
    );

    let empty: Vec<&'static str> = Vec::new();
    assert_eq!(
        dispatched_in(&ws, "f_int"),
        empty,
        "an int hole emits no call"
    );
    assert_eq!(
        walk_dispatched_in(&ws, "f_int"),
        empty,
        "an int hole must not be given an obligation the body does not carry"
    );
}

/// TWO float holes in ONE literal are TWO obligations.
///
/// They share the enclosing `Expr::InterpolatedString`, so keying the site on
/// that expression collapses them onto one key -- the census would report one
/// obligation against two call instructions and the second would be
/// `site-not-in-body`. The hole's own expression is the key.
#[test]
fn two_float_holes_in_one_literal_are_two_obligations() {
    let ws = check(
        r#"
fn two(a: float, b: float) -> string { return "{a} and {b}" }
fn main() { print(two(1.0, 2.0)) }
"#,
    );
    assert_eq!(
        dispatched_in(&ws, "two"),
        vec!["__rock_format_float", "__rock_format_float"]
    );
    assert_eq!(
        walk_dispatched_in(&ws, "two"),
        vec!["__rock_format_float", "__rock_format_float"]
    );

    let c = census_of(
        r#"
fn two(a: float, b: float) -> string { return "{a} and {b}" }
fn main() { print(two(1.0, 2.0)) }
"#,
    );
    assert_dispatched_population(&c, 2);
}

/// A nobox `value` hole renders ONE call PER CALLING LEAF FIELD, all at one
/// expression: `__rock_format_float` for each float, `__rock_quote_string` for
/// the string. Both ABI modes render it through the same structural format
/// tree, so they share the obligations. The field-index PATH keeps the calls
/// distinct; without it the walk records one obligation and the census
/// reports the rest as `site-not-in-body`.
///
/// The int field is the admission control: it takes a non-calling route, so
/// the count is three rather than "one per field".
#[test]
fn a_nobox_value_hole_carries_one_obligation_per_float_leaf() {
    let src = r#"
nobox value V { a: float, b: float, c: int, s: string }
fn show(v: V) -> string { return "{v}" }
fn main() { print(show(V { a: 1.0, b: 2.0, c: 3, s: "x" })) }
"#;
    let ws = check(src);
    assert_eq!(
        dispatched_in(&ws, "show"),
        vec![
            "__rock_format_float",
            "__rock_format_float",
            "__rock_quote_string"
        ],
        "two float leaves and a string leaf; the int leaf synthesizes nothing"
    );
    assert_eq!(
        walk_dispatched_in(&ws, "show"),
        vec![
            "__rock_format_float",
            "__rock_format_float",
            "__rock_quote_string"
        ]
    );
    assert_dispatched_population(&census_of(src), 3);
}

/// A COMPOSITE hole synthesizes `inspect`; a transparent newtype in the same
/// program synthesizes nothing (it renders `Name(<tostring inner>)`).
///
/// `inspect` has no population anywhere in the 28-project corpus -- every real
/// interpolation of a struct goes through the nobox path -- so this fixture is
/// the only evidence the route is covered at all.
#[test]
fn a_composite_hole_joins_inspect_and_a_newtype_hole_has_no_obligation() {
    let src = r#"
struct P { x: int, y: int }
newtype Id(int)
fn show_p(p: P) -> string { return "{p}" }
fn show_id(i: Id) -> string { return "{i}" }
fn main() { print(show_p(P { x: 1, y: 2 })) print(show_id(Id(3))) }
"#;
    let ws = check(src);
    assert_eq!(dispatched_in(&ws, "show_p"), vec!["inspect"]);
    assert_eq!(walk_dispatched_in(&ws, "show_p"), vec!["inspect"]);

    let empty: Vec<&'static str> = Vec::new();
    assert_eq!(dispatched_in(&ws, "show_id"), empty);
    assert_eq!(walk_dispatched_in(&ws, "show_id"), empty);
    assert_dispatched_population(&census_of(src), 1);
}

/// A format SPEC's helpers are planned by the same oracle. A composite with a
/// precision routes through `inspect_formatted` and then pads; a plain `.2f` on
/// a float is a single `string.format` and calls nothing.
///
/// Also with no corpus population, and the reason the plan carries the printf
/// flags rather than only the callee list: lowering reads `skip_width` and
/// `needs_pad` off the same object that decides whether the pad helper is
/// called.
#[test]
fn a_spec_hole_joins_its_helpers_and_a_printf_spec_has_no_obligation() {
    let src = r#"
struct P { x: int, y: int }
fn show_p(p: P) -> string { return "{p:.3}" }
fn show_f(x: float) -> string { return "{x:.2f}" }
fn main() { print(show_p(P { x: 1, y: 2 })) print(show_f(1.5)) }
"#;
    let ws = check(src);
    assert_eq!(
        dispatched_in(&ws, "show_p"),
        vec!["__rock_fmt_pad", "inspect_formatted"]
    );
    assert_eq!(
        walk_dispatched_in(&ws, "show_p"),
        vec!["__rock_fmt_pad", "inspect_formatted"]
    );
    assert_eq!(
        dispatched_instance_names_in(&ws, "show_p"),
        vec![
            ("__rock_fmt_pad".to_string(), "__rock_fmt_pad".to_string()),
            (
                "inspect_formatted".to_string(),
                "inspect_formatted".to_string()
            ),
        ]
    );

    let empty: Vec<&'static str> = Vec::new();
    assert_eq!(
        dispatched_in(&ws, "show_f"),
        empty,
        "a printf conversion calls nothing"
    );
    assert_eq!(walk_dispatched_in(&ws, "show_f"), empty);
    assert_dispatched_population(&census_of(src), 2);
}

#[test]
fn an_ambiguous_dispatched_helper_rejects_the_call() {
    let ws = check_with_dependency(
        r#"
struct P { x: int }
fn main() { print("{P { x: 1 }:.2}") }
"#,
        r#"@nomangle inline(lua) fn inspect_formatted(obj: any, precision: float) -> string = "return 'impostor'""#,
    );
    assert!(
        skull_mir::lower::dispatched_callee_decl(&ws, "inspect_formatted").is_none(),
        "two distinct bare declarations must be ambiguous"
    );
    let graph = graph_of(&ws);
    assert!(
        graph
            .unresolved()
            .any(|unresolved| { unresolved.reason == "compiler-generated-entry-unresolved" })
    );
}

#[test]
fn a_dispatched_render_does_not_borrow_the_holes_bound_tree() {
    let ws = check(
        r#"
value V2 { x: float, y: float }

fn show(a: [V2]) -> string { return "{a.map(_.x).sum_f()}" }
fn main() { print(show([V2 { x: 1.0, y: 2.0 }])) }
"#,
    );
    assert_eq!(
        dispatched_instance_names_in(&ws, "show"),
        vec![(
            "__rock_format_float".to_string(),
            "__rock_format_float".to_string()
        )]
    );
    let mf = lowered_body(&ws, "show");
    assert!(
        mf.call_sites.iter().any(|(id, site)| {
            matches!(site.site.op, SiteOp::DirectCall) && mf.bound_trees.contains_key(id)
        }),
        "the hole's bounded user call must retain its own tree"
    );
    assert!(mf.call_sites.iter().all(|(id, site)| {
        !matches!(site.site.op, SiteOp::Dispatched { .. }) || !mf.bound_trees.contains_key(id)
    }));
}

/// The `%` and `b` conversions and the GROUPING separator each call their own
/// stdlib helper, and each is paired with the nearest spec that calls nothing.
///
/// None of the three has any population in the 28-project corpus, so this is
/// the only evidence they are covered at all. They are separate arms of
/// `FormatPlan`, not one repeated: `special` replaces the printf conversion
/// while `grouping` wraps whatever the conversion produced, and `needs_pad`
/// then depends on both.
#[test]
fn the_percent_binary_and_grouping_helpers_join_and_their_controls_do_not() {
    let src = r#"
fn pct(x: float) -> string { return "{x:%}" }
fn pct_plain(x: float) -> string { return "{x:f}" }
fn bin(x: int) -> string { return "{x:b}" }
fn bin_plain(x: int) -> string { return "{x:x}" }
fn grouped(x: int) -> string { return "{x:,}" }
fn grouped_plain(x: int) -> string { return "{x:d}" }
fn main() {
    print(pct(0.5)) print(pct_plain(0.5))
    print(bin(5)) print(bin_plain(5))
    print(grouped(1000)) print(grouped_plain(1000))
}
"#;
    let ws = check(src);
    let empty: Vec<&'static str> = Vec::new();

    assert_eq!(dispatched_in(&ws, "pct"), vec!["__rock_fmt_percent"]);
    assert_eq!(walk_dispatched_in(&ws, "pct"), vec!["__rock_fmt_percent"]);
    assert_eq!(
        dispatched_in(&ws, "pct_plain"),
        empty,
        "a plain `f` calls nothing"
    );
    assert_eq!(walk_dispatched_in(&ws, "pct_plain"), empty);

    assert_eq!(dispatched_in(&ws, "bin"), vec!["__rock_fmt_binary"]);
    assert_eq!(walk_dispatched_in(&ws, "bin"), vec!["__rock_fmt_binary"]);
    assert_eq!(
        dispatched_in(&ws, "bin_plain"),
        empty,
        "a plain `x` calls nothing"
    );
    assert_eq!(walk_dispatched_in(&ws, "bin_plain"), empty);

    assert_eq!(dispatched_in(&ws, "grouped"), vec!["__rock_fmt_grouping"]);
    assert_eq!(
        walk_dispatched_in(&ws, "grouped"),
        vec!["__rock_fmt_grouping"]
    );
    assert_eq!(
        dispatched_in(&ws, "grouped_plain"),
        empty,
        "a plain `d` calls nothing"
    );
    assert_eq!(walk_dispatched_in(&ws, "grouped_plain"), empty);

    assert_dispatched_population(&census_of(src), 3);
}

/// A SPECIAL conversion plus a WIDTH pads as well, and the same conversion
/// without a width does not.
///
/// `needs_pad` is `(grouping.is_some() || special_conv) && has_width`, so the
/// `special_conv` disjunct needs its own width case: with only the grouping one
/// below, deleting `|| special_conv` leaves every formatter test green.
#[test]
fn a_special_conversion_with_a_width_also_pads_and_without_one_does_not() {
    let ws = check(
        r#"
fn pct_wide(x: float) -> string { return "{x:10%}" }
fn pct_bare(x: float) -> string { return "{x:%}" }
fn bin_wide(x: int) -> string { return "{x:10b}" }
fn bin_bare(x: int) -> string { return "{x:b}" }
fn main() {
    print(pct_wide(0.5)) print(pct_bare(0.5))
    print(bin_wide(5)) print(bin_bare(5))
}
"#,
    );
    assert_eq!(
        dispatched_in(&ws, "pct_wide"),
        vec!["__rock_fmt_pad", "__rock_fmt_percent"]
    );
    assert_eq!(
        walk_dispatched_in(&ws, "pct_wide"),
        vec!["__rock_fmt_pad", "__rock_fmt_percent"]
    );
    assert_eq!(dispatched_in(&ws, "pct_bare"), vec!["__rock_fmt_percent"]);
    assert_eq!(
        walk_dispatched_in(&ws, "pct_bare"),
        vec!["__rock_fmt_percent"]
    );

    assert_eq!(
        dispatched_in(&ws, "bin_wide"),
        vec!["__rock_fmt_binary", "__rock_fmt_pad"]
    );
    assert_eq!(
        walk_dispatched_in(&ws, "bin_wide"),
        vec!["__rock_fmt_binary", "__rock_fmt_pad"]
    );
    assert_eq!(dispatched_in(&ws, "bin_bare"), vec!["__rock_fmt_binary"]);
    assert_eq!(
        walk_dispatched_in(&ws, "bin_bare"),
        vec!["__rock_fmt_binary"]
    );
}

/// GROUPING plus a WIDTH pads as well, and grouping alone does not.
///
/// `needs_pad` is a function of the grouping and the width together, so a
/// fixture with only one of them cannot distinguish "pad is called" from "pad
/// is never called".
#[test]
fn grouping_with_a_width_also_pads_and_grouping_alone_does_not() {
    let ws = check(
        r#"
fn wide(x: int) -> string { return "{x:10,}" }
fn narrow(x: int) -> string { return "{x:,}" }
fn main() { print(wide(1000)) print(narrow(1000)) }
"#,
    );
    assert_eq!(
        dispatched_in(&ws, "wide"),
        vec!["__rock_fmt_grouping", "__rock_fmt_pad"]
    );
    assert_eq!(
        walk_dispatched_in(&ws, "wide"),
        vec!["__rock_fmt_grouping", "__rock_fmt_pad"]
    );
    assert_eq!(dispatched_in(&ws, "narrow"), vec!["__rock_fmt_grouping"]);
    assert_eq!(
        walk_dispatched_in(&ws, "narrow"),
        vec!["__rock_fmt_grouping"]
    );
}

// ---------------------------------------------------------------------------
// Equality: the structural arm against its nearest primitive controls
// ---------------------------------------------------------------------------

/// Structural equality synthesizes `__op_eq` / `__op_ne` and both join. Scalar
/// equality and nobox-value equality, in the same program, synthesize nothing.
///
/// The two negative arms are different mechanisms, not one repeated: a scalar
/// pair lowers to `Inst::Eq`, a nobox pair to leaf-wise compares. Either one
/// answering "structural" would be a wrong callee at a real call, not a missing
/// one.
#[test]
fn structural_equality_joins_its_helper_and_scalar_and_nobox_do_not() {
    let src = r#"
struct P { x: int, y: int }
nobox value V { a: float, b: float }
fn eq_struct(a: P, b: P) -> bool { return a == b }
fn ne_struct(a: P, b: P) -> bool { return a != b }
fn eq_scalar(a: int, b: int) -> bool { return a == b }
fn eq_nobox(a: V, b: V) -> bool { return a == b }
fn main() {
    print("{eq_struct(P { x: 1, y: 2 }, P { x: 1, y: 2 })}")
    print("{ne_struct(P { x: 1, y: 2 }, P { x: 1, y: 2 })}")
    print("{eq_scalar(1, 2)}")
    print("{eq_nobox(V { a: 1.0, b: 2.0 }, V { a: 1.0, b: 2.0 })}")
}
"#;
    let ws = check(src);
    assert_eq!(dispatched_in(&ws, "eq_struct"), vec!["__op_eq"]);
    assert_eq!(walk_dispatched_in(&ws, "eq_struct"), vec!["__op_eq"]);
    assert_eq!(dispatched_in(&ws, "ne_struct"), vec!["__op_ne"]);
    assert_eq!(walk_dispatched_in(&ws, "ne_struct"), vec!["__op_ne"]);

    let empty: Vec<&'static str> = Vec::new();
    assert_eq!(dispatched_in(&ws, "eq_scalar"), empty);
    assert_eq!(walk_dispatched_in(&ws, "eq_scalar"), empty);
    assert_eq!(dispatched_in(&ws, "eq_nobox"), empty);
    assert_eq!(walk_dispatched_in(&ws, "eq_nobox"), empty);

    assert_dispatched_population(&census_of(src), 2);
}

#[test]
fn values_first_class_nobox_equality_stays_a_typed_operation() {
    let ws = check(
        r#"
nobox value Point { x: int, y: int }
fn same(a: Point, b: Point) -> bool { return a == b }
fn main() { print("{same(Point { x: 1, y: 2 }, Point { x: 1, y: 2 })}") }
"#,
    );
    assert_eq!(walk_dispatched_in(&ws, "same"), Vec::<&'static str>::new());
    let mf = lowered_values_first_class_body(&ws, "same");
    assert!(
        mf.insts
            .iter()
            .any(|(_, inst)| matches!(inst, Inst::Eq(_, _))),
        "values-first-class equality must remain a typed MIR operation"
    );
    assert!(
        mf.call_sites
            .values()
            .all(|site| !matches!(site.site.op, SiteOp::Dispatched { .. })),
        "the target-neutral graph records no dispatched helper for nobox equality"
    );
}

/// `__op_eq` names SIX declarations and only one of them is callable by that
/// spelling.
///
/// The other five are `@pure @inline operator(lua) ==` overloads over `int`,
/// `float`, `bool`, `string` and `any`, which MIR turns into typed primitive
/// ops. Selecting by parse order reached the right one only because the
/// structural helper is declared first; selecting by the MANGLER is the
/// property that makes a declaration reachable by a bare synthesized name.
#[test]
fn the_structural_helper_is_the_declaration_that_emits_its_bare_name() {
    let ws = check("fn main() { print(\"x\") }");
    let all = ws
        .functions_by_name
        .get(&Ustr::from("__op_eq"))
        .expect("`__op_eq` is declared")
        .len();
    assert!(
        all > 1,
        "the fixture needs the overload set present, else the filter proves nothing"
    );
    let callable = skull_mir::lower::dispatched_callee_decls(&ws, "__op_eq");
    assert_eq!(
        callable.len(),
        1,
        "exactly one of the {all} `__op_eq` declarations emits the bare name, got {}",
        callable.len()
    );
    let f = &ws.module(callable[0].module).functions[callable[0].func];
    assert!(
        f.annotations
            .iter()
            .any(|a| a.name.name.as_str() == "nomangle"),
        "the callable one is the `@nomangle` structural helper"
    );
}
