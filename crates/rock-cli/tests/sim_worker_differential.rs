//! The Phase B differential oracle (`docs/sim-speed-plan.md` B0).
//!
//! A boot-checkpoint worker is only allowed to become the default when a sim
//! that ran after a predecessor in ONE process is observationally identical to
//! the same sim in a fresh process. This is the gate that decides that. The B2
//! boundary (`sim_worker_restore_boot`) restores the Lua-visible world --
//! globals, statics, RNG, buffer bytes, engine config, input maps, the poller
//! -- and the remaining sentinels (logger dedup, subprocesses) pin exactly the
//! native half B3 still owes.
//!
//! Both sides of every comparison are produced by the same spawn/demux/record
//! code with the same schema, differing only in how many entries share the
//! process. Comparison is over structured records -- never over the runner's
//! colored terminal output.
//!
//! Each fixture owns ONE state class and lives in its own
//! `games/testbed/tests/sims/b0_*.rock` file, so a failure in one cannot mask
//! the next.

use rock_cli::run_sim::{run_differential, run_worker_suite, DifferentialPlan};
use rock_cli::sim_record::{compare, Comparison, SimRecord, COMPARED_ARMS};
use rock_cli::Backend;

/// Arms whose denominator these fixtures deliberately leave at zero, with the
/// reason. `errors` counts runner/sim `error` ops, and every op that emits one
/// also fails its sim -- a fixture built to populate it would be a permanently
/// red entry in `rock sim testbed`. Its comparator arm is still mutation-
/// guarded twice over: `Comparison::population` panics when an arm is missing
/// entirely, and `sim_record`'s synthetic per-arm tests fail when an arm is
/// censused but no longer compared.
const ARMS_WITHOUT_A_FIXTURE: &[&str] = &["errors"];

fn differential(predecessors: &[&str], target: &str) -> (SimRecord, SimRecord) {
    let plan = DifferentialPlan {
        predecessors: predecessors.iter().map(|s| s.to_string()).collect(),
        target: target.to_string(),
    };
    run_differential(Backend::Skull, "testbed", &plan)
        .unwrap_or_else(|e| panic!("differential for `{target}` failed to run: {e:#}"))
}

fn assert_equivalent(what: &str, c: &Comparison) {
    assert!(
        c.is_equivalent(),
        "`{what}` diverged between a fresh process and a worker:\n{}",
        c.report()
    );
}

fn artifact_names(record: &SimRecord) -> Vec<String> {
    record.artifacts.keys().cloned().collect()
}

/// Distinct entries get distinct `$OUT`, in a worker as in a fresh process.
#[test]
fn a_worker_entry_gets_its_own_out_directory() {
    let (isolated, worker) = differential(&["b0_out_writer"], "b0_out_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_out_probe", &c);

    assert!(
        c.population("artifacts") > 0,
        "nothing was compared: the fixture produced no artifacts"
    );
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(artifact_names(&worker), vec!["probe.txt".to_string()]);
    assert_eq!(
        artifact_names(&isolated),
        artifact_names(&worker),
        "the predecessor's writer.txt must not appear under the probe's OUT"
    );
}

/// The other half of the same contract: a comma-chain's parts deliberately
/// SHARE one `$OUT`, and a per-entry reset must not break that.
#[test]
fn a_comma_chain_still_shares_one_out_directory_in_a_worker() {
    let (isolated, worker) = differential(&["b0_out_writer"], "b0_chain_head,b0_chain_tail");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_chain_head,b0_chain_tail", &c);

    assert_eq!(
        artifact_names(&worker),
        vec!["chain.txt".to_string(), "tail.txt".to_string()],
        "both parts wrote into one shared OUT"
    );
    assert_eq!(artifact_names(&isolated), artifact_names(&worker));

    // The tail also has a standalone branch, so agreement is only evidence
    // once both sides are known to have taken the CHAINED one.
    const CHAINED: &str = "the head part's file reads back in the tail";
    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            record.expects_pass.iter().any(|m| m.message.contains(CHAINED)),
            "{label}: the tail took its standalone branch, so the fixture proved \
             nothing about sharing: {:?}",
            record.expects_pass
        );
    }
}

