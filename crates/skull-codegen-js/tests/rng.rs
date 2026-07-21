//! Pin the `JS_RNG_RUNTIME` mulberry32 stream (M2, docs/js-deployment-plan.md).
//! The JS stream differs from Lua's splitmix by doctrine (bit-parity is a
//! non-goal); this pins the JS stream INDEPENDENT of node's own PRNG so a
//! refactor of the prelude can't silently shift it. Runs the ACTUAL prelude
//! const under node, node-gated with a skip-log (never a silent pass).

use std::process::Command;

use skull_codegen_js::JS_RNG_RUNTIME;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the RNG prelude + `body`, return trimmed stdout.
fn node_run(body: &str) -> String {
    let program = format!("{JS_RNG_RUNTIME}\n{body}");
    let out = Command::new("node")
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
fn mulberry32_seed42_first4_draws() {
    if !node_available() {
        eprintln!("SKIP mulberry32_seed42_first4_draws: node not found");
        return;
    }
    let got = node_run(
        "__rock_rng_seed(42); var d=[]; for (var i=0;i<4;i++) d.push(__rock_rng_next32()); console.log(d.join(\",\"));",
    );
    assert_eq!(got, "2581720956,1925393290,3661312704,2876485805");
}

#[test]
fn random_int_seed42_first5() {
    if !node_available() {
        eprintln!("SKIP random_int_seed42_first5: node not found");
        return;
    }
    let got = node_run(
        "__rock_rng_seed(42); var d=[]; for (var i=0;i<5;i++) d.push(__rock_random_int(0,6)); console.log(d.join(\",\"));",
    );
    assert_eq!(got, "0,4,0,5,0");
}

#[test]
fn random_in_unit_interval() {
    if !node_available() {
        eprintln!("SKIP random_in_unit_interval: node not found");
        return;
    }
    let got = node_run(
        "__rock_rng_seed(99); var ok=true; for (var i=0;i<100000;i++){var r=__rock_random(); if(r<0||r>=1)ok=false;} console.log(ok);",
    );
    assert_eq!(got, "true");
}

#[test]
fn random_int_large_range_covers_beyond_2pow32() {
    if !node_available() {
        eprintln!("SKIP random_int_large_range_covers_beyond_2pow32: node not found");
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
    if !node_available() {
        eprintln!("SKIP state_get_set_roundtrip_reproduces_stream: node not found");
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
