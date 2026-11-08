//! The generation-staleness guard at the `query::type_at` chokepoint. Lives
//! here because `query` now lives in bone-query; `bone`'s own structural tests
//! cover everything that doesn't need a query.

use bone_semantic::prelude::{BodyOwner, Ustr};
use bone_semantic::semantic_passes::semantic_pass;

// A body relowered after its inference was cached must trip the generation
// debug_assert at the query chokepoint -- a missed invalidation is a loud panic,
// not a silently stale type. This simulates exactly what the incremental update
// path does on a body edit (bump the Body generation) WITHOUT re-inferring, then
// queries through the stale cache.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stale inference")]
fn stale_body_generation_panics_at_type_at() {
    let src = "fn f() -> int {\n    let a = 1\n    a\n}";
    let module = skull_parser::parse_module_clean(src, Ustr::from("<query_generation>")).unwrap();
    let mut ws = semantic_pass(&mut [module]).unwrap();

    let mid = ws.source_modules().next().expect("one user module").id;
    let (f_idx, _) = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .expect("fn f exists");

    // the offset of `a` in the body (the let initializer's variable use)
    let offset = src.rfind("    a").unwrap() + 4;

    // sanity: the query works against matching generations
    let inf = ws.inference(mid).expect("user module inferred").clone();
    assert!(
        bone_semantic::query::type_at(&ws, mid, &inf, offset).is_some(),
        "marker offset must resolve before the bump"
    );

    // simulate a relower-without-reinfer (the bug class the guard detects)
    ws.modules
        .get_mut(mid.index())
        .unwrap()
        .bodies
        .get_mut(&BodyOwner::Function(f_idx))
        .unwrap()
        .generation += 1;

    let _ = bone_semantic::query::type_at(&ws, mid, &inf, offset);
}