/// Deferred artifact processing runs while `$OUT` is live, and the manifest is
/// taken before `cleanup_path` sweeps: a runner that tore the directory down
/// first would silently lose the swept file from the record.
#[test]
fn deferred_artifacts_are_recorded_while_out_is_still_live() {
    let (isolated, worker) = differential(&["b0_out_writer"], "b0_deferred_artifact");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_deferred_artifact", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert_eq!(
            artifact_names(record),
            vec!["kept.txt".to_string(), "swept.txt".to_string()],
            "{label}: the swept file must still be in the manifest"
        );
    }
}

/// The boot-checkpoint restore at the worker boundary
/// (`sim_worker_restore_boot`) puts game globals back: a counter the
/// predecessor bumped reads fresh in the next entry, in the probe's expect and
/// in the file whose contents encode the value.
#[test]
fn a_predecessors_global_does_not_reach_the_next_entry() {
    let (isolated, worker) = differential(&["b0_leak_seed"], "b0_leak_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_leak_probe", &c);

    // Agreement is only evidence when the probe measured: it passed its own
    // fresh-counter expect and wrote the counter into an artifact.
    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(
        worker.artifacts["count.txt"].len, 1,
        "the counter is written as one digit"
    );
    assert_eq!(isolated.artifacts["count.txt"], worker.artifacts["count.txt"]);
}

/// The checkpoint restores the RNG stream position, so the probe's first draw
/// after a predecessor is the same one a fresh `ROCK_TEST_SEED=42` process
/// makes. Only the produced file can see this class -- both sides pass their
/// own expects either way -- so the load-bearing comparison is the artifact's
/// CONTENTS.
#[test]
fn the_rng_stream_position_resets_at_the_entry_boundary() {
    let (isolated, worker) = differential(&["b0_rng_seed"], "b0_rng_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_rng_probe", &c);

    assert!(c.population("artifacts") > 0, "the probe never wrote its draw");
    assert_eq!(isolated.artifacts["draw.txt"], worker.artifacts["draw.txt"]);
}

/// A `static` local in an ordinarily-called function restores at the boundary:
/// the probe's bump reaches a fresh cell.
#[test]
fn a_called_function_static_resets_at_the_entry_boundary() {
    let (isolated, worker) = differential(&["b0_static_called_seed"], "b0_static_called_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_static_called_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(
        isolated.artifacts["called_static.txt"],
        worker.artifacts["called_static.txt"]
    );
}

/// The same state class reached through INLINED call sites, where the cell is
/// read and written directly in the caller's body rather than through a call.
/// Kept separate from its called-function sibling because a registry built
/// from emitted bodies treats the two differently.
#[test]
fn an_inlined_static_resets_at_the_entry_boundary() {
    let (isolated, worker) = differential(&["b0_static_inlined_seed"], "b0_static_inlined_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_static_inlined_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(
        isolated.artifacts["inlined_static.txt"],
        worker.artifacts["inlined_static.txt"]
    );
}

/// Logger suppression state restores at the boundary -- and this is the reason
/// the `entry_diagnostics` arm exists. `log_once` dedups per call site in a Lua
/// global outside `__ROCK_GLOBALS`, so the census never reaches it and a
/// predecessor that logged through the same helper used to SUPPRESS the probe's
/// warning entirely. No other arm can see that: the sim passes, writes nothing,
/// and exits cleanly either way.
#[test]
fn logger_dedup_does_not_suppress_the_next_entrys_warning() {
    let (isolated, worker) = differential(&["b0_dedup_seed"], "b0_dedup_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_dedup_probe", &c);

    // Agreement over silence would be agreement over nothing: the class is only
    // observable when the isolated side actually emitted the warning.
    assert!(
        isolated.entry_diagnostics.contains("b0 dedup fixture warning"),
        "the isolated run must actually emit the warning: {:?}",
        isolated.entry_diagnostics
    );
    assert!(
        worker.entry_diagnostics.contains("b0 dedup fixture warning"),
        "the worker's second entry must emit it too: {:?}",
        worker.entry_diagnostics
    );
}

