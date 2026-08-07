//! Controls for [`skull_mir::site::SiteDomain`] -- WHICH expression arena a
//! site's index is meaningful against (plan S 9 / Stage 3, phase A2).
//!
//! A struct literal that omits a field lowers that field's initializer from the
//! declaring struct's `BodyOwner::StructDefaults` body into the enclosing
//! function's CFG. The call is the enclosing INSTANCE's obligation, but its
//! `Idx<Expr>` indexes the defaults arena. The domain is what keeps the two
//! apart; the previous answer was to withhold the stamp entirely, which was
//! fail-closed but not a contract emission could consume.

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::concrete_body::Callee;
use skull::instance_graph::{self, InstanceGraph};
use skull::site_join::{self, SiteJoinCensus, Unjoined};
use skull_mir::lower::ScalarCtx;
use skull_mir::mir::MirFunc;
use skull_mir::site::SiteDomain;
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

/// An instance's arguments as one string, for comparison and for a failure
/// message that names what was found rather than an opaque id.
fn render_args(g: &InstanceGraph, args: &[skull_mir::monoty::MonoTyId]) -> String {
    args.iter().map(|a| g.arena.render(*a)).collect::<Vec<_>>().join(",")
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

fn prepared_body(ws: &Workspace, name: &str) -> MirFunc {
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let (fid, f) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("fn not found");
    let fref = skull_mir::mir::FuncRef { module: mid, func: fid };
    let scx = ScalarCtx::new(ws);
    let subst = skull_mir::lower::SpecSubst::default();
    let mut mf = skull_mir::lower::lower_instance_body(ws, &scx, mid, fid, &subst).expect("lower");
    let mangled = skull_mir::mangle::mangle_fn(ws, f);
    let mut cache = skull_transform::InlineCache::default();
    skull::mir_emit::prepare_instance_body(ws, &scx, fref, &subst, &mangled, &mut mf, &mut cache)
        .expect("prepare");
    mf
}

/// Every `(domain, expr)` a prepared body stamped, nested bodies included.
fn stamped_domains(ws: &Workspace, name: &str) -> Vec<(SiteDomain, u32)> {
    fn walk(mf: &MirFunc, out: &mut Vec<(SiteDomain, u32)>) {
        for c in mf.call_sites.values() {
            out.push((c.site.domain.clone(), c.site.expr.into_raw().into_u32()));
        }
        for nf in &mf.nested_fns {
            walk(nf, out);
        }
    }
    let mut out = Vec::new();
    walk(&prepared_body(ws, name), &mut out);
    out.sort();
    out
}

fn unjoined(c: &SiteJoinCensus, cause: Unjoined) -> usize {
    c.unjoined.get(&cause).copied().unwrap_or(0)
}

/// A default containing a direct call joins, and the enclosing function's own
/// calls are the admission control.
///
/// Without the enclosing-body arm a domain that swallowed every site in the
/// function would pass: nothing would be unjoined because nothing would be
/// looked up in the owner's arena at all.
#[test]
fn a_default_bodys_call_joins_and_the_enclosing_bodys_calls_still_do() {
    let src = r#"
fn seed() -> int { return 7 }
fn other() -> int { return 1 }

struct Cfg {
    n: int = seed(),
    m: int,
}

fn main() {
    let c = Cfg { m: other() }
    print("{c.n + c.m}")
}
"#;
    let c = census_of(src);
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(unjoined(&c, Unjoined::SiteNotInBody), 0, "{c:?}");
    assert_eq!(unjoined(&c, Unjoined::NoSite), 0, "{c:?}");

    let ws = check(src);
    let stamped = stamped_domains(&ws, "main");
    assert!(
        stamped.iter().any(|(d, _)| matches!(d, SiteDomain::StructDefaults(..))),
        "the default's call was not stamped in a defaults domain: {stamped:?}"
    );
    assert!(
        stamped.iter().any(|(d, _)| *d == SiteDomain::Own),
        "the enclosing body's own calls lost their domain: {stamped:?}"
    );
}

/// TWO arenas handing out the SAME numeric `Idx<Expr>` stay distinct.
///
/// This is the whole reason the domain is part of the key rather than a flag
/// beside it. `Cfg`'s defaults body is tiny, so its call sits at a low index --
/// exactly where the enclosing function's own early expressions sit. Asserted
/// on the stamps, because a census that joined both through one arena would
/// still report zero unjoined: it would have found A callee at that index.
#[test]
fn two_arenas_sharing_one_expression_index_stay_distinct() {
    let src = r#"
fn seed() -> int { return 7 }
fn early() -> int { return 1 }

struct Cfg {
    n: int = seed(),
    m: int,
}

fn main() {
    let a = early()
    let c = Cfg { m: a }
    print("{c.n + c.m}")
}
"#;
    let ws = check(src);
    let stamped = stamped_domains(&ws, "main");
    let collisions: Vec<u32> = stamped
        .iter()
        .filter(|(d, _)| matches!(d, SiteDomain::StructDefaults(..)))
        .filter(|(_, e)| stamped.contains(&(SiteDomain::Own, *e)))
        .map(|(_, e)| *e)
        .collect();
    assert!(
        !collisions.is_empty(),
        "the fixture does not put two arenas at one index, so it proves nothing: {stamped:?}"
    );
    // The two are DIFFERENT keys, so the body's table holds both.
    let mut g = graph_of(&ws);
    let bodies = skull::concrete_body::build(&ws, &mut g);
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let main = bodies
        .bodies
        .iter()
        .find(|(i, _)| {
            i.func.module == mid && ws.module(mid).functions[i.func.func].name.as_str() == "main"
        })
        .map(|(_, b)| b)
        .expect("main has a concrete body");
    for e in &collisions {
        let at: Vec<&Callee> = main
            .direct_calls
            .iter()
            .filter(|(s, _)| s.expr.into_raw().into_u32() == *e)
            .map(|(_, c)| c)
            .collect();
        assert_eq!(
            at.len(),
            2,
            "index {e} carries one callee, so the two arenas collapsed: {:?}",
            main.direct_calls.keys().collect::<Vec<_>>()
        );
    }
}

/// A default reached through ANOTHER default -- `Outer { }` omitting a field
/// whose default is `Inner { }`, which omits a field whose default calls --
/// joins.
///
/// The scan is a worklist for exactly this: one pass over the instance's own
/// body sees `Outer`'s literal and nothing below it, and bitgun's `Mob` ->
/// `MobTimers` -> `random_range` is the real case that found it.
#[test]
fn a_default_reached_through_another_default_joins() {
    let src = r#"
fn seed() -> int { return 7 }

struct Inner { k: int = seed() }
struct Outer { i: Inner = Inner {}, m: int }

fn main() {
    let o = Outer { m: 1 }
    print("{o.i.k + o.m}")
}
"#;
    let c = census_of(src);
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        unjoined(&c, Unjoined::SiteNotInBody),
        0,
        "the nested default's call has no recorded obligation: {c:?}"
    );
    // `Outer`'s own default for `i` is a LITERAL and calls nothing, so exactly
    // one defaults arena carries an obligation -- `Inner`'s, two bodies down
    // from `main`. Reaching it at all is what the worklist buys: a single pass
    // over `main` sees `Outer {}` and stops.
    let ws = check(src);
    let defaults: std::collections::BTreeSet<SiteDomain> = stamped_domains(&ws, "main")
        .into_iter()
        .map(|(d, _)| d)
        .filter(|d| matches!(d, SiteDomain::StructDefaults(..)))
        .collect();
    assert_eq!(
        defaults.len(),
        1,
        "expected the INNER defaults arena and only it, got {defaults:?}"
    );
}

