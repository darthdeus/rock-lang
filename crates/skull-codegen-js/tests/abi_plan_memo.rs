//! The JS scalar-ABI check must ask the SHARED ABI-plan oracle rather than
//! restate its derivation. With inlining off nothing else re-asks an Owner
//! question, so an owner HIT can only come from a consumer re-reading the plan
//! definition lowering stored -- which is exactly the unification under test.

use crate::common::ws_with;

#[test]
fn the_scalar_abi_check_reads_the_shared_owner_oracle() {
    let _inline_off = crate::common::InlineOffGuard::on();
    let ws = ws_with(
        "fn helper(x: int) -> int { return x }\n\
         fn first() -> int { return helper(1) }\n\
         fn second() -> int { return helper(2) }\n\
         fn main() { let r = first() + second() print(\"{r}\") }\n",
    );
    let mid = crate::common::test_mid(&ws);
    let (_, cache) = skull_codegen_js::emit::emit_program_cached(&ws, mid).unwrap();
    let stats = cache.abi_plan_stats();
    assert!(
        stats.owner_hits + stats.owner_misses > 0,
        "no owner question was asked at all -- the fixture lost its subject: {stats:?}"
    );
    assert!(
        stats.owner_hits > 0,
        "definition lowering stores each body's Owner plan and the scalar-ABI \
         check re-asks it per call; zero owner hits means the check bypassed \
         the shared oracle: {stats:?}"
    );
    assert!(
        stats.duplicate_cold_stores == 0,
        "a duplicate cold store means two consumers derived one key: {stats:?}"
    );
}
