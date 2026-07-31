//! Stage 0d properties for the shadow instance graph
//! (`docs/monomorphization-plan.md` S 0d, S 12, S 20g.4).

use std::sync::LazyLock;

use bone_semantic::inference::Workspace;
use skull::instance_graph::{self, EdgeKind};
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn check(src: &str) -> Workspace {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

fn graph_of(src: &str) -> (Workspace, instance_graph::InstanceGraph) {
    let ws = check(src);
    let roots = instance_graph::emission_roots(&ws, None);
    let g = instance_graph::build(&ws, roots, false);
    (ws, g)
}

/// One generic function called at two element types is TWO instances. Today's
/// `compute_call_spec` produces at most one specialization here (and usually
/// none, because `any_value` does not hold for primitives).
#[test]
fn one_generic_two_arguments_is_two_instances() {
    let (ws, g) = graph_of(
        r#"
fn ident<T>(x: T) -> T { return x }
fn main() {
    let a = ident(1)
    let b = ident("s")
}
"#,
    );
    let syms = g.sorted_symbols(&ws);
    let idents: Vec<&String> = syms.iter().filter(|s| s.contains("ident")).collect();
    assert_eq!(
        idents.len(),
        2,
        "expected ident<int> and ident<string> as distinct instances, got {idents:?}"
    );
    assert!(idents.iter().any(|s| s.ends_with("<int>")), "{idents:?}");
    assert!(idents.iter().any(|s| s.ends_with("<string>")), "{idents:?}");
}

/// Transitive instantiation: a generic caller passes its OWN binding down, so
/// the callee's instance is decided by the caller's substitution rather than by
/// the inner call site alone.
#[test]
fn instantiation_flows_through_a_generic_caller() {
    let (ws, g) = graph_of(
        r#"
fn inner<T>(x: T) -> T { return x }
fn outer<T>(x: T) -> T { return inner(x) }
fn main() {
    let a = outer(1)
    let b = outer(true)
}
"#,
    );
    let syms = g.sorted_symbols(&ws);
    for want in ["<int>", "<bool>"] {
        assert!(
            syms.iter().any(|s| s.contains("inner") && s.ends_with(want)),
            "inner was not instantiated at {want}: {:?}",
            syms.iter().filter(|s| s.contains("inner")).collect::<Vec<_>>()
        );
    }
}

/// The manifest must not depend on the order roots are discovered in
/// (plan S 20g.4).
///
/// HONEST SCOPE: this pins the property, and it does NOT reproduce the defect
/// that found it. The expansion cap was originally keyed on PATH length, which
/// made membership depend on which root reached an instance first;
/// `games/little-knights` produced two different manifests, and re-applying the
/// path cap still turns that project's `order-stable` column to NO. Every
/// synthetic fixture tried here stays green under the path cap, because the
/// orders have to diverge over a real library's transitive depth. **The corpus
/// run (`rock mono-manifest`, the per-project `order-stable` column) is the
/// oracle for this property; this test is a cheap lower bound.** Do not read it
/// as coverage of the cap.
#[test]
fn manifest_is_independent_of_discovery_order() {
    // The fixture MUST reach the expansion cap, or it proves nothing: a shallow
    // program is order-independent whatever the cap is keyed on. `grow<T>`
    // instantiates at ever-deeper types, and two roots reach it by paths of
    // different lengths, so a path-length cap expands a different set depending
    // on which root is seeded first.
    let src = r#"
fn grow<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + grow(wrapped, n - 1)
}
fn near() -> int { return grow(1, 20) }
fn hop1() -> int { return near() }
fn hop2() -> int { return hop1() }
fn hop3() -> int { return hop2() }
fn hop4() -> int { return hop3() }
fn hop5() -> int { return hop4() }
fn far() -> int { return hop5() }
fn main() { near(); far() }
"#;
    let ws = check(src);
    // `Some("<test>")` makes the STDLIB a library, so its generic bodies are
    // reached only transitively and at path lengths that depend on which root
    // got there first. With every module a root (the `None` form) path depth
    // cannot vary, and this test would pass under any cap -- the corpus found
    // the defect precisely because real projects have library modules.
    let roots = instance_graph::emission_roots(&ws, Some("<test>"));
    let fwd = instance_graph::build(&ws, roots.clone(), false);
    let rev = instance_graph::build(&ws, roots, true);
    assert_eq!(
        instance_graph::render_manifest(&ws, &fwd),
        instance_graph::render_manifest(&ws, &rev),
        "reversing root discovery order changed the manifest"
    );
    assert_eq!(
        instance_graph::manifest_digest(&ws, &fwd),
        instance_graph::manifest_digest(&ws, &rev)
    );
}