/// A field the literal WRITES does not drag its default's callee into the
/// graph.
///
/// The walk restricts itself to the omitted fields' own subtrees. Walking the
/// whole defaults body instead would make `unused_seed` reachable, which is an
/// emitted body no program calls -- invisible to the join, and paid for at
/// every instance once definitions are instance-owned.
#[test]
fn a_written_field_does_not_make_its_defaults_callee_reachable() {
    let src = r#"
fn used_seed() -> int { return 7 }
fn unused_seed() -> int { return 9 }

struct Cfg {
    n: int = used_seed(),
    k: int = unused_seed(),
    m: int,
}

fn main() {
    let c = Cfg { k: 0, m: 1 }
    print("{c.n + c.k + c.m}")
}
"#;
    // NOT asserted on `g.instances`: production's root predicate makes every
    // non-library declaration an emission root, so both helpers are reachable
    // whatever this walk does. The discriminator is which OBLIGATIONS the
    // defaults domain records.
    let ws = check(src);
    let g = graph_of(&ws);
    let targets: std::collections::BTreeSet<String> = g
        .call_sites
        .values()
        .flat_map(|sites| sites.iter())
        .filter(|(site, _)| matches!(site.domain, SiteDomain::StructDefaults(..)))
        .filter_map(|(_, rec)| match &rec.outcome {
            skull::instance_graph::SiteOutcome::Resolved(i) => Some(g.identity(&ws, i)),
            _ => None,
        })
        .collect();
    assert!(
        targets.iter().any(|n| n.starts_with("used_seed")),
        "the OMITTED field's default recorded no obligation, so the negative \
         below is measured over nothing: {targets:?}"
    );
    assert!(
        !targets.iter().any(|n| n.starts_with("unused_seed")),
        "a WRITTEN field's default recorded an obligation: {targets:?}"
    );
}

