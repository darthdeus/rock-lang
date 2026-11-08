//! Pin the `JS_RNG_RUNTIME` stream to LuaJIT's 32-bit RNG contract.
//! Runs the ACTUAL prelude const under node.

use skull_codegen_js::JS_RNG_RUNTIME;

/// Run the RNG prelude + `body`, return trimmed stdout.
fn node_run(body: &str) -> String {
    let program = format!("{JS_RNG_RUNTIME}\n{body}");
    let out = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn xorshift32_seed42_matches_luajit_draws_and_signed_state() {
    if !crate::common::require_node() {
        return;
    }
    let got = node_run(
        "__rock_rng_seed(42); var d=[]; \
         d.push(__rock_rng_next32()); d.push(__rock_rng_next32()); \
         var secondState=__rock_rng_state_get(); \
         d.push(__rock_rng_next32()); d.push(__rock_rng_next32()); \
         console.log(d.join(\",\")+\"|\"+secondState);",
    );
    assert_eq!(
        got,
        "1349475785,4158816443,2940726184,3486860168|-136153255"
    );
}

#[test]
fn signed_seed_and_state_set_wrap_to_luajit_state() {
    if !crate::common::require_node() {
        return;
    }
    let got = node_run(
        "__rock_rng_seed(-1); var seeded=__rock_rng_state_get(); \
         __rock_rng_state_set(4294967295); var restored=__rock_rng_state_get(); \
         console.log(seeded+\",\"+restored);",
    );
    assert_eq!(got, "-1,-1");
}

#[test]
fn presentation_stream_is_independent_and_matches_luajit_sequence() {
    if !crate::common::require_node() {
        return;
    }
    let got = node_run(
        "__rock_rng_seed(42); __rock_presentation_rng_seed(42); \
         var gameplayFirst=__rock_rng_next32(); \
         var presentationFirst=__rock_presentation_rng_next32(); \
         var presentationSecond=__rock_presentation_rng_next32(); \
         var gameplaySecond=__rock_rng_next32(); \
         console.log([gameplayFirst,presentationFirst,presentationSecond,gameplaySecond].join(\",\"));",
    );
    assert_eq!(got, "1349475785,1349475785,4158816443,4158816443");
}

#[test]
fn random_int_seed42_first5() {
    if !crate::common::require_node() {
        return;
    }
    let got = node_run(
        "__rock_rng_seed(42); var d=[]; for (var i=0;i<5;i++) d.push(__rock_random_int(0,6)); console.log(d.join(\",\"));",
    );
    assert_eq!(got, "5,5,4,2,4");
}

#[test]
fn random_in_unit_interval() {
    if !crate::common::require_node() {
        return;
    }
    let got = node_run(
        "__rock_rng_seed(99); var ok=true; for (var i=0;i<100000;i++){var r=__rock_random(); if(r<0||r>=1)ok=false;} console.log(ok);",
    );
    assert_eq!(got, "true");
}

#[test]
fn random_int_large_range_covers_beyond_2pow32() {
    if !crate::common::require_node() {
        return;
    }
    // r = 9e9 > 2^32 forces the two-draw 53-bit compose path; assert every draw
    // stays in [0, r) and the observed max climbs above 2^32 (range coverage is
    // the semantic a single 32-bit draw would silently violate).
    let got = node_run(
        "__rock_rng_seed(123); var r=9000000000, ok=true, mx=0; \
         for (var i=0;i<200000;i++){var v=__rock_random_int(0,r); if(v<0||v>=r)ok=false; if(v>mx)mx=v;} \
         console.log(ok && mx > 4294967296);",
    );
    assert_eq!(got, "true");
}

#[test]
fn state_get_set_roundtrip_reproduces_stream() {
    if !crate::common::require_node() {
        return;
    }
    // The single-word state is the replay-serialization surface: capture it,
    // advance, restore, and the same draws must reappear.
    let got = node_run(
        "__rock_rng_seed(7); __rock_rng_next32(); __rock_rng_next32(); \
         var st=__rock_rng_state_get(); var a=[__rock_rng_next32(),__rock_rng_next32()]; \
         __rock_rng_state_set(st); var b=[__rock_rng_next32(),__rock_rng_next32()]; \
         console.log(a[0]===b[0] && a[1]===b[1]);",
    );
    assert_eq!(got, "true");
}