/// The throttle half of the same class, and a separate test for the same reason
/// the fixture is separate: a last-fire TIMESTAMP compared against restored game
/// time resets differently from a count that suppresses forever, and one
/// fixture's first failure would hide the other.
#[test]
fn a_throttled_sites_last_fire_does_not_reach_the_next_entry() {
    let (isolated, worker) = differential(&["b0_throttle_seed"], "b0_throttle_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_throttle_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            record.entry_diagnostics.contains("b0 throttle fixture warning"),
            "{label}: nothing was emitted, so the agreement is over silence: {:?}",
            record.entry_diagnostics
        );
    }
}

/// The clock the throttle stamps are measured AGAINST, which is a third fixture
/// and not a variant of the second. `frame_core_pre_update` restamps
/// `__stone_log_now_override` from engine `time` only on the following frame,
/// while `sim_start_entry` ends in `run(handle)` -- so an entry's opening
/// statements see its predecessor's clock, and a site with a real boot stamp
/// then measures against it. `sim_publish_log_clock` sets the clock from engine
/// `time` at both points instead -- leaving it unset falls back to `os.clock()`,
/// which climbs across entries.
#[test]
fn the_throttle_clock_does_not_reach_the_next_entry() {
    let (isolated, worker) = differential(&["b0_clock_seed"], "b0_clock_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_clock_probe", &c);

    // The fixture is only meaningful while the site is SUPPRESSED on both
    // sides: a leaked clock is what makes it fire, so agreement over two
    // firings would mean the boot stamp never landed.
    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            !record.entry_diagnostics.contains("b0 clock fixture warning"),
            "{label}: the throttled site fired inside the entry, so the boot stamp \
             it must be measured against was missing: {:?}",
            record.entry_diagnostics
        );
    }
}

/// Engine configuration restores at the boundary. `TIME_SCALE` is a Stone
/// global outside replay's six-global predicate; the checkpoint's membership is
/// the whole registry, so the probe sees real time -- in the scale field AND in
/// the dt its own frame advances, which is the reason this class matters.
#[test]
fn engine_configuration_resets_at_the_entry_boundary() {
    let (isolated, worker) = differential(&["b0_config_seed"], "b0_config_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_config_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            record
                .expects_pass
                .iter()
                .any(|m| m.message.contains("one frame advances a full fixed dt")),
            "{label}: the probe never measured game time, so agreement proves \
             nothing about the dt the scale feeds: {:?}",
            record.expects_pass
        );
    }
    assert_eq!(isolated.artifacts["scale.txt"], worker.artifacts["scale.txt"]);
}