/// The subtree walk is TOTAL over a defaults body.
///
/// A missed child link in `expr_subtree` would attribute fewer calls and every
/// count would still balance -- the join would simply see an obligation it does
/// not have. The residue is the only place it shows.
#[test]
fn every_defaults_body_expression_belongs_to_some_field_subtree() {
    let ws = check(
        r#"
fn seed() -> int { return 7 }
struct Cfg {
    a: int = if seed() > 2 { seed() } else { 0 },
    b: [int] = [seed(), 1, 2],
    c: (int, int) = (seed(), 1),
    d: int = match seed() { 0 => 1, _ => 2 },
    e: string = "x{seed()}y",
    m: int,
}
fn main() { let c = Cfg { m: 1 } print("{c.a + c.m}") }
"#,
    );
    let mut examined = 0usize;
    let mut residue = 0usize;
    for module in ws.modules.iter() {
        for (owner, body) in module.bodies.iter() {
            if !matches!(owner, bone_semantic::hir::BodyOwner::StructDefaults(_)) {
                continue;
            }
            examined += body.expr_arena.len();
            residue += skull::expr_subtree::defaults_body_residue(body);
        }
    }
    assert!(examined > 0, "no defaults body was examined, so the zero says nothing");
    assert_eq!(
        residue, 0,
        "{residue} of {examined} defaults-body expressions belong to no field subtree"
    );
}

// ---------------------------------------------------------------------------
// PINNED BUG: a generic struct's field default names no instance
// ---------------------------------------------------------------------------

