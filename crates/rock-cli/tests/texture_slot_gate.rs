//! A texture slot drawn with but never marked filled must fail a bounded run.
//!
//! The injection preserves the real texture bindings and suppresses only their
//! filled stamps, so pixel output cannot make the failure pass accidentally.

use crate::stone_harness::{HeavyCommandExt, SpawnBudget};

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run_frame(force_unfilled: bool) -> (String, bool) {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = crate::command::rock_bin();
    cmd.current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "5"])
        .arg("--output")
        .arg(out_dir.path().join("main.lua"))
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    if force_unfilled {
        cmd.env("STONE_TEXTURE_SLOT_FORCE_UNFILLED", "1");
    }
    let out = cmd.output_with_deadline().expect("spawn rock frame");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

fn run_sim(force_unfilled: bool) -> (String, bool) {
    let env: &[(&str, &str)] =
        if force_unfilled { &[("STONE_TEXTURE_SLOT_FORCE_UNFILLED", "1")] } else { &[] };
    run_named_sim("tonemap_input_order", env)
}

fn run_named_sim(name: &str, env: &[(&str, &str)]) -> (String, bool) {
    let mut cmd = crate::command::rock_bin();
    cmd.current_dir(repo_root())
        .args(["sim", "bitgun", name])
        .env("ROCK_FRAME_TIMEOUT_SECS", "120");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output_with_deadline().expect("spawn rock sim");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (text, out.status.success())
}

#[test]
fn unfilled_texture_slot_fails_a_bounded_run() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (control, control_ok) = run_frame(false);
    assert!(control_ok, "control run must succeed.\n{control}");
    assert!(
        !control.contains("texture slot(s) drawn with but never filled"),
        "control run reported an unfilled texture slot.\n{control}"
    );

    let (injected, injected_ok) = run_frame(true);
    assert!(
        !injected_ok,
        "a bounded run with an unfilled texture slot must exit nonzero.\n{injected}"
    );
    assert!(
        injected.contains("texture slot(s) drawn with but never filled"),
        "the run must report the texture-slot failure count.\n{injected}"
    );
}

#[test]
fn unfilled_texture_slot_fails_a_sim() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (control, control_ok) = run_sim(false);
    assert!(control_ok, "control sim must succeed.\n{control}");

    let (injected, injected_ok) = run_sim(true);
    assert!(
        !injected_ok,
        "a sim with an unfilled texture slot must fail.\n{injected}"
    );
    assert!(
        injected.contains("texture slots drawn with but never filled"),
        "stop_sim must report its texture-slot invariant.\n{injected}"
    );

    // The DIAGNOSTIC, not just the summary count. `audit_texture_slots_for_draw`
    // must key its log_once per (shader, slot): with the 2-arg form the auto
    // __SITE__ is one line, so the first pair reported and every later one was
    // deduped away for the rest of the run -- the count stayed right, so nothing
    // caught it. tonemap declares exactly two slots and this sim drives it, so
    // both lines must appear.
    for slot in [0, 1] {
        let want = format!("`stone::tonemap_frag` drew with slot {slot}");
        assert!(
            injected.contains(&want),
            "every unfilled (shader, slot) must report, not just the first -- \
             missing `{want}`.\n{injected}"
        );
    }
}

/// An activation mark answers for EVERY draw in the activation. Without a
/// second tier the first audit clears it and every later draw false-alarms,
/// which is what makes a four-sampler program unauditable per draw.
#[test]
fn an_activation_mark_survives_every_audit_until_the_program_is_rebound() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (ok, ok_success) =
        run_named_sim("texture_slot_mark_tiers", &[("SIM_TIER_ARM", "activation_survives")]);
    assert!(
        ok_success,
        "an activation mark must satisfy every audit in its activation.\n{ok}"
    );

    // The other half of the tier: a PER-DRAW mark answers for one draw only, or
    // an earlier binding hides a later missing one.
    let (per_draw, per_draw_success) =
        run_named_sim("texture_slot_mark_tiers", &[("SIM_TIER_ARM", "per_draw_cleared")]);
    assert!(
        !per_draw_success,
        "the second audit must report all three slots, failing the run.\n{per_draw}"
    );
    assert!(
        per_draw.contains("and none of them survives into the next draw (missing=3)"),
        "per-draw marks must be cleared after every audit.\n{per_draw}"
    );

    // Re-binding the program starts a new activation, so the previous one's
    // bindings must stop answering: "bound while this program is current" is
    // scoped to the current bind, not to the program's whole lifetime.
    let (rebind, rebind_success) =
        run_named_sim("texture_slot_mark_tiers", &[("SIM_TIER_ARM", "rebind_clears")]);
    assert!(
        !rebind_success,
        "the audit after a re-bind must report the unbound slot, failing the run.\n{rebind}"
    );
    assert!(
        rebind.contains("re-binding the program drops its activation marks (missing=1)"),
        "a re-bind must clear the program's activation marks.\n{rebind}"
    );
}

/// A nonzero image handle is not a bind: `sprite.rs` misses in IMAGES, binds
/// nothing, sets no sampler uniform and draws anyway. The gate must mark slot 0
/// from the RESOLVED texture, or it calls that slot filled while the shader
/// samples whatever the previous draw left on unit 0.
#[test]
fn an_unresolvable_image_handle_is_not_a_filled_slot() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (control, control_ok) = run_named_sim("implicit_texture_bind", &[]);
    assert!(
        control_ok,
        "a resolvable image must bind unit 0 and satisfy slot 0.\n{control}"
    );

    let (bad, bad_ok) = run_named_sim("implicit_texture_bind", &[("SIM_IMPLICIT_BAD_HANDLE", "1")]);
    assert!(!bad_ok, "an unresolvable handle must fail the run.\n{bad}");
    assert!(
        bad.contains("`stone::sprite_basic_vert+stone::sprite_basic_frag` drew with slot 0"),
        "the unresolvable handle must report slot 0 unbound.\n{bad}"
    );
}

#[test]
fn an_unresolvable_explicit_bind_is_not_a_filled_slot() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (control, control_ok) = run_named_sim("explicit_texture_bind", &[]);
    assert!(control_ok, "a resolved explicit bind must satisfy its slot.\n{control}");

    let (bad, bad_ok) =
        run_named_sim("explicit_texture_bind", &[("SIM_EXPLICIT_BAD_HANDLE", "1")]);
    assert!(!bad_ok, "an unresolved explicit bind must fail the run.\n{bad}");
    assert!(
        bad.contains("`stone::sprite_batch_vert+stone::sprite_batch_frag` drew with slot 3"),
        "the failed bind must leave slot 3 unfilled instead of certifying stale unit state.\n{bad}"
    );
}

#[test]
fn a_required_texture_bind_rejects_an_unknown_sampler() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let (out, ok) = run_named_sim("unknown_texture_sampler", &[]);
    assert!(ok, "the draw itself remains valid.\n{out}");
    for sampler in ["not_a_sampler", "not_a_sampler_either"] {
        let want = format!("`stone::sprite_basic_vert+stone::sprite_basic_frag` has no sampler `{sampler}`");
        assert!(
            out.contains(&format!("ERROR] [SHADER] {want}")),
            "each required bind miss needs its own diagnostic key; missing `{want}`.\n{out}"
        );
    }
    assert!(
        out.contains("ERROR] [SHADER] `bitgun::required_bind_no_texture_frag` declares no @textures group -- nothing can bind required sampler `uTexture`"),
        "a required bind against a shader with no texture group must diagnose the missing group.\n{out}"
    );
}