/// Input edges restore at the boundary: a predecessor that stopped mid-hold
/// left `keys_down` set, and `input_end_frame` would roll it forward forever --
/// the restored map starts the probe with nothing held and no release edge.
#[test]
fn a_held_key_does_not_reach_the_next_entry() {
    let (isolated, worker) = differential(&["b0_input_seed"], "b0_input_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_input_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    // "held" and "free" are the same length, so the CONTENTS comparison is the
    // load-bearing one -- a name-or-length manifest would call a leak equal.
    assert_eq!(isolated.artifacts["hold.txt"], worker.artifacts["hold.txt"]);
}

/// A task left live at `stop_sim()` does not tick in the next entry: restoring
/// `POLLER` rebinds the boot arena, so the predecessor's registration is
/// ABANDONED -- never polled again, and its terminal defers deliberately do not
/// fire, because a fresh process's exit would not have fired them either.
///
/// The discriminant is a DELTA measured strictly inside the probe's window, not
/// the counter's value: a value would only repeat the leaked-global class,
/// while a delta can move only if foreign work is still being polled here.
#[test]
fn a_task_left_live_at_stop_sim_does_not_tick_in_the_next_entry() {
    let (isolated, worker) = differential(&["b0_poller_seed"], "b0_poller_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_poller_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "nothing foreign ticks in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(
        worker.artifacts["ticks.txt"].len, 1,
        "the probe writes its one-digit delta, so the zero was measured"
    );
    assert_eq!(isolated.artifacts["ticks.txt"], worker.artifacts["ticks.txt"]);
}

/// A native resource -- and the one native class the runtime exposes an honest
/// observer for: handles are 1-based registry slots (`stone-rs/src/proc.rs`) and
/// `stone_proc_alive` answers per handle, so the fixture compares handle SETS.
/// A count could not see this at all -- the registry reuses a freed slot, so the
/// freed and the leaked case have the same count.
///
/// The boundary's teardown (`sim_worker_teardown_procs`) has to satisfy both
/// views: nothing the predecessor spawned is still live, AND the probe's own
/// spawn lands on the first slot rather than being pushed past a held one. A
/// teardown that killed the child but kept the slot passes the first and fails
/// the second.
#[test]
fn a_subprocess_does_not_outlive_the_entry_that_spawned_it() {
    let (isolated, worker) = differential(&["b0_proc_seed"], "b0_proc_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_proc_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "a fresh process inherits nothing");

    for view in [
        "no subprocess from a predecessor is live in this entry",
        "this entry's own subprocess takes the first registry slot",
    ] {
        assert!(
            worker.expects_pass.iter().any(|m| m.message.contains(view)),
            "the `{view}` view did not fire, so only one of the two independent \
             observations is evidence: {:?}",
            worker.expects_pass
        );
    }
    // Both runs have to have taken the measurement and found it empty --
    // otherwise the agreement is between two probes that never looked.
    assert_eq!(
        isolated.artifacts["procs.txt"].len, 2,
        "the isolated live-handle set must be the empty `[]`, not unwritten"
    );
    assert_eq!(isolated.artifacts["procs.txt"], worker.artifacts["procs.txt"]);
}

/// A non-default frame budget is per ENTRY, not per process: the budget an
/// entry runs on is the one ITS sim declared, and it is measured from that
/// entry's start frame.
///
/// The two fixtures declare DIFFERENT budgets on purpose -- a queue that served
/// only one value would fail whichever it dropped -- and the predecessor burns
/// most of its own before the probe starts, so a process-wide bound would kill
/// the probe partway and cost it its later expects and its `stop_sim()`.
#[test]
fn a_declared_frame_budget_belongs_to_its_entry() {
    let (isolated, worker) = differential(&["b0_budget_pred"], "b0_budget_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_budget_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(record.saw_stop, "{label}: the entry was cut short of stop_sim()");
        assert!(
            record
                .expects_pass
                .iter()
                .any(|m| m.message.contains("kept its own budget through to the end")),
            "{label}: the probe never reached its post-budget frames, so agreement \
             proves nothing: {:?}",
            record.expects_pass
        );
    }
    // The declared value, not just "a budget": 90 is the probe's, 40 the
    // predecessor's, 600 the default nobody here asked for.
    assert_eq!(
        worker.artifacts["budget.txt"].len,
        2,
        "the entry reports a two-digit budget, so neither 600 nor an empty read"
    );
    assert_eq!(isolated.artifacts["budget.txt"], worker.artifacts["budget.txt"]);
    assert!(c.population("expects_pass") > 0);
}

/// Boot-signature admission, side one: entries that AGREE on a non-default
/// boot value share one worker and the target behaves as it does alone.
///
/// The value is `SIM_SKIP_RENDER`, latched once by `runner_load_sim_entries`
/// before the queue starts, so nothing per-entry could stand in for it -- and
/// the probe asserts the latch actually moved off its default, so agreement is
/// not two runs of an unread variable.
#[test]
fn a_compatible_boot_signature_reaches_the_shared_worker() {
    let (isolated, worker) = differential(&["b0_env_pred"], "b0_env_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_env_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            record
                .expects_pass
                .iter()
                .any(|m| m.message.contains("boot latched it, away from its default")),
            "{label}: the boot value never took effect, so this compared nothing: {:?}",
            record.expects_pass
        );
    }
    assert_eq!(worker.artifacts["env.txt"].len, 1, "the entry read the declared `0`");
    assert_eq!(isolated.artifacts["env.txt"], worker.artifacts["env.txt"]);
    assert!(c.population("artifacts") > 0);
}

/// Boot-signature admission, side two: an entry that DISAGREES on a
/// boot-consumed value is refused, naming both sims and the reason -- never
/// run in a process booted with the other value.
#[test]
fn an_incompatible_boot_signature_is_refused_with_its_reason() {
    let plan = DifferentialPlan {
        predecessors: vec!["b0_env_conflict".to_string()],
        target: "b0_env_probe".to_string(),
    };
    let err = run_differential(Backend::Skull, "testbed", &plan)
        .expect_err("a worker must not serve two different boot signatures");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("boot signatures") && msg.contains("b0_env_conflict") && msg.contains("b0_env_probe"),
        "the refusal must say which sims and why, got: {msg}"
    );
}

/// The other admission arm, with its own distinct reason: `$OUT` in a value is
/// resolved before the process starts, so it cannot follow a per-entry `$OUT`.
/// These two entries AGREE on their signature, so this is not the check above
/// firing under another name.
#[test]
fn an_out_interpolating_env_is_refused_with_its_own_reason() {
    let plan = DifferentialPlan {
        predecessors: vec!["b0_env_outref_head".to_string()],
        target: "b0_env_outref_tail".to_string(),
    };
    let err = run_differential(Backend::Skull, "testbed", &plan)
        .expect_err("a worker cannot give one process two different $OUT-derived values");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("B0_ENV_SESSION") && msg.contains("$OUT"),
        "the refusal must name the offending pair and the reason, got: {msg}"
    );
    assert!(
        !msg.contains("boot signatures"),
        "these entries agree on their signature; a signature refusal here would mean \
         the fixture never reached the $OUT check: {msg}"
    );
}