/// A generic struct's field DEFAULT names ONE concrete instance per literal.
///
/// `struct Box<T> { value: T? = seed() }` with `let a: Box<int> = Box {}` and
/// `let b: Box<string> = Box {}` injects the same initializer twice, and the
/// two injections resolve `seed<int>` and `seed<string>`.
///
/// Three things have to hold together for that, and each is asserted here
/// because any one of them alone reads as success:
///
/// - bone publishes the fresh inference var each declared generic was
///   instantiated to (`BodyInference::struct_default_generic_slots`), so the
///   defaults body's open class can be named from outside -- the relation
///   itself is asserted in `struct_default_generic_relation.rs`;
/// - lowering binds those slots to the LITERAL's arguments, so
///   `monoty::convert` resolves instead of answering `unresolved-infer`;
/// - `SiteDomain::StructDefaults` carries the arguments, so the two injections
///   are two obligations rather than one read twice.
///
/// The last is why the ARGUMENTS are compared separately below: both injections
/// lower one expression in one arena, so two domains that differ anywhere else
/// would not be differing by instantiation.
#[test]
fn a_generic_structs_field_default_names_its_own_instance() {
    let src = r#"
fn seed<T>() -> T? { return null }

struct Box<T> { value: T? = seed() }

fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#;
    let ws = check(src);

    // The population: BOTH literals really do inject a call, so the counts
    // below are over two obligations rather than over an empty space.
    let stamped = stamped_domains(&ws, "main");
    let defaults: Vec<&(SiteDomain, u32)> = stamped
        .iter()
        .filter(|(d, _)| matches!(d, SiteDomain::StructDefaults { .. }))
        .collect();
    assert_eq!(
        defaults.len(),
        2,
        "the fixture must inject the default twice, else nothing below is \
         measured over the defect: {stamped:?}"
    );

    // The two injections carry DIFFERENT sites: the domain keys on the
    // instantiation, so `Box<int>` and `Box<string>` are two obligations over
    // one defaults body rather than one obligation read twice.
    assert_ne!(
        defaults[0], defaults[1],
        "two instantiations of one generic struct's defaults collapsed onto one \
         site, so the second silently reads the first's callee"
    );

    // And they differ in the ARGUMENTS, not merely in the expression index --
    // both injections lower the same initializer expression, so a difference
    // anywhere else would not be the instantiation.
    let args: Vec<&Vec<skull_mir::monoty::MonoTy>> = defaults
        .iter()
        .map(|(d, _)| match d {
            SiteDomain::StructDefaults(_, _, a) => a,
            SiteDomain::Own => unreachable!("filtered above"),
        })
        .collect();
    assert_eq!(defaults[0].1, defaults[1].1, "same initializer expression");
    assert_ne!(args[0], args[1], "the domains differ somewhere other than the arguments");

    let c = census_of(src);
    assert!(c.accounts(), "accounting identity broken: {c:?}");
    assert_eq!(
        unjoined(&c, Unjoined::SiteNotInBody),
        0,
        "a generic struct default's calls must join: {c:?}"
    );

    // LOWERING's side: both calls named a concrete instantiation and it AGREED
    // with the graph's. `joined_instance_verified` is the only arm that says so
    // -- distinct sites are not the property, and two distinct sites both
    // resolving the SHARED generic body satisfy every other assertion here.
    // Measured: emptying lowering's slot substitution while leaving the domain
    // key intact moves this 2 -> 0 and `CalleeNoInstance` 0 -> 2, and changes
    // nothing else in the census.
    assert_eq!(c.joined_instance_verified, 2, "{c:?}");
    assert_eq!(unjoined(&c, Unjoined::CalleeNoInstance), 0, "{c:?}");

    // THE WALK's side, which is a second derivation and can fail alone: two
    // distinct concrete `seed` instances exist at all.
    let g2 = graph_of(&check(src));
    let seeds: Vec<&Vec<skull_mir::monoty::MonoTyId>> = g2
        .instances
        .iter()
        .filter(|i| {
            ws.module(i.func.module).functions[i.func.func].name.as_str() == "seed"
        })
        .map(|i| &i.args)
        .collect();
    assert_eq!(
        seeds.len(),
        2,
        "expected `seed<int>` and `seed<string>`, got {}: {:?}",
        seeds.len(),
        seeds.iter().map(|a| render_args(&g2, a)).collect::<Vec<_>>()
    );
    let mut rendered: Vec<String> = seeds.iter().map(|a| render_args(&g2, a)).collect();
    rendered.sort();
    assert_eq!(rendered, vec!["int".to_string(), "string".to_string()]);

    // The program is LEGAL, so the walk rejects nothing.
    let g = graph_of(&ws);
    let unconstrained = g
        .rejections
        .iter()
        .filter(|r| {
            matches!(r.kind, skull::instance_graph::RejectionKind::UnconstrainedTypeVar)
        })
        .count();
    assert_eq!(
        unconstrained, 0,
        "a legal generic-struct default is reported as an unconstrained type \
         variable: {:?}",
        g.rejections
    );
}

