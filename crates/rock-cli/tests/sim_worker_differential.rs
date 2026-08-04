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

/// SENTINEL -- and the reason the `entry_diagnostics` arm exists. `log_once`
/// dedups per call site in a process-global table, so a predecessor that logged
/// through the same helper SUPPRESSES the probe's warning entirely. No other
/// arm can see this: the sim passes, writes nothing, and exits cleanly.
///
/// FLIP WHEN FIXED: becomes `assert_equivalent` when the boundary clears logger
/// dedup/throttle state.
#[test]
fn logger_dedup_still_suppresses_the_next_entrys_warning() {
    let (isolated, worker) = differential(&["b0_dedup_seed"], "b0_dedup_probe");
    let c = compare(&isolated, &worker);

    assert!(
        c.differs_on("entry_diagnostics"),
        "the worker no longer suppresses a repeated warning -- flip this sentinel:\n{}",
        c.report()
    );
    assert!(
        isolated.entry_diagnostics.contains("b#") || isolated.entry_diagnostics.contains("dedup"),
        "the isolated run must actually emit the warning: {:?}",
        isolated.entry_diagnostics
    );
    assert_eq!(
        isolated.verdict(),
        worker.verdict(),
        "a suppressed warning changes no verdict -- that is what makes it invisible elsewhere"
    );
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

/// SENTINEL -- a native resource. Subprocesses are the one native class the
/// runtime exposes an honest observer for: handles are 1-based registry slots
/// (`stone-rs/src/proc.rs`) and `stone_proc_alive` answers per handle, so the
/// fixture compares handle SETS. A count could not see this at all -- the
/// registry reuses a freed slot, so the freed and the leaked case have the same
/// count.
///
/// Both views must move: the probe finds a live handle it never spawned, AND
/// its own spawn is pushed off the first slot the predecessor is still holding.
///
/// FLIP WHEN FIXED: becomes `assert_equivalent` when the boundary tears down
/// per-entry native resources (`docs/sim-speed-plan.md` B3).
#[test]
fn a_subprocess_still_outlives_the_entry_that_spawned_it() {
    let (isolated, worker) = differential(&["b0_proc_seed"], "b0_proc_probe");
    let c = compare(&isolated, &worker);

    assert!(
        !c.is_equivalent(),
        "the worker no longer inherits a live subprocess -- land the teardown \
         and flip this sentinel to assert_equivalent"
    );
    for arm in ["verdict", "expects_pass", "expects_fail", "artifacts"] {
        assert!(c.differs_on(arm), "expected a `{arm}` difference:\n{}", c.report());
    }
    assert_eq!(isolated.verdict(), "PASS", "a fresh process inherits nothing");
    assert_eq!(worker.verdict(), "FAIL");

    for view in [
        "no subprocess from a predecessor is live in this entry",
        "this entry's own subprocess takes the first registry slot",
    ] {
        assert!(
            worker.expects_fail.iter().any(|m| m.message.contains(view)),
            "the `{view}` view did not fire, so only one of the two independent \
             observations is evidence: {:?}",
            worker.expects_fail
        );
    }
    // The isolated run has to have taken the same measurement and found it
    // empty -- otherwise the probe never looked.
    assert_eq!(
        isolated.artifacts["procs.txt"].len, 2,
        "the isolated live-handle set must be the empty `[]`, not unwritten"
    );
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