/// The DROPPED-DEFINITION static (`docs/sim-speed-plan.md` B1). Its
/// `Iterator`-bounded owner has no standalone emission, so the cell exists only
/// inlined at call sites; the B1 registration fix carries a spliced callee's
/// statics into the body they splice into, so the cell reaches
/// `__ROCK_STATICS` and the checkpoint restores it like any other. Pinned at
/// the emitted-Lua level too, by
/// `crates/skull/tests/static_registration_coverage.rs`.
#[test]
fn a_dropped_definitions_static_resets_at_the_entry_boundary() {
    let (isolated, worker) = differential(&["b1_dropped_seed"], "b1_dropped_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b1_dropped_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(
        worker.artifacts["calls.txt"].len, 1,
        "the probe writes its one-digit count, so the fresh cell was measured"
    );
    assert_eq!(isolated.artifacts["calls.txt"], worker.artifacts["calls.txt"]);
}

/// A predecessor the entry budget CUT -- its coroutine still suspended, a
/// global dirtied -- is abandoned and restored over like any finished one. The
/// wedge only wedges when chained behind its marker-writing head, so the suite
/// sees both fixtures pass standalone.
#[test]
fn a_budget_cut_predecessor_still_restores_boot() {
    let (isolated, worker) = differential(&["b2_wedge_head,b2_wedge_tail"], "b0_leak_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_leak_probe after a wedged predecessor", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
    assert_eq!(
        worker.artifacts["count.txt"].len, 1,
        "the probe wrote the counter it read, so the fresh zero was measured"
    );
    assert_eq!(isolated.artifacts["count.txt"], worker.artifacts["count.txt"]);
}

/// Failure containment (`docs/sim-speed-plan.md` B2): a worker that DIES
/// mid-queue loses only the entry it died inside; every unstarted entry reruns
/// in a fresh worker of the same boot signature and behaves as it does alone.
/// The crash only fires when chained behind its marker-writing head, so the
/// suite sees both fixtures pass standalone.
#[test]
fn a_crashed_worker_recovers_its_unstarted_entries() {
    let records = run_worker_suite(
        Backend::Skull,
        "testbed",
        &["b2_crash_head,b2_crash_tail".to_string(), "b0_leak_probe".to_string()],
    )
    .expect("the suite call itself must not error: the crash is an entry failure");
    assert_eq!(records.len(), 2, "one record per entry spec");
    let records: Vec<SimRecord> = records.into_iter().map(|(r, _)| r).collect();

    let crashed = &records[0];
    assert_eq!(
        crashed.exit_code,
        Some(7),
        "the crash belongs to the entry the worker died inside: {crashed:?}"
    );
    assert_eq!(crashed.verdict(), "FAIL");

    let probe = &records[1];
    assert!(
        probe.errors.is_empty(),
        "recovery must rerun the unstarted entry, not record it as never started: {:?}",
        probe.errors
    );
    assert_eq!(probe.verdict(), "PASS", "the recovered entry behaves as it does alone");
    assert!(probe.saw_stop);
    assert!(
        probe
            .expects_pass
            .iter()
            .any(|m| m.message.contains("a counter no predecessor touched")),
        "the recovered entry must have run on a fresh world: {:?}",
        probe.expects_pass
    );
    assert_eq!(
        probe.artifacts["count.txt"].len, 1,
        "the recovered entry produced its artifact under its own $OUT"
    );
}

/// An entry that REPLACES the boot checkpoint mid-run (the checkpoint-
/// machinery sims do this by design) must not have its mid-entry image
/// silently restored into the next entry -- that was observed as a corruption
/// cascade across every later entry in the queue. The boundary refuses with
/// its reason and the worker dies there; recovery then reruns the unstarted
/// entries fresh, so the damage is contained to the entry that caused it.
#[test]
fn a_replaced_boot_checkpoint_is_refused_not_restored() {
    // Both sims carry the SIM_WORKER_ISOLATED marker, so worker-mode GROUPING
    // never queues them -- this drives the raw queue surface underneath it,
    // which is exactly where an unforeseen mid-entry take would land.
    let records = run_worker_suite(
        Backend::Skull,
        "testbed",
        &["b1_checkpoint_take_succeeds".to_string(), "b1_checkpoint_membership".to_string()],
    )
    .expect("the suite call itself must not error: the refusal is an entry failure");
    assert_eq!(records.len(), 2);
    let records: Vec<SimRecord> = records.into_iter().map(|(r, _)| r).collect();

    let corruptor = &records[0];
    assert!(
        corruptor
            .errors
            .iter()
            .any(|m| m.message.contains("boot checkpoint was replaced")),
        "the boundary must name the refusal, not restore a mid-entry image: {:?}",
        corruptor.errors
    );
    assert_eq!(corruptor.verdict(), "FAIL");

    let probe = &records[1];
    assert!(probe.errors.is_empty(), "recovery reruns the probe fresh: {:?}", probe.errors);
    assert_eq!(
        probe.verdict(),
        "PASS",
        "the recovered entry behaves as it does alone (its own machinery asserts \
         hold only in a fresh process): {probe:?}"
    );
}

/// A native resource an entry CREATED does not reach its successor
/// (`docs/sim-speed-plan.md` B3). The Lua checkpoint cannot see a GL texture or
/// an FBO, so the boundary tears the native world back down to the exact
/// handle set boot published -- and the fixture reads that set two independent
/// ways, the ledger's own diff and the monotonic id its own canvas is handed.
#[test]
fn a_predecessors_native_resource_does_not_reach_the_next_entry() {
    let (isolated, worker) = differential(&["b3_leak_seed"], "b3_leak_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b3_leak_probe", &c);
    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
}

/// `stop_sim`'s three implicit invariants are process-lifetime state -- two
/// growing atomics and a set of sticky per-block marks -- so in a worker they
/// charged an entry with its predecessors' failures. A fresh process charges an
/// entry with boot's failures plus its own, and that is what the worker must
/// charge too.
///
/// The injection is real: `stone_create_shader` on a source that cannot compile
/// drives the same reporting path a broken game shader does. It is chained-only
/// so `rock sim testbed` stays green -- the same escape the `expects_fail`
/// denominator uses.
#[test]
fn a_predecessors_shader_failure_is_not_charged_to_the_next_entry() {
    let (isolated, worker) =
        differential(&["b3_diag_head,b3_diag_injector"], "b3_diag_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b3_diag_probe after an injected shader failure", &c);
    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert_eq!(worker.verdict(), "PASS", "and behind a predecessor that failed a compile");
    assert!(c.population("expects_pass") > 0, "the fixture asserted nothing");
}

/// The other direction, which no teardown can fix: an entry that DESTROYED a
/// boot-owned native resource leaves the restored globals naming a handle whose
/// GL object is gone, and the first draw through one is a GL error. The
/// boundary names what is missing and ends the worker -- but the entry itself
/// ran to completion before that was knowable, so its record is kept (it
/// followed a predecessor, which is what makes it evidence) and only its
/// unstarted successors restart in a fresh worker.
#[test]
fn an_entry_that_destroyed_boots_native_world_keeps_its_record_and_ends_its_worker() {
    // The head is not decoration. As entry ZERO the seed would have followed no
    // predecessor, so its record could not be boundary evidence whatever the
    // boundary did, and the provenance assertion below would pass vacuously.
    let records = run_worker_suite(
        Backend::Skull,
        "testbed",
        &[
            "b3_canvas_head".to_string(),
            "b3_canvas_seed".to_string(),
            "b3_canvas_probe".to_string(),
        ],
    )
    .expect("the suite call itself must not error: ineligibility is a scheduling fact");
    assert_eq!(records.len(), 3, "one record per entry spec");

    let (head, head_crossed) = &records[0];
    assert_eq!(head.verdict(), "PASS", "the head runs normally: {head:?}");
    assert!(!head_crossed, "entry zero follows nothing");

    let (seed, seed_crossed) = &records[1];
    assert_eq!(
        seed.verdict(),
        "PASS",
        "ending the worker is a scheduling fact, not the entry's failure: {seed:?}"
    );
    assert!(
        seed_crossed,
        "the entry FOLLOWED a predecessor and its record is kept, so it stays \
         boundary evidence rather than being thrown away for a fresh-process one"
    );
    assert_eq!(
        seed.artifacts["runs.txt"].len, 1,
        "the entry ran EXACTLY ONCE: it completed before the boundary could know, \
         so rerunning it would repeat its side effects"
    );

    let (probe, probe_crossed) = &records[2];
    assert!(
        !probe_crossed,
        "the successor restarts in a FRESH worker, so it follows nothing and \
         cannot certify a restore"
    );
    assert_eq!(
        probe.verdict(),
        "PASS",
        "the successor renders through boot's canvases, not through destroyed ones: {probe:?}"
    );
    assert!(
        !probe
            .expects_fail
            .iter()
            .any(|m| m.message.contains("GL errors")),
        "the worker-only GL error class is what this boundary closes: {:?}",
        probe.expects_fail
    );
}

/// Mutation guard for the comparator itself: every arm must be present, and
/// every arm a fixture can populate must have compared a non-zero population
/// in at least one fixture. Deleting an arm from `compare` makes `population`
/// panic here; leaving an arm in but never exercising it fails the denominator
/// assert.
#[test]
fn every_comparator_arm_is_present_and_exercised() {
    // No single fixture reaches every arm: only the dedup one logs inside its
    // entry envelope, and with the B2 boundary landed nothing fails in a worker
    // that would not fail isolated -- `expects_fail` is populated by the
    // deliberate chained-only failure, identical on both sides. The denominator
    // is the union.
    let comparisons = [
        {
            let (i, w) = differential(&["b0_leak_seed"], "b0_leak_probe");
            compare(&i, &w)
        },
        {
            let (i, w) = differential(&["b0_dedup_seed"], "b0_dedup_probe");
            compare(&i, &w)
        },
        {
            let (i, w) = differential(&["b0_leak_seed"], "b0_fail_head,b0_fail_tail");
            let c = compare(&i, &w);
            // The tail has a standalone branch; population is only evidence
            // once both sides are known to have taken the FAILING one.
            for (label, record) in [("isolated", &i), ("worker", &w)] {
                assert!(
                    record
                        .expects_fail
                        .iter()
                        .any(|m| m.message.contains("deliberate chained-only failure")),
                    "{label}: the tail took its standalone branch, so `expects_fail` \
                     was never populated deliberately: {:?}",
                    record.expects_fail
                );
            }
            c
        },
    ];

    for arm in COMPARED_ARMS {
        let population: usize = comparisons.iter().map(|c| c.population(arm)).max().unwrap_or(0);
        if ARMS_WITHOUT_A_FIXTURE.contains(arm) {
            assert_eq!(population, 0, "`{arm}` is documented as having no fixture");
            continue;
        }
        assert!(population > 0, "arm `{arm}` compared an empty population in every fixture");
    }
    let (isolated, worker) = differential(&["b0_leak_seed"], "b0_leak_probe");
    assert!(
        isolated.errors.is_empty() && worker.errors.is_empty(),
        "the zero `errors` denominator has to be a real zero, not an unread field"
    );
}

/// `--workers` scheduling is POSITIONAL: naming one sim twice is two entries,
/// both of which must run and both of which must land a result. Keying results
/// by spec spelling ran both and then reported the second as never scheduled.
/// Driven through the CLI because `run_specs_worker_mode` is the layer that
/// places results, and the tally line is what a user reads.
#[test]
fn a_duplicated_spec_runs_and_reports_twice_in_worker_mode() {
    let mut rock = std::env::current_exe().expect("test exe");
    rock.pop();
    if rock.ends_with("deps") {
        rock.pop();
    }
    let rock = rock.join("rock");
    assert!(rock.exists(), "rock binary not built at {}", rock.display());

    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();

    let out = std::process::Command::new(&rock)
        .current_dir(&root)
        .args(["sim", "testbed", "b0_leak_probe", "b0_leak_probe", "--workers"])
        .output()
        .expect("run rock sim --workers");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "a duplicated spec must not fail the invocation:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("[sim] 2/2 sims passed"),
        "both occurrences must be accounted for:\n{stdout}"
    );
    // Per-entry records are the command's result on stderr; the tally is stdout.
    assert_eq!(
        stderr.matches("b0_leak_probe: PASS").count(),
        2,
        "each occurrence owns its own record:\n{stderr}"
    );
}