/// CONTROL for the sentinel above: a struct default whose callee is NON-generic
/// joins, measured as a DELTA against the same program with the fields written.
///
/// Without this the sentinel could pass because struct defaults are broken in
/// general rather than because the GENERIC instantiation is undetermined.
///
/// A delta rather than absolute counts, because the census walks every expanded
/// instance including the stdlib's: a global "some non-generic call joined" is
/// satisfied by calls this fixture never made, and filtering a cause out of the
/// unjoined map hides an increase in it. The two programs differ in exactly one
/// thing -- three injected `tick()` calls -- so the whole census must differ in
/// exactly that: three more obligations, three more non-generic joins, and an
/// unjoined map that is IDENTICAL, `owner-not-an-instance` included.
#[test]
fn a_generic_structs_default_calling_a_non_generic_fn_still_joins() {
    const DECLS: &str = r#"
fn tick() -> int { return 1 }

struct Plain { n: int = tick() }
struct Box<T> { n: int = tick() }
"#;
    // The fields WRITTEN: no default is injected, so no `tick()` call exists in
    // `main`. Every declaration is identical, so `tick` is an emission root in
    // both and the rest of the census is unchanged by construction.
    // Match `tick()`'s result so both arms feed identical values into control flow.
    let written = format!(
        "{DECLS}
fn main() {{
    let p = Plain {{ n: 1 }}
    let a: Box<int> = Box {{ n: 1 }}
    let b: Box<string> = Box {{ n: 1 }}
    if p.n + a.n + b.n > 0 {{ print(\"ok\") }}
}}
"
    );
    // The fields OMITTED: three injections, one per literal.
    let omitted = format!(
        "{DECLS}
fn main() {{
    let p = Plain {{}}
    let a: Box<int> = Box {{}}
    let b: Box<string> = Box {{}}
    if p.n + a.n + b.n > 0 {{ print(\"ok\") }}
}}
"
    );

    let ws = check(&omitted);
    // THE POPULATION, lowering side: three literals inject `tick()`.
    let stamped = stamped_domains(&ws, "main");
    let defaults: Vec<&(SiteDomain, u32)> = stamped
        .iter()
        .filter(|(d, _)| matches!(d, SiteDomain::StructDefaults(..)))
        .collect();
    assert_eq!(
        defaults.len(),
        3,
        "expected three defaults-domain stamps in `main`, got {defaults:?}"
    );

    let base = census_of(&written);
    let with_defaults = census_of(&omitted);
    assert!(base.accounts() && with_defaults.accounts(), "accounting identity broken");

    // The three injected calls are examined...
    assert_eq!(
        with_defaults.obligations_examined as i64 - base.obligations_examined as i64,
        3,
        "the injected calls did not reach the census as three obligations: \
         {} vs {}",
        base.obligations_examined,
        with_defaults.obligations_examined
    );
    // ...and all three JOIN, as non-generic (one instance, so the declaration
    // match IS the instance match).
    assert_eq!(
        with_defaults.joined_non_generic as i64 - base.joined_non_generic as i64,
        3,
        "the injected calls were examined but did not join non-generically: \
         {} vs {}",
        base.joined_non_generic,
        with_defaults.joined_non_generic
    );
    // ...and NOTHING became unjoined, under any cause. Compared as whole maps,
    // so an increase in `owner-not-an-instance` -- Cut C+D's own population,
    // which both programs carry from the stdlib -- fails here rather than
    // being filtered away.
    assert_eq!(
        with_defaults.unjoined, base.unjoined,
        "injecting the defaults changed the unjoined population"
    );
}

