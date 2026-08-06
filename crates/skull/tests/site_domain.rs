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
    let mut mf = skull_mir::lower::lower_function_spec(ws, &scx, mid, fid, &subst).expect("lower");
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
            out.push((c.site.domain, c.site.expr.into_raw().into_u32()));
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