/// PROVENANCE, which is what the `--multi-order` gate's denominator rests on:
/// only an entry that actually followed another one inside one process can be
/// evidence that a restore boundary works. An entry that ran first had no
/// predecessor's world to inherit, so its agreement with a fresh process is
/// agreement between two fresh processes.
///
/// Both classifications are pinned, because getting either wrong silently
/// changes what the gate certifies: a healthy queue's successors must count,
/// and an entry that RECOVERED into a fresh worker must not -- recovery is
/// exactly the case where queue position lies about what happened.
#[test]
fn only_an_entry_that_followed_a_predecessor_counts_as_evidence() {
    let healthy = run_worker_suite(
        Backend::Skull,
        "testbed",
        &["b0_leak_seed".to_string(), "b0_leak_probe".to_string(), "b0_rng_probe".to_string()],
    )
    .expect("the healthy queue must run");
    assert_eq!(
        healthy.iter().map(|(_, crossed)| *crossed).collect::<Vec<_>>(),
        vec![false, true, true],
        "in one healthy worker only the first entry lacks a predecessor"
    );
    for (record, _) in &healthy {
        assert_eq!(record.verdict(), "PASS", "the queue must be healthy: {record:?}");
    }

    // The crash kills the worker inside entry 0, so `b0_leak_probe` never
    // started and is rerun in a FRESH worker -- where it is first, with no
    // predecessor, however late its queue position is.
    let recovered = run_worker_suite(
        Backend::Skull,
        "testbed",
        &["b2_crash_head,b2_crash_tail".to_string(), "b0_leak_probe".to_string()],
    )
    .expect("the suite call itself must not error: the crash is an entry failure");
    assert_eq!(
        recovered.iter().map(|(_, crossed)| *crossed).collect::<Vec<_>>(),
        vec![false, false],
        "the recovered entry restarted a worker, so it followed nobody -- position \
         alone would have called it a successor"
    );
    assert_eq!(
        recovered[1].0.verdict(),
        "PASS",
        "the recovered entry still ran; it is just not evidence about a boundary"
    );
}