/// Two nominals with identical leaves must NOT collapse into one instance --
/// the S 20g.2 adversary, carried through from `MonoTy` into instance identity.
#[test]
fn same_shape_nominals_produce_distinct_instances() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
value Q { x: float, y: float }
fn ident<T>(v: T) -> T { return v }
fn main() {
    let p = ident(P { x: 1.0, y: 2.0 })
    let q = ident(Q { x: 1.0, y: 2.0 })
}
"#,
    );
    let syms = g.sorted_symbols(&ws);
    let idents: Vec<&String> = syms.iter().filter(|s| s.contains("ident")).collect();
    assert_eq!(idents.len(), 2, "P and Q must not share an instance: {idents:?}");
    assert!(idents.iter().any(|s| s.ends_with("<P>")), "{idents:?}");
    assert!(idents.iter().any(|s| s.ends_with("<Q>")), "{idents:?}");
}

/// A `for` over a user collection contributes an `iterator-next` edge with a
/// CONCRETE witness instance (plan S 12). Without reading `next_generics` the
/// witness has no instantiation and the edge is lost entirely -- which is how
/// the first version of this walk reported zero iterator edges over the whole
/// corpus while looking healthy.
#[test]
fn iterator_next_witness_is_instantiated() {
    let (_ws, g) = graph_of(
        r#"
fn main() {
    let xs: [int] = [1, 2, 3]
    let total = 0
    for v in xs.iter().map(_ + 1) { total = total + v }
}
"#,
    );
    let nexts = g.edges.iter().filter(|e| e.kind == EdgeKind::IteratorNext).count();
    assert!(nexts > 0, "no iterator-next edge was recorded");
    assert!(
        g.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::IteratorNext)
            .all(|e| !e.to.args.is_empty()),
        "an iterator-next witness reached a generic callee with no instantiation"
    );
}

/// Ordinary same-instance recursion is finite and must be reported as such,
/// not as a polymorphic-recursion candidate.
#[test]
fn same_instance_recursion_is_not_polymorphic() {
    let (ws, g) = graph_of(
        r#"
fn countdown(n: int) -> int {
    if n <= 0 { return 0 }
    return countdown(n - 1)
}
fn main() { countdown(3) }
"#,
    );
    let census = g.recursion_census(&ws);
    assert!(
        census.direct_same_instance.iter().any(|s| s.contains("countdown")),
        "direct recursion not detected: {:?}",
        census.direct_same_instance
    );
    assert!(
        census.changing_instance.is_empty(),
        "same-instance recursion was misfiled as polymorphic: {:?}",
        census.changing_instance
    );
}

/// Polymorphic recursion (`f<T> -> f<[T]>`) grows the instantiation TYPE
/// without bound. It must terminate as a capped report rather than diverging --
/// and it must be filed as changing-instance, distinct from the case above.
/// The plan (S 7) records that this shape compiles and RUNS today.
#[test]
fn changing_instance_recursion_is_capped_and_reported() {
    let (ws, g) = graph_of(
        r#"
fn depth<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    let wrapped: [T] = [x]
    return 1 + depth(wrapped, n - 1)
}
fn main() { depth(1, 3) }
"#,
    );
    let census = g.recursion_census(&ws);
    assert!(
        !census.changing_instance.is_empty(),
        "polymorphic recursion was not reported as changing-instance"
    );
    // The walk terminated, which is the point of the cap.
    assert!(g.instances.len() < 500, "instance explosion: {}", g.instances.len());
}

/// Plan S 15a: every specialization today's conditional machinery produces must
/// appear in the shadow graph as a complete instance. Equality is NOT expected
/// -- the shadow side is strictly larger, because the old mechanism is
/// conditional -- but an absence in THAT direction means the shadow walk cannot
/// see an edge production already takes.
///
/// A miss whose CALLING body the walk never reached is classified separately:
/// that is a reachability difference (dead code, or a shared generic body no
/// instance reaches), not a missing instance. Over the whole closed world the
/// unexplained count is 0 and the classified count is 569.
#[test]
fn every_current_specialization_appears_in_the_shadow_graph() {
    let (ws, g) = graph_of(
        r#"
value P { x: float, y: float }
@inline fn ident<T>(v: T) -> T { return v }
fn main() {
    let a = ident(P { x: 1.0, y: 2.0 })
    let b = ident(1)
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    xs.push(P { x: 3.0, y: 4.0 })
}
"#,
    );
    let r = instance_graph::reconcile(&ws, &g, bone_semantic::hir::FnBackend::Lua);
    assert!(r.today > 0, "the fixture produced no specializations to reconcile");
    assert!(
        r.missing.is_empty(),
        "current specializations absent from the shadow graph: {:?}",
        r.missing
    );
    assert_eq!(r.rejected_sites, 0, "a Stage-0b invariant fired on a valid program");
    // The expected direction: the shadow graph is strictly larger.
    assert!(r.shadow_only > 0, "shadow graph added nothing over today's specializer");
}
