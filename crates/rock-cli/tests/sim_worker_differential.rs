//! The Phase B differential oracle (`docs/sim-speed-plan.md` B0).
//!
//! A boot-checkpoint worker is only allowed to become the default when a sim
//! that ran after a predecessor in ONE process is observationally identical to
//! the same sim in a fresh process. This is the gate that decides that, and it
//! exists BEFORE the reset it will eventually certify: the leak fixture below
//! is red today and pins exactly how far the seam currently is from correct.
//!
//! Both sides of every comparison are produced by the same spawn/demux/record
//! code with the same schema, differing only in how many entries share the
//! process. Comparison is over structured records -- never over the runner's
//! colored terminal output.
//!
//! Each fixture owns ONE state class and lives in its own
//! `games/testbed/tests/sims/b0_*.rock` file, so a failure in one cannot mask
//! the next.

use rock_cli::run_sim::{run_differential, DifferentialPlan};
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

/// SENTINEL -- asserts the CURRENTLY BROKEN behavior. The worker seam runs
/// entries back to back with no state reset (`docs/sim-speed-plan.md` B1/B3 are
/// unimplemented), so a game global set by one entry survives into the next.
///
/// FLIP WHEN FIXED: once the boot-checkpoint restore lands, this must become
/// `assert_equivalent`, in the same commit as the fix.
#[test]
fn a_leaked_global_still_reaches_the_next_entry() {
    let (isolated, worker) = differential(&["b0_leak_seed"], "b0_leak_probe");
    let c = compare(&isolated, &worker);

    assert!(
        !c.is_equivalent(),
        "the worker no longer leaks a game global -- land the reset and flip this \
         sentinel to assert_equivalent"
    );
    // Named arms, not just "some difference": the leak has to be visible both
    // as an assertion outcome and in a produced file's CONTENTS, because a
    // reset that only fixed one of those would still be wrong.
    for arm in ["verdict", "expects_pass", "expects_fail", "artifacts"] {
        assert!(c.differs_on(arm), "expected a `{arm}` difference:\n{}", c.report());
    }

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert_eq!(worker.verdict(), "FAIL", "and fails after a predecessor");
    assert_eq!(
        worker.artifacts["count.txt"].len, 1,
        "the counter is written as one digit either way"
    );
    assert_ne!(
        isolated.artifacts["count.txt"], worker.artifacts["count.txt"],
        "same-length file, different contents -- a name-only manifest would miss this"
    );
}

/// SENTINEL -- the RNG stream position is process-global, so a predecessor's
/// draws move the probe's. Both entries still PASS their own assertions: the
/// leak reaches the record only through the produced file's contents, which is
/// why the manifest hashes bytes rather than listing names.
///
/// FLIP WHEN FIXED: becomes `assert_equivalent` when the checkpoint restores
/// RNG state.
#[test]
fn the_rng_stream_position_still_leaks_into_the_next_entry() {
    let (isolated, worker) = differential(&["b0_rng_seed"], "b0_rng_probe");
    let c = compare(&isolated, &worker);

    assert!(
        c.differs_on("artifacts"),
        "the worker no longer leaks RNG position -- flip this sentinel:\n{}",
        c.report()
    );
    assert_eq!(
        isolated.verdict(),
        worker.verdict(),
        "both sides pass their own expects; only the drawn value moved"
    );
    assert_ne!(isolated.artifacts["draw.txt"], worker.artifacts["draw.txt"]);
}

/// SENTINEL -- a `static` local in an ordinarily-called function. Skull records
/// this one's cell and done flag in `__rock_known_globals`, so it is the case a
/// checkpoint registry can already see.
///
/// FLIP WHEN FIXED: becomes `assert_equivalent` when statics are restored.
#[test]
fn a_called_function_static_still_leaks_into_the_next_entry() {
    let (isolated, worker) = differential(&["b0_static_called_seed"], "b0_static_called_probe");
    let c = compare(&isolated, &worker);

    assert!(!c.is_equivalent(), "the called-function static no longer leaks -- flip this sentinel");
    for arm in ["verdict", "expects_fail", "artifacts"] {
        assert!(c.differs_on(arm), "expected a `{arm}` difference:\n{}", c.report());
    }
}

/// SENTINEL -- the same state class reached through INLINED call sites, where
/// the cell is read and written directly in the caller's body rather than
/// through a call. Kept separate from its called-function sibling because a
/// registry built from emitted bodies treats the two differently.
///
/// FLIP WHEN FIXED: becomes `assert_equivalent` when statics are restored.
#[test]
fn an_inlined_static_still_leaks_into_the_next_entry() {
    let (isolated, worker) = differential(&["b0_static_inlined_seed"], "b0_static_inlined_probe");
    let c = compare(&isolated, &worker);

    assert!(!c.is_equivalent(), "the inlined static no longer leaks -- flip this sentinel");
    for arm in ["verdict", "expects_fail", "artifacts"] {
        assert!(c.differs_on(arm), "expected a `{arm}` difference:\n{}", c.report());
    }
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

/// Mutation guard for the comparator itself: every arm must be present, and
/// every arm a fixture can populate must have compared a non-zero population
/// in at least one fixture. Deleting an arm from `compare` makes `population`
/// panic here; leaving an arm in but never exercising it fails the denominator
/// assert.
#[test]
fn every_comparator_arm_is_present_and_exercised() {
    // No single fixture reaches every arm: the leak differential is the only
    // one populating `expects_fail`, and only the dedup one logs inside its
    // entry envelope. The denominator is the union.
    let comparisons = [
        {
            let (i, w) = differential(&["b0_leak_seed"], "b0_leak_probe");
            compare(&i, &w)
        },
        {
            let (i, w) = differential(&["b0_dedup_seed"], "b0_dedup_probe");
            compare(&i, &w)
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