/// The wall-clock timeout is the WEDGED-CHILD guard, so it bounds an entry,
/// not the worker process: a queue of individually healthy entries must not
/// fail whichever one happens to be running when their sum crosses it.
#[test]
fn the_wall_clock_deadline_is_per_entry_not_per_worker() {
    const VAR: &str = "ROCK_SIM_TIMEOUT_SECS";
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the heavy harness runs single-threaded (--test-threads=1).
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var(VAR, v),
                    None => std::env::remove_var(VAR),
                }
            }
        }
    }
    let _restore = Restore(std::env::var(VAR).ok());
    // Each entry sleeps 4s; three of them plus boot cannot fit in 12s, while
    // every single entry fits with room to spare.
    unsafe { std::env::set_var(VAR, "12") };

    let records = run_worker_suite(Backend::Skull, "testbed", &vec!["b2_slow_entry".to_string(); 3])
        .expect("the suite call itself must not error");
    assert_eq!(records.len(), 3);
    let records: Vec<SimRecord> = records.into_iter().map(|(r, _)| r).collect();
    for (i, record) in records.iter().enumerate() {
        assert!(
            !record.timed_out,
            "entry {i} was inside its own deadline: {record:?}"
        );
        assert_eq!(record.verdict(), "PASS", "entry {i}: {record:?}");
    }
}
