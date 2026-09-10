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

/// Run the real `rock` binary from the workspace root and return its stderr.
/// `hostile_parent` puts `ROCK_MASK_WALL_CLOCK=1` in the PARENT environment,
/// which is the route an inherited value takes into a child the runner spawns.
fn run_rock(args: &[&str], hostile_parent: bool) -> String {
    let parent_env: &[(&str, &str)] =
        if hostile_parent { &[("ROCK_MASK_WALL_CLOCK", "1")] } else { &[] };
    run_rock_with_env(args, parent_env)
}

/// Same run, with an arbitrary PARENT environment -- the route an inherited
/// value takes into a child the runner spawns. `run_rock` is this with the
/// wall-clock mask, kept as its own spelling because most callers only ever
/// want that one.
fn run_rock_with_env(args: &[&str], parent_env: &[(&str, &str)]) -> String {
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

    let mut cmd = std::process::Command::new(&rock);
    cmd.current_dir(&root).args(args).envs(parent_env.iter().copied());
    let out = cmd.output().unwrap_or_else(|e| panic!("run rock {args:?}: {e}"));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{args:?} failed:\n{stderr}");
    stderr
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

#[test]
fn a_predecessors_live_scene_does_not_reach_the_next_entry() {
    let (isolated, worker) = differential(&["b0_scene_seed"], "b0_scene_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_scene_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert_eq!(record.verdict(), "PASS", "{label} Scene probe failed: {record:?}");
        assert!(
            record
                .expects_pass
                .iter()
                .any(|m| m.message.contains("empty boot Scene registry")),
            "{label}: the Scene registry assertion never ran: {:?}",
            record.expects_pass
        );
    }
}

#[test]
fn state_image_worker_registries_stay_bounded_across_repeated_entries() {
    let entries = vec!["t1a_worker_registries_stay_bounded".to_string(); 12];
    let records = run_worker_suite(Backend::Skull, "testbed", &entries)
        .expect("the repeated StateImage worker queue must run");
    assert_eq!(records.len(), entries.len(), "the worker returned one record per entry");
    let crossed = records.iter().filter(|(_, crossed)| *crossed).count();
    assert_eq!(crossed, 11, "every entry after the first must cross one worker boundary");
    assert!(crossed > 0, "the bounded-registry assertion needs a nonzero boundary population");
    for (record, _) in records {
        assert_eq!(record.verdict(), "PASS", "worker registry fixture failed: {record:?}");
        assert!(record.errors.is_empty(), "worker registry fixture emitted errors: {record:?}");
        assert_eq!(record.expects_pass.len(), 2, "both registry populations were asserted");
    }
}

#[test]
fn repeated_bootimage_entries_prove_immutable_owner_conservation() {
    let entries = vec!["t1a_worker_registries_stay_bounded".to_string(); 8];
    let records = run_worker_suite(Backend::Skull, "testbed", &entries)
        .expect("the repeated BootImage ownership queue must run");
    assert_eq!(records.len(), entries.len(), "the worker returned one record per entry");
    let crossed = records.iter().filter(|(_, crossed)| *crossed).count();
    assert_eq!(crossed, 7, "every successor must actually follow a predecessor");
    assert!(crossed > 0, "the conservation arm needs a nonzero boundary population");
    for (record, _) in records {
        assert_eq!(record.verdict(), "PASS", "BootImage ownership fixture failed: {record:?}");
        assert!(
            record.expects_pass.iter().any(|m| m.message.contains("BootImage owner conservation")),
            "the fixture must assert immutable-image ownership and fresh-session ownership, not just finish: {record:?}"
        );
    }
}

/// A global that was NULL at boot is null again at the next entry.
///
/// Its own class, not a variant of `b0_leak_probe`: the image is keyed by
/// member name and a nil value has no key, so a restore driven by the image
/// cannot tell "boot had nil" from "not a member" -- while the frozen member
/// set records it either way. A global with any other boot value has a key and
/// is restored, which is why every existing fixture passed over this.
///
/// Found through mag's `reagents` (`docs/sim-speed-plan.md` B4): it asserts
/// `reagent_context_spell() == null` and failed only after `spell_rows_focus`,
/// which leaves the `SpellKind?` global `REG_HOVER_SPELL` set.
#[test]
fn a_global_that_was_null_at_boot_is_null_again_at_the_next_entry() {
    let (isolated, worker) = differential(&["b0_null_global_seed"], "b0_null_global_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_null_global_probe", &c);

    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process");
    assert_eq!(worker.verdict(), "PASS", "and after a predecessor that set the global");
    // Agreement over an unmade assertion would be agreement over nothing.
    // (Membership is not asserted in the sim: a one-entry queue takes no
    // checkpoint at all, so checkpoint membership is empty in the isolated arm
    // by design and would diverge for a reason unrelated to the class.)
    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            record
                .expects_pass
                .iter()
                .any(|m| m.message.contains("null again at the next entry")),
            "{label}: the fixture never asserted the property: {:?}",
            record.expects_pass
        );
    }
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
/// host/diagnostic global outside the compiler StateSchema, so the census never reaches it and a
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

/// A record whose content is a wall-clock measurement is DECLARED as one at
/// its log site, and the arm drops it by that declaration -- never by finding
/// an `ms` in the text, which would make `cooldown 100ms` compare equal to
/// `cooldown 200ms`.
///
/// The probe reads the engine clock, which the boundary does not restore, so
/// all three of its records carry a number that really does differ between a
/// fresh process and a worker's second entry. Equivalence here is therefore
/// evidence about the declarations and not about a fixture with nothing to
/// disagree on -- confirmed by mutation: dropping any one declaration makes
/// this red.
#[test]
fn a_declared_wall_clock_record_does_not_diverge_but_its_neighbours_still_compare() {
    let (isolated, worker) = differential(&["b0_wall_clock_seed"], "b0_wall_clock_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_wall_clock_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        // Both declared forms must have FIRED -- agreement over a record that
        // was never emitted is agreement over nothing -- and must be absent
        // from the compared text.
        for declared in ["b0 wall-clock record at", "b0 wall-clock throttled record at"] {
            assert!(
                record.diagnostics.contains(declared),
                "{label}: `{declared}` never fired, so its exclusion proves nothing: {:?}",
                record.diagnostics
            );
            assert!(
                !record.entry_diagnostics.contains(declared),
                "{label}: `{declared}` is declared wall-clock and must not be compared: {:?}",
                record.entry_diagnostics
            );
        }

        // The mixed record survives whole, minus the one declared value.
        assert!(
            record.entry_diagnostics.contains("b0 mixed record: cooldown 100ms, elapsed <ms>"),
            "{label}: the undeclared duration and the masked one must share the line: {:?}",
            record.entry_diagnostics
        );
    }
    assert!(
        c.population("entry_diagnostics") > 0,
        "the arm compared nothing, so the surviving record was never checked"
    );
}

/// The value declaration is scoped to runs whose records are COMPARED. An
/// ordinary `rock sim` run keeps the real measurement -- masking it would turn
/// the editor's timing logs into noise and would change what a sim's own
/// artifacts contain, for a comparison nobody is making.
///
/// Both modes are asserted here because either alone is half a control: the
/// differential test above proves the mask is on where records are compared,
/// and this proves it is off where they are not.
#[test]
fn the_value_mask_is_on_only_where_records_are_compared() {
    for mode in [vec!["sim", "testbed", "b0_wall_clock_probe"],
                 vec!["sim", "testbed", "b0_wall_clock_seed", "b0_wall_clock_probe", "--workers"]] {
        let stderr = run_rock(&mode, false);

        let mixed = stderr
            .lines()
            .find(|l| l.contains("b0 mixed record:"))
            .unwrap_or_else(|| panic!("{mode:?} emitted no mixed record:\n{stderr}"));
        assert!(
            !mixed.contains("<ms>"),
            "{mode:?} is not a compared run, so it must keep the measurement: {mixed}"
        );
        assert!(
            mixed.contains("cooldown 100ms, elapsed ") && mixed.ends_with("ms"),
            "{mode:?} must render a real duration: {mixed}"
        );
    }
}

/// `ROCK_MASK_WALL_CLOCK` is the HARNESS's variable, and a compared run must
/// stay masked however loudly a fixture asks for real numbers. The value is
/// latched into a `static` on first read, so an entry-dependent override could
/// not be honoured coherently even if it were wanted -- the first entry to read
/// it would decide for the whole worker.
///
/// The declaring pair is the mutation subject: with the runner publishing the
/// mask before a sim's `@sim_env`, this fixture's `"0"` wins and both sides
/// render a real duration the boundary does not restore, so the records
/// diverge.
#[test]
fn a_fixture_cannot_unmask_a_compared_run() {
    let (isolated, worker) = differential(&["b0_wall_clock_unmask_seed"], "b0_wall_clock_unmask_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_wall_clock_unmask_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        assert!(
            record.entry_diagnostics.contains("b0 mixed record: cooldown 100ms, elapsed <ms>"),
            "{label}: the fixture's ROCK_MASK_WALL_CLOCK=0 must not reach a compared run: {:?}",
            record.entry_diagnostics
        );
    }
    assert!(
        c.population("entry_diagnostics") > 0,
        "the arm compared nothing, so the masked record was never checked"
    );
}

/// The other direction, and the one an inherited environment can reach: an
/// ordinary run keeps its measurements even when the PARENT environment or the
/// fixture itself sets the mask. Neither is allowed to speak for the runner --
/// masking an ordinary run would turn the editor's timing logs into noise and
/// would change what a sim's own artifacts contain.
///
/// The two hostile sources get a test each rather than two arms of one, because
/// they enter by different routes -- the fixture's value through `preset_env`,
/// the parent's through `Command`'s inherited environment -- and a single test
/// would stop at whichever failed first, hiding the other.
///
/// Both ordinary modes are checked: an isolated run and a `--workers` run whose
/// records nobody compares.
fn assert_ordinary_run_keeps_its_measurements(seed: &str, probe: &str, hostile_parent: bool) {
    for workers in [false, true] {
        // An isolated run needs only the probe; a worker run needs the
        // predecessor that puts it behind a boundary.
        let args = if workers {
            vec!["sim", "testbed", seed, probe, "--workers"]
        } else {
            vec!["sim", "testbed", probe]
        };

        let stderr = run_rock(&args, hostile_parent);
        let mixed = stderr
            .lines()
            .find(|l| l.contains("b0 mixed record:"))
            .unwrap_or_else(|| panic!("{args:?} emitted no mixed record:\n{stderr}"));
        assert!(
            !mixed.contains("<ms>"),
            "{args:?} (hostile_parent={hostile_parent}) is not a compared run, \
             so it must keep the measurement: {mixed}"
        );
        assert!(
            mixed.contains("cooldown 100ms, elapsed ") && mixed.ends_with("ms"),
            "{args:?} (hostile_parent={hostile_parent}) must render a real duration: {mixed}"
        );
    }
}

#[test]
fn an_ordinary_run_keeps_its_measurements_when_the_fixture_asks_to_mask() {
    assert_ordinary_run_keeps_its_measurements(
        "b0_wall_clock_mask_seed",
        "b0_wall_clock_mask_probe",
        false,
    );
}

#[test]
fn an_ordinary_run_keeps_its_measurements_when_the_parent_environment_masks() {
    assert_ordinary_run_keeps_its_measurements(
        "b0_wall_clock_seed",
        "b0_wall_clock_probe",
        true,
    );
}

/// The identifier half of the same property the wall-clock family covers: an
/// artifact a COMPARED run writes has to be reproducible, and a value drawn
/// fresh from OS randomness per process defeats that exactly as thoroughly as a
/// measured duration does.
///
/// A telemetry queue mints two such values, both `Uuid::new_v4()` in
/// `stone-rs/src/telemetry.rs`: the consent generation in
/// `reports/.telemetry-consent` and the session id on every event row. They are
/// the reason `--multi-order`'s isolated reference was not reproducible against
/// ITSELF -- two runs of the same two-sim selection produced identical file
/// SIZES and four different content hashes. An isolated-only sim never gets a
/// second process to compare with, which is why this never showed.
///
/// Both compared arms carry the ids here. `artifacts` is where the real defect
/// lived; `entry_diagnostics` carries the logged line so a failure names WHICH
/// id moved. The `artifacts` population assert is what stops this passing
/// because the queue never opened.
#[test]
fn a_telemetry_id_does_not_diverge_across_the_entry_boundary() {
    let (isolated, worker) = differential(&["b0_telemetry_ids_seed"], "b0_telemetry_ids_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_telemetry_ids_probe", &c);

    for (label, record) in [("isolated", &isolated), ("worker", &worker)] {
        let (consent, session) = telemetry_ids(&record.entry_diagnostics)
            .unwrap_or_else(|| panic!("{label}: no telemetry record line:\n{}", record.entry_diagnostics));
        // Agreement over an id that was never minted is agreement over nothing.
        assert!(
            is_uuid(&consent) && is_uuid(&session),
            "{label}: the reproducible ids must still be well-formed UUIDs, got consent={consent} session={session}"
        );
        // A reproducible SEQUENCE, not one constant: the two kinds are drawn at
        // the same index and must not collide.
        assert_ne!(consent, session, "{label}: the consent generation and the session id must differ");
        // The undeclared control in the same record still compares.
        assert!(
            record.entry_diagnostics.contains("queue 1"),
            "{label}: the control must survive: {}",
            record.entry_diagnostics
        );
    }
    assert!(
        c.population("artifacts") > 0,
        "the artifact arm compared nothing, so the telemetry files were never checked"
    );
}

/// `ROCK_DETERMINISTIC_IDS` is the HARNESS's, and a compared run stays
/// reproducible however loudly a fixture asks for real ids. Without this the
/// gate could be talked out of checking the very fixtures it exists for: a sim
/// could declare its way out of the comparison and the gate would report green
/// on artifacts it had stopped looking at.
///
/// The declaring pair is the mutation subject -- with the runner publishing the
/// flag before a sim's `@sim_env`, this fixture's `"0"` would win and both sides
/// would mint fresh UUIDs the boundary cannot restore.
#[test]
fn a_fixture_cannot_ask_a_compared_run_for_real_telemetry_ids() {
    let (isolated, worker) =
        differential(&["b0_telemetry_ids_unmask_seed"], "b0_telemetry_ids_unmask_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_telemetry_ids_unmask_probe", &c);
    assert!(
        c.population("artifacts") > 0,
        "the artifact arm compared nothing, so the fixture's declaration was never tested"
    );
}

/// The other direction, and the one that protects the shipping game: telemetry
/// is a product feature, and a build that emitted a fixed session id would
/// collapse every real player's sessions into one. An ordinary `rock sim` run
/// is not compared with anything, so it must keep drawing real random ids --
/// whatever the fixture or the parent environment asks for.
///
/// Proven by RUNNING TWICE and requiring the ids to differ, rather than by
/// asserting against a known derived value: that is the property that actually
/// matters, and it cannot be satisfied by a derivation that merely looks random.
fn assert_ordinary_run_keeps_real_ids(seed: &str, probe: &str, parent_env: &[(&str, &str)]) {
    for workers in [false, true] {
        let args = if workers {
            vec!["sim", "testbed", seed, probe, "--workers"]
        } else {
            vec!["sim", "testbed", probe]
        };

        let mut seen: Vec<(String, String)> = Vec::new();
        for _ in 0..2 {
            let stderr = run_rock_with_env(&args, parent_env);
            let ids = telemetry_ids(&stderr)
                .unwrap_or_else(|| panic!("{args:?} emitted no telemetry record line:\n{stderr}"));
            assert!(
                is_uuid(&ids.0) && is_uuid(&ids.1),
                "{args:?} must emit well-formed UUIDs, got {ids:?}"
            );
            seen.push(ids);
        }
        assert_ne!(
            seen[0], seen[1],
            "{args:?} (parent_env={parent_env:?}) is not a compared run, so it must keep drawing \
             REAL random telemetry ids; two runs produced the same pair"
        );
    }
}

#[test]
fn an_ordinary_run_keeps_its_real_telemetry_ids() {
    assert_ordinary_run_keeps_real_ids("b0_telemetry_ids_seed", "b0_telemetry_ids_probe", &[]);
}

#[test]
fn an_ordinary_run_keeps_its_real_telemetry_ids_when_the_fixture_asks_for_reproducible_ones() {
    assert_ordinary_run_keeps_real_ids(
        "b0_telemetry_ids_mask_seed",
        "b0_telemetry_ids_mask_probe",
        &[],
    );
}

#[test]
fn an_ordinary_run_keeps_its_real_telemetry_ids_when_the_parent_environment_asks_otherwise() {
    assert_ordinary_run_keeps_real_ids(
        "b0_telemetry_ids_seed",
        "b0_telemetry_ids_probe",
        &[("ROCK_DETERMINISTIC_IDS", "1")],
    );
}

/// Pull `consent` and `session` out of the fixture's one log line.
fn telemetry_ids(text: &str) -> Option<(String, String)> {
    let line = text.lines().find(|l| l.contains("b0 telemetry record:"))?;
    let after = |key: &str| -> Option<String> {
        let rest = line.split_once(key)?.1;
        Some(rest.split(|c: char| c == ',').next()?.trim().to_string())
    };
    Some((after("consent on ")?, after("session ")?))
}

fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && [8usize, 4, 4, 4, 12] == groups.iter().map(|g| g.len()).collect::<Vec<_>>()[..]
        && value.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
}

/// The Session Arena registry crosses the REAL worker boundary intact.
///
/// The b1 fixtures call `sim_checkpoint_take`/`sim_checkpoint_restore` directly
/// and declare `SIM_WORKER_ISOLATED`, so they certify the registry IMAGE and
/// nothing about `sim_worker_restore_boot` using it. This pair declares
/// neither, so the probe runs as the second entry in the seed's process.
///
/// Provenance is asserted rather than assumed, because every way this fixture
/// could fail to be evidence looks like a pass: a `SIM_WORKER_ISOLATED`
/// declaration, a boot-signature refusal, or a recovery rerun would all put the
/// probe in a fresh process, where its agreement with a fresh process is
/// agreement between two fresh processes. Two independent signals say it did
/// not: the entry's position in its queue, and the absence of the process boot
/// log an entry that booted its own process necessarily carries.
#[test]
fn the_arena_registry_survives_the_real_worker_boundary() {
    let (isolated, worker) = differential(&["b2_arena_boundary_seed"], "b2_arena_boundary_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b2_arena_boundary_probe", &c);

    assert_eq!(isolated.seq, 0, "the isolated arm is a one-entry queue");
    assert!(
        worker.seq > 0,
        "the probe ran first in its process, so it crossed no boundary and \
         certifies nothing about the restore"
    );
    assert!(
        isolated.diagnostics.contains("Initialized successfully"),
        "the isolated arm must carry the boot log of the process it booted: {:?}",
        isolated.diagnostics
    );
    assert!(
        !worker.diagnostics.contains("Initialized successfully"),
        "the worker arm carries a process boot log, so it booted its own process \
         rather than following the seed: {:?}",
        worker.diagnostics
    );

    // The artifact is what makes agreement mean something: both sides emit
    // PASS whatever the ids are, and only this compares the identities.
    assert_eq!(
        artifact_names(&worker),
        vec!["arena_boundary.txt".to_string()],
        "the probe's identity manifest is the compared evidence"
    );
    assert!(
        c.population("artifacts") > 0,
        "the artifact arm compared nothing, so the identities were never checked"
    );
    assert!(
        c.population("expects_pass") > 0,
        "the fixture asserted nothing"
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
/// StateSchema and the checkpoint restores it like any other. Pinned at
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

/// An ordinary main-coroutine error is a finished entry, not a worker crash.
/// The head is deliberately manual-only because its FAIL/no-stop record is the
/// boundary under test. The update marker proves that the runner did not leave
/// the errored raw handle live until its frame budget; the successor's crossing
/// proves no recovery boot was inserted between the two entries.
#[test]
fn an_errored_main_closes_without_recovery_or_tail_updates() {
    let records = run_worker_suite(
        Backend::Skull,
        "testbed",
        &[
            "b0_main_error_head".to_string(),
            "b0_main_error_probe".to_string(),
        ],
    )
    .expect("an ordinary main error must finalize its entry and keep the worker alive");
    assert_eq!(records.len(), 2, "both entries must be finalized");

    let boots = records
        .iter()
        .map(|(record, _)| record.diagnostics.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .matches("[STONE] Initialized successfully")
        .count();
    assert_eq!(boots, 1, "the ordinary error must not trigger a recovery boot");

    let (head, head_crossed) = &records[0];
    assert!(!head_crossed, "the head follows no predecessor");
    assert_eq!(head.verdict(), "FAIL", "the deliberate main error must remain visible");
    assert!(!head.saw_stop, "the failed main never called stop_sim(): {head:?}");
    assert!(
        head.expects_pass
            .iter()
            .any(|message| message.message.contains("reached an actual game frame")),
        "the head must perform its real fixture assertion before raising: {head:?}"
    );
    assert!(
        head.entry_diagnostics.contains("b0-main-error-boundary"),
        "the main coroutine error must be logged on the failed entry: {head:?}"
    );
    assert!(
        !head.entry_diagnostics.contains("b0-main-error-tail-update"),
        "the failed entry must not execute the post-error update witness: {head:?}"
    );
    assert!(
        head.expects_fail.is_empty(),
        "the update witness must report marker-write failures instead of hiding them: {head:?}"
    );
    assert!(
        !head.artifacts.contains_key("post_error_update.txt"),
        "an errored main must not burn its frame budget in update systems: {head:?}"
    );

    let (probe, probe_crossed) = &records[1];
    assert!(
        *probe_crossed,
        "the successor must follow the failed entry in the same worker: {probe:?}"
    );
    assert_eq!(probe.verdict(), "PASS", "the successor must still complete: {probe:?}");
    assert!(probe.saw_stop, "the successor must call stop_sim(): {probe:?}");
    assert!(
        probe
            .expects_pass
            .iter()
            .any(|message| message.message.contains("successor ran")),
        "the successor assertion must execute: {probe:?}"
    );
    assert!(probe.artifacts.contains_key("successor.txt"));
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
    assert!(
        seed.artifacts.contains_key("shutdown.txt"),
        "the worker left through the ORDINARY exit, so `@shutdown` systems, \
         `replay_finish` and engine teardown ran -- an `os_exit` would skip all \
         of it and `runs.txt`, written mid-entry, could not tell: {:?}",
        artifact_names(seed)
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

/// A voice on Bitgun's boot-owned sound, which cannot be covered by sound
/// teardown and therefore exercises the voice destructor itself.
///
/// The load-bearing assertion is the SUCCESSOR's provenance. A boundary that
/// destroyed what the seed left lets the worker continue, so the probe follows
/// a predecessor; one that could not ends the worker, and the probe restarts
/// fresh with no predecessor at all. The probe's own assertions cannot tell
/// those apart -- it passes either way -- which is exactly the trap a
/// leak fixture falls into once containment exists.
#[test]
fn a_predecessors_voice_on_a_boot_sound_does_not_reach_the_next_entry() {
    let records = run_worker_suite(
        Backend::Skull,
        "bitgun",
        &["b3_voice_leak_seed".to_string(), "b3_voice_leak_probe".to_string()],
    )
    .expect("the suite call itself must not error");
    assert_eq!(records.len(), 2);

    let (seed, _) = &records[0];
    assert_eq!(seed.verdict(), "PASS", "the seed leaves its voice live on purpose: {seed:?}");

    let (probe, probe_crossed) = &records[1];
    assert_eq!(probe.verdict(), "PASS", "the successor inherits boot's audio world: {probe:?}");
    assert!(
        probe_crossed,
        "the boundary DESTROYED what the seed left, so the worker carried on and \
         this entry followed a predecessor -- a teardown that could not would have \
         ended the worker and restarted this entry fresh, and its own assertions \
         would still pass"
    );
    assert!(
        !probe.expects_pass.is_empty(),
        "the probe asserted nothing: {probe:?}"
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

/// A boot world whose canvas contents are NOT creation's cannot be returned by
/// clearing, so it is refused from a worker rather than silently zeroed.
///
/// This is the premise the whole contents reset rests on. `tex_image_2d(.., None)`
/// allocates without specifying contents, so creation now CLEARS explicitly and
/// the boundary restores to that defined state -- which is only sound while boot
/// itself has not painted over it. `b3_cc_paint_at_boot` makes `@init` paint one,
/// and the worker must say so instead of installing blank where boot's
/// projection was white.
///
/// The pixel value cannot be the assertion: boot's white survives in a fresh
/// process AND in a worker that never cleared it, so the probe agrees either
/// way. The REASON is the observable, and no entry may cross a boundary.
#[test]
fn a_boot_painted_canvas_is_refused_from_a_worker() {
    let records = run_worker_suite(
        Backend::Skull,
        "testbed",
        &["b3_cc_painted_head".to_string(), "b3_cc_painted_probe".to_string()],
    )
    .expect("the suite call itself must not error: ineligibility is a scheduling fact");
    assert_eq!(records.len(), 2, "one record per entry spec");

    for (record, crossed) in &records {
        assert_eq!(
            record.verdict(),
            "PASS",
            "refusing the boot world is a scheduling fact, not a failure: {record:?}"
        );
        assert!(
            !crossed,
            "no entry may inherit a boot world whose canvas contents cannot be \
             reconstructed -- every one of these must have restarted fresh"
        );
    }

    // TWO names: a one-entry queue takes no checkpoint and never reaches a
    // boundary, so it would report nothing and this assertion would be about
    // the wrong thing. The handle NUMBER is deliberately not asserted -- ids are
    // monotonic per process, so pinning one pins a coincidence of allocation
    // order.
    let (_, out, err) = run_rock_status(&[
        "sim",
        "testbed",
        "--multi-order",
        "b3_cc_painted_head",
        "b3_cc_painted_probe",
    ]);
    let reason = format!("{out}{err}");
    assert!(
        reason.contains("boot state whose contents cannot be reconstructed")
            && reason.contains("canvas:"),
        "the refusal must name its reason AND the class that raised it, so an \
         ineligible boot world is a reported fact rather than a silently slower \
         suite:\n{reason}"
    );
}

/// The DEFAULT mode is the hybrid: render-skipped entries share a
/// boot-checkpoint worker, entries that render get a fresh process with a
/// reported reason.
///
/// Both populations are asserted non-empty. A split that admitted everything,
/// or that admitted nothing, would satisfy a test that only looked at one side
/// -- and "nothing was admitted" is exactly what a silently broken predicate
/// produces while every sim still passes.
///
/// The default admits RENDERED entries into a shared worker, since B4. Two
/// RENDERED sims, because that is the only corpus that can show it: entries
/// declaring different `SIM_SKIP_RENDER` values have different BOOT SIGNATURES
/// and get separate workers whatever the rendered rule says, so a mixed corpus
/// boots twice both before and after the flip and discriminates nothing.
///
/// `-j 1` because a signature group splits into at most `--jobs` CONTIGUOUS
/// lanes and each lane's head starts its own worker (B5) -- at -j 4 these two
/// land in separate lanes and each boots, which says nothing about admission.
#[test]
fn the_default_mode_admits_rendered_sims_into_one_worker() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "canvas_create_preserves_color_mask",
        "b3_readback_control",
    ]);
    let all = format!("{stdout}{stderr}");
    assert!(ok, "the default mode must run both green:\n{all}");

    assert!(
        !all.contains("runs isolated") && !all.contains("renders (SIM_SKIP_RENDER=0)"),
        "rendering is no longer a reason to isolate; nothing may report one:\n{all}"
    );

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "two rendered entries of one signature must SHARE a worker: {boots} boots\n{all}"
    );
}

/// A boot-owned pixel-readback slot's CONTENTS are returned to creation's at the
/// boundary. The set diff cannot see this class: the slot is present, at its
/// boot id, neither extra nor missing, and what crossed is a promoted submit
/// inside it (`a97015add`).
///
/// Three entries, and the order is the point. `b3_readback_control` proves the
/// priming recipe yields a readable 1.0 inside ONE entry, so a sentinel in the
/// probe cannot be blamed on priming that never worked. The seed primes and
/// deliberately never reads -- `consume` is one-shot and clears `ready_primed`,
/// so any check that the seed primed the slot would be the thing that unprimes
/// it. The probe then consumes and must see the sentinel.
///
/// `-j 1` for the lane reason above: at -j 4 the seed and probe land in separate
/// lanes, each boots, and the probe reads the sentinel because it is a FRESH
/// PROCESS -- passing for a reason that has nothing to do with the boundary.
/// Measured: that is exactly what an earlier version of this fixture did.
#[test]
fn a_boot_pixel_readback_slot_does_not_carry_its_submit_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_readback_control",
        "b3_readback_seed",
        "b3_readback_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the readback boundary must be green:\n{all}");
}

/// A file change queued during one entry is not delivered to the next as a
/// hot-reload event.
///
/// The asset watcher's `CHANGED` set is a QUEUE a SUCCESSOR drains, and its
/// stores are path-keyed, so id accounting cannot see them and neither
/// per-handle axis has anything to hang them on. Live under the harness:
/// `ASSET_DEV_MODE` is on for every `sim` invocation.
///
/// The oracle is the EVENT, not the queue depth, and that distinction is
/// measured rather than assumed: two earlier versions asserted on the queue and
/// the mutation that deletes the restore survived both, because whoever polls
/// first empties it. With the restore deleted the probe hot-reloads three fonts
/// it never touched -- that is the defect, and `ASSET_CHANGE_EVENTS` is what
/// cannot be laundered.
#[test]
fn a_predecessors_file_change_is_not_the_next_entrys_hot_reload() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_watcher_active",
        "b3_watcher_seed",
        "b3_watcher_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the asset watcher boundary must be green:\n{all}");
}

/// A sampler an entry binds on a unit >= 2 does not reach the next entry.
///
/// Units 2 and up, and the choice carries the assertion. Units 0 and 1 are
/// rewritten every frame by the present and tonemap passes, so a violation on
/// either is laundered before any reading can attribute it and an assertion
/// over them passes whether or not the boundary works -- the `sampler_release`
/// lesson in CLAUDE.md, learned by deleting two of six releases and watching
/// the sweep stay green. Every reading is taken before the entry's first frame
/// advance, since the render path binds samplers of its own.
#[test]
fn a_predecessors_sampler_does_not_reach_the_next_entrys_texture_unit() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_sampler_active",
        "b3_sampler_seed",
        "b3_sampler_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the sampler unit boundary must be green:\n{all}");
}

/// The colour mask, clear colour and scissor an entry leaves do not reach the
/// next entry.
///
/// Their own ambient row rather than a side effect: today they are normalized
/// only inside `stone_canvas_reset_contents`, so a program with no boot canvas
/// -- or one whose canvases the policy refuses -- normalizes nothing, and the
/// per-frame site that would otherwise cover them sits inside the render gate
/// and never runs for a non-rendered sim. This fixture renders nothing, which
/// is what makes the probe's reading about the boundary and not about a frame.
#[test]
fn a_predecessors_gl_write_state_does_not_reach_the_next_entry() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim", "testbed", "-j", "1", "b3_ws_active", "b3_ws_seed", "b3_ws_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the GL write state boundary must be green:\n{all}");
}

/// A boot canvas an entry NARROWED is back at boot's active rect for the next
/// entry.
///
/// The half of a canvas's contents the paint flag cannot see:
/// `stone_set_canvas_active` moves the sub-rect every later bind draws into and
/// sets no `bound_as_target`, so the canvas reports "never painted" while its
/// geometry has moved. Per-handle, so it lives in the canvas class's own policy
/// rather than in `AMBIENT` -- one process-wide row over N canvases could not
/// say which canvas moved.
///
/// The probe reads before its first frame advance, which is the only window
/// where this is observable: `set_scene_active_rects` re-establishes the rect on
/// the successor's first `frame_core_resolve_view`, so a reading one frame later
/// passes with the fix deleted.
#[test]
fn a_predecessors_canvas_narrowing_does_not_reach_the_next_entry() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim", "testbed", "-j", "1", "b3_car_active", "b3_car_seed", "b3_car_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the canvas active-rect boundary must be green:\n{all}");
}

/// A canvas BOOT itself narrowed takes that boot signature off the worker.
///
/// The refusal half of the same policy, and the reason `boot_mutated` grew a
/// second question rather than the reset simply growing a second write: widening
/// boot's own narrowed rect back to the full size installs a geometry boot never
/// had, which is a different wrong state rather than a restore. Both sims still
/// PASS -- a refusal is not an error -- so the BOOT COUNT is the evidence.
#[test]
fn a_boot_narrowed_canvas_refuses_the_worker_and_names_itself() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim", "testbed", "-j", "1", "b3_car_boot_narrowed", "b3_car_boot_narrowed_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    assert!(
        all.contains("boot state whose contents cannot be reconstructed: canvas:"),
        "the refusal must name the canvas that earned it:\n{all}"
    );
    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 2,
        "a refused boot world must put the successor in a FRESH process: \
         {boots} boots\n{all}"
    );
    assert!(ok, "a refusal is not a failure; both entries still run:\n{all}");
}

/// A warning the RUST logger suppressed for one entry is not suppressed for
/// the next.
///
/// The Rust half of the store `logger_dedup_does_not_suppress_the_next_entrys
/// _warning` covers on the Lua side. It had no reset at all -- `reset_dedup()`
/// was not even exported -- and it is call-site keyed, so no ledger axis could
/// reach it: a warning that fired in entry 3 was simply silent in entry 7, and
/// a suppressed warning is invisible to every other observation.
///
/// `b3_log_dedup_active` runs first as the recipe control, and carries the
/// row's denominator: boot's image is legitimately EMPTY here, so the number
/// that matters is that a capture RAN.
#[test]
fn the_rust_loggers_dedup_does_not_suppress_the_next_entrys_warning() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_log_dedup_active",
        "b3_log_dedup_seed",
        "b3_log_dedup_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the Rust logger dedup boundary must be green:\n{all}");
}

/// An mpv target an ENTRY created is destroyed as an extra, and its successor
/// stays in the same worker.
///
/// The admitted side of the `unrestorable` gate, and it is the half a
/// rejection-only test cannot supply: a policy that refused every target of the
/// class would satisfy the refusal test below while taking the worker off the
/// table for a program that never booted one.
#[test]
fn an_entry_created_mpv_target_is_torn_down_without_costing_the_worker() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_mpv_entry_only",
        "b3_mpv_entry_only_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "an entry-created target must not cost the worker: {boots} boots\n{all}"
    );
    assert!(ok, "the entry-created mpv target arm must be green:\n{all}");
}

/// A BOOT-owned mpv target takes that boot signature off the worker, reported
/// with the handle that did it.
///
/// The refused side. Both sims still PASS -- the refusal is not an error, it is
/// the boundary declining to claim a world it did not return -- so the verdict
/// alone proves nothing and the BOOT COUNT is the evidence: the follower reruns
/// in a fresh process, which is exactly what the refusal buys.
#[test]
fn a_boot_owned_mpv_target_refuses_the_worker_and_names_itself() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_mpv_boot_live",
        "b3_mpv_boot_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    assert!(
        all.contains("boot state whose contents cannot be reconstructed: mpv_target:"),
        "the refusal must name the handle that earned it:\n{all}"
    );
    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 2,
        "a refused boot world must put the successor in a FRESH process: \
         {boots} boots\n{all}"
    );
    assert!(ok, "a refusal is not a failure; both entries still run:\n{all}");
}

/// A boot-owned IMAGE's texture is returned to boot's at the boundary, pixels
/// and wrap state both.
///
/// The GPU-side half of the ImageData case below, and the one that could not be
/// done by snapshotting contents: reading a texture back needs `glGetTexImage`,
/// which GLES does not have, and imaging every boot asset would cost mag
/// hundreds of megabytes per worker. So boot's TEXTURE OBJECT is retained and
/// the first write to a boot-owned image is diverted onto a replacement
/// (`image_write_target`), which makes the cost scale with the images an entry
/// mutates rather than with the ones it boots.
///
/// `b3_img_active` runs first as the recipe control: it proves the texel probe
/// reads what boot uploaded, that REPEAT is a state creation does not establish,
/// and the capture's own denominator -- a boundary that snapshotted no image
/// diverts nothing and refuses nothing, which looks from the outside exactly
/// like one that found nothing to divert.
///
/// Two boot images, and the untouched one is what separates a restore from a
/// boundary that swept every texture it found.
#[test]
fn a_boot_image_does_not_carry_its_texture_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_img_active",
        "b3_img_seed",
        "b3_img_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the image contents boundary must be green:\n{all}");
}

/// A boot-owned ImageData's PIXELS are returned to boot's at the boundary.
///
/// Same blind spot as the canvas and readback cases -- present at its boot id,
/// neither extra nor missing, contents crossed -- and the first one found by
/// the mutator census rather than by a divergence: `image_data` is one of the
/// classes `ContentPolicy` now forces to declare an answer.
///
/// Boot PAINTS the image, so the probe's assertion separates a RESTORE from a
/// plausible-looking clear: a boundary that reset to creation's zeroes would
/// satisfy "not the seed's red" while installing a state boot never had.
///
/// The boot count is asserted BEFORE the verdict, and it is load-bearing in a
/// way this fixture makes sharp. A restore that reports failure ends the worker
/// and the parent reruns the probe in a FRESH process, where it passes for the
/// ordinary reason -- so "the sims passed" alone is satisfied by the boundary
/// being broken. One boot is what makes the probe follow the seed.
#[test]
fn a_boot_image_data_does_not_carry_its_pixels_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_image_data_seed",
        "b3_image_data_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the ImageData contents boundary must be green:\n{all}");
}

/// A boot-owned shader's default-block UNIFORM VALUES are returned to boot's at
/// the boundary.
///
/// Program state, deliberately: a linked program is immutable but its uniform
/// values are stored in it and outlive whatever set them, so an entry that
/// writes one on a boot-owned shader leaves it written for its successor. The
/// tree already knows this fact one scope down -- `ui_ctx_shader.rock` exists
/// because "a GL uniform is per-program state that persists across flushes".
///
/// `b3_su_active` runs first as the recipe control: it proves `uProbe` is an
/// ACTIVE uniform and that boot's value reads back, so a sentinel in the probe
/// cannot be blamed on a uniform the linker eliminated. It also asserts the
/// capture's own denominator -- a snapshot that walked zero uniforms would
/// satisfy every other assertion here.
#[test]
fn a_boot_shader_does_not_carry_its_uniform_values_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_su_active",
        "b3_su_seed",
        "b3_su_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the shader uniform boundary must be green:\n{all}");
}

/// A uniform BLOCK bound onto a boot-owned shader is returned to the zeroes
/// registration established.
///
/// The block half of the shader class, and the state a real game's uniforms
/// mostly live in. A block has no handle of its own -- `BLOCKS` is keyed by
/// shader handle -- so the identity diff is blind twice over: the buffer is in
/// no live set, and the shader owning it is present at its boot id whatever an
/// entry uploaded.
///
/// `b3_ub_active` runs first as the recipe control and carries more weight here
/// than for any sibling class: the restored contents ARE zeroes, so the probe's
/// reading alone cannot tell a returned buffer from one that never existed. The
/// control proves the block takes an upload and reads it back, and asserts the
/// boundary's own denominator.
#[test]
fn a_boot_shader_does_not_carry_its_uniform_block_bytes_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_ub_active",
        "b3_ub_seed",
        "b3_ub_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the uniform block contents boundary must be green:\n{all}");
}

/// GL's BLEND state is returned to boot's at the boundary.
///
/// Census 3's first row, and the state neither per-handle ledger axis can reach:
/// the blend mode belongs to no handle, so there is nothing to put in a live set
/// and nothing whose contents it is.
///
/// The sharpest case in the arc, because the checkpoint CAUSES it. The Rock-side
/// `CURRENT_BLEND` shadow rides the checkpoint back to boot's value while GL
/// keeps the predecessor's, and the flush loop then SKIPS `set_blend_mode` when
/// the batch's mode already equals the shadow -- so restoring the shadow alone
/// manufactures a disagreement and the guard preserves it. Measured before the
/// fix: the probe's rect drew ADDITIVE at 0.996 where alpha reads 0.75.
///
/// `b3_blend_control` runs first and produces both readings inside one entry, so
/// the spread the other two rest on is shown rather than assumed. All three
/// declare the same `SIM_SKIP_RENDER`, since differing values are different boot
/// signatures and would put the probe in its own process.
#[test]
fn a_successor_entry_draws_with_boots_gl_blend_state() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_blend_control",
        "b3_blend_seed",
        "b3_blend_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the ambient blend boundary must be green:\n{all}");
}

/// The letterbox PRESENT rect is returned to boot's at the boundary.
///
/// Census 3's second row, and the one with no laundering anywhere: every bind of
/// canvas 0 RE-APPLIES the rect, so a successor does not merely inherit stale
/// state, it has the predecessor's sub-rect actively re-installed on its own
/// draws. `present_letterbox_end` is the only clear and early-returns when the
/// scene covers the drawable, which it does in every project here.
///
/// No rendering needed, and that is the point: with draws skipped there is still
/// nothing between the seed and the probe that could put the rect back. The
/// probe asserts the CONSEQUENCE as well as the stored value -- what GL's
/// viewport becomes after a canvas-0 bind.
#[test]
fn a_successor_entry_presents_through_boots_letterbox_rect() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3pv_control",
        "b3pv_seed",
        "b3pv_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the ambient present-viewport boundary must be green:\n{all}");
}

/// The ACTIVE cursor and the pointer visibility are returned to boot's.
///
/// Census 3's third row. `CURSORS` is a ledger class and its handles are diffed,
/// so an entry-created cursor is an extra and gets destroyed -- but WHICH cursor
/// SDL points at belongs to no handle, and the boundary's own teardown moves it:
/// creating a cursor ACTIVATES it, and `stone_free_cursor` resets the cursor when
/// it frees the active one.
///
/// Boot owns a cursor of its own in this fixture, and that is what makes the
/// probe discriminating: with boot's active id at 0 the teardown's reset would
/// land on boot's value by accident, and a restore would be indistinguishable
/// from doing nothing.
#[test]
fn a_successor_entry_points_at_boots_cursor() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3c_control",
        "b3c_seed",
        "b3c_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the ambient cursor boundary must be green:\n{all}");
}

/// Both master volumes and every bus's parameters are returned to boot's.
///
/// Census 3's fourth row, and the one whose probe can assert EQUALITY rather
/// than a difference: boot's own reading rides a Rock global the checkpoint
/// returns to every entry, so a boundary installing plausible defaults instead
/// of boot's values fails the assertion. The control runs LAST because it
/// disturbs the mix without putting it back, and running it first would make its
/// leftovers the seed's starting state.
///
/// `STONE_AUDIO_STUB=0` on all three, and the same value on all three: a
/// differing one is a different boot signature and would give the probe its own
/// process.
#[test]
fn a_successor_entry_mixes_with_boots_audio_ambient() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3a_seed",
        "b3a_probe",
        "b3a_control",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the ambient audio boundary must be green:\n{all}");
}

/// Under `STONE_AUDIO_STUB` the audio row RECORDS, because the state is real.
///
/// It used to record ABSENT, and that was right for a stub with no `AudioState`
/// at all. A stubbed run now builds a real state with no device -- the volumes
/// and every bus parameter exist and take writes -- so that state genuinely
/// crosses a boundary in the configuration every sim runs by default, and
/// "absent" there would have been a row publishing "nothing to capture" over
/// state a predecessor can really move. The fixture proves the writability
/// rather than asserting it, and checks the neighbouring rows so the answer is
/// this row's rather than a process-wide capture.
///
/// Its own run: the default environment is a different boot signature from the
/// trio above, which asserts it shares one process.
#[test]
fn the_audio_row_records_under_the_stub_because_its_state_is_real() {
    let (ok, stdout, stderr) = run_rock_status(&["sim", "testbed", "b3a_stub_recorded"]);
    let all = format!("{stdout}{stderr}");
    assert!(ok, "the stub's ambient answer must be green:\n{all}");
}

/// A block BOOT filled takes the worker off the table, named as the shader that
/// owns it.
///
/// The refusal half of the same policy, and the reason the block half is a
/// RESET rather than a snapshot everywhere: zeroing a buffer boot itself
/// uploaded into installs contents boot never had, which is a different wrong
/// state rather than a restore.
///
/// Its own `@sim_env`, so it is a different boot signature from the entries
/// above -- which assert they share one process, and could not if this
/// fixture's refusal reached them.
#[test]
fn a_block_boot_filled_takes_the_worker_off_the_table() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_ub_boot_filled",
        "b3_ub_boot_filled_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 2,
        "a block boot filled cannot be zeroed back, so the successor must be \
         RERUN in a fresh process: {boots} boots\n{all}"
    );
    assert!(
        all.contains("ended its worker: boot state whose contents cannot be reconstructed: shader:"),
        "and the refusal must name the shader that owns it:\n{all}"
    );
    assert!(ok, "while every entry still runs and passes:\n{all}");
}

/// A boot-owned retained mesh's INSTANCE BUFFER is returned to boot's at the
/// boundary.
///
/// `stone_mesh_set_instances` is a one-shot latch, so an entry that latches a
/// boot mesh does not merely leave contents behind: the mesh is unlatchable for
/// every successor after it, and the live-handle diff sees nothing because the
/// mesh is present at its boot id.
///
/// `b3_mesh_active` runs first as the recipe control. It latches a mesh of its
/// OWN -- no FFI un-latches one, so a control writing a boot mesh could not put
/// it back -- and it asserts both the capture's denominator and the one-shot
/// premise the restore is built on.
#[test]
fn a_boot_mesh_does_not_carry_its_instance_buffer_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_mesh_active",
        "b3_mesh_seed",
        "b3_mesh_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the mesh instance boundary must be green:\n{all}");
}

/// A boot-owned font's ATLAS is re-baked to boot's raster at the boundary.
///
/// `stone_reload_font` and `stone_resize_font` rebuild the atlas under the same
/// font_id and the same GL texture object -- on purpose, so every Font handle
/// stays valid -- which is exactly what keeps the crossing out of the
/// live-handle diff.
///
/// `b3_font_active` runs first as the recipe control. It re-bakes a font of its
/// OWN, asserts the capture's denominator, and asserts the premise the restore
/// rests on: that a re-raster from the same bytes at the same size reproduces
/// the glyph table exactly.
#[test]
fn a_boot_font_does_not_carry_its_atlas_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "-j",
        "1",
        "b3_font_active",
        "b3_font_seed",
        "b3_font_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the font atlas boundary must be green:\n{all}");
}

/// A boot-owned sound's DECODED BUFFER is returned to boot's at the boundary.
///
/// `stone_reload_sound` decodes a new file over a live handle and keeps the
/// handle -- so every `Sound` naming it survives a hot reload, which is what
/// keeps the crossing out of the live-handle diff.
///
/// bitgun rather than testbed for the reason `b3_native_audio` gives:
/// `stone_load_sound` returns 0 for a missing file and testbed ships no audio.
/// All three entries carry the same `@sim_env("STONE_AUDIO_STUB", "0")`,
/// because a differing one is a different boot signature and would put the
/// probe in a worker of its own.
///
/// `b3_sound_active` runs first as the recipe control. It reloads a sound of
/// its OWN, asserts the capture's denominator, and asserts that the two files
/// really do decode differently -- without which the probe would pass for a
/// boundary that touched nothing.
#[test]
fn a_boot_sound_does_not_carry_its_decoded_buffer_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "bitgun",
        "-j",
        "1",
        "b3_sound_active",
        "b3_sound_seed",
        "b3_sound_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the sound contents boundary must be green:\n{all}");
}

/// A BOOT-owned voice takes the worker off the table, named as the handle that
/// did it.
///
/// The one class that answers a boundary with a refusal rather than a restore:
/// a voice's contents are its position in a buffer, that position advances on
/// its own with the wall clock, and nothing can put it back -- `SoundSource`
/// implements no `try_seek` so rodio's default answers `NotSupported`, and a
/// voice whose samples ran out cannot be restarted at all.
///
/// Two invocations, because the arms differ in a `@sim_env` value and so are
/// different boot signatures by construction. The second is what scopes the
/// refusal to BOOT: an entry's own voice is an extra the class diff destroys,
/// and its successor still shares the process.
#[test]
fn a_boot_voice_takes_the_worker_off_the_table() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "bitgun",
        "-j",
        "1",
        "b3_voice_boot_live",
        "b3_voice_boot_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 2,
        "a boot voice cannot cross a boundary, so the successor must be RERUN \
         in a fresh process: {boots} boots\n{all}"
    );
    assert!(
        all.contains("ended its worker: boot state whose contents cannot be reconstructed: voice:"),
        "and the refusal must name the handle that caused it:\n{all}"
    );
    assert!(ok, "while every entry still runs and passes:\n{all}");

    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "bitgun",
        "-j",
        "1",
        "b3_voice_entry_only",
        "b3_voice_entry_only_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "an ENTRY's voice is an extra the diff destroys, so its successor \
         still follows it in one process: {boots} boots\n{all}"
    );
    assert!(
        !all.contains("ended its worker"),
        "and nothing about that boot world is unreturnable:\n{all}"
    );
    assert!(ok, "with both entries green:\n{all}");
}

/// A boot-owned track's place at the single music SINK is returned at the
/// boundary, and a track boot was playing refuses instead.
///
/// The bytes behind a music handle are immutable -- nothing writes
/// `audio.music` after the insert -- so what crosses is which handle the one
/// sink is playing. A track boot left SILENT is returned by stopping the
/// playback an entry left; a track boot left PLAYING cannot be returned at all,
/// because its offset advances on its own and `repeat_infinite()`'s `Buffered`
/// supports no seek.
///
/// Two invocations, because the arms differ in a `@sim_env` value and so are
/// different boot signatures by construction.
///
/// `b3_music_active` runs first as the recipe control. It plays a track of its
/// OWN, asserts the capture's denominator, and asserts that playing one really
/// does move the sink -- without which the probe would pass for a boundary that
/// touched nothing.
#[test]
fn a_boot_track_does_not_carry_the_music_sink_across_the_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "bitgun",
        "-j",
        "1",
        "b3_music_active",
        "b3_music_seed",
        "b3_music_probe",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 1,
        "the probe must FOLLOW the seed in one process or it proves nothing: \
         {boots} boots\n{all}"
    );
    assert!(ok, "the music sink boundary must be green:\n{all}");

    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "bitgun",
        "-j",
        "1",
        "b3_music_boot_playing",
        "b3_music_boot_playing_follows",
    ]);
    let all = format!("{stdout}{stderr}");

    let boots = all.matches("[STONE] Initialized successfully").count();
    assert_eq!(
        boots, 2,
        "a track boot was PLAYING cannot cross, so the successor must be RERUN \
         in a fresh process: {boots} boots\n{all}"
    );
    assert!(
        all.contains("ended its worker: boot state whose contents cannot be reconstructed: music:"),
        "and the refusal must name the handle that caused it:\n{all}"
    );
    assert!(ok, "while every entry still runs and passes:\n{all}");
}

/// Run `rock` and return `(exit_ok, stdout, stderr)` WITHOUT asserting on the
/// exit code. The multi-order gate deliberately exits nonzero on a corpus that
/// cannot populate every arm, and that refusal is part of what is under test.
fn run_rock_status(args: &[&str]) -> (bool, String, String) {
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
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run rock {args:?}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `[multi-order] <n> comparisons followed a predecessor ...` -> `n`.
fn boundary_crossings(stdout: &str) -> usize {
    let line = stdout
        .lines()
        .find(|l| l.contains("comparisons followed a predecessor"))
        .unwrap_or_else(|| panic!("the gate never reported its provenance split:\n{stdout}"));
    line.split_whitespace()
        .find_map(|w| w.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("no count in `{line}`"))
}

/// A NAMED subset is the same gate over a smaller corpus, and the property that
/// makes that true is the boundary-crossing count.
///
/// A subset that quietly ran its sims isolated -- or that narrowed the corpus
/// only after the arms were scheduled -- would still print three arms and still
/// agree with the reference, because two fresh processes agree with each other.
/// Only a non-zero boundary-crossing population says a restore was exercised at
/// all, which is why that is what this asserts. The count is exact: two eligible
/// sims share one worker per arm, so the second entry crosses and the first does
/// not, three times over.
///
/// An equivalent subset must EXIT ZERO. testbed passes everything standalone,
/// so `expects_fail`/`errors` have no population -- which is a hole in a
/// whole-corpus gate and means nothing here, because the caller picked this
/// pair to re-measure one class, not to populate every arm. Refusing over it
/// would make the iteration gate unusable as an iteration gate.
#[test]
fn a_named_subset_is_the_same_gate_and_still_crosses_a_worker_boundary() {
    let (ok, stdout, stderr) = run_rock_status(&[
        "sim",
        "testbed",
        "--multi-order",
        "b0_leak_seed",
        "b0_leak_probe",
    ]);

    assert!(
        stdout.contains("[multi-order] 2 sims"),
        "the corpus must be the two named sims, not the whole registry:\n{stdout}"
    );
    assert_eq!(
        boundary_crossings(&stdout),
        3,
        "one worker per arm over two eligible sims crosses exactly once per arm:\n{stdout}"
    );
    assert!(
        !stdout.contains("diverged from the isolated reference"),
        "the subset must agree with its own fresh-process references:\n{stdout}"
    );
    assert!(
        ok,
        "an equivalent subset is the iteration gate and must exit zero:\n{stdout}\n{stderr}"
    );
}

/// The refusals that keep a subset from silently measuring less than it was
/// asked to. Each is the CLI surface of a `select_gate_corpus` unit test.
#[test]
fn a_subset_refuses_a_name_it_cannot_measure() {
    let (ok, _, stderr) =
        run_rock_status(&["sim", "testbed", "--multi-order", "b0_leak_seed", "no_such_sim"]);
    assert!(!ok, "an unknown name must not run the gate over the ones that matched");
    assert!(stderr.contains("no_such_sim"), "the refusal names the offender:\n{stderr}");

    let (ok, _, stderr) =
        run_rock_status(&["sim", "testbed", "--multi-order", "b0_leak_seed", "b0_leak_seed"]);
    assert!(!ok, "a repeated name has one reference record and must refuse");
    assert!(stderr.contains("b0_leak_seed"), "the refusal names the offender:\n{stderr}");
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

/// A task the BOOT process owns is restored at the boundary and keeps being
/// polled -- the half of task handling
/// `a_task_left_live_at_stop_sim_does_not_tick_in_the_next_entry` does not
/// reach. That test covers a task an ENTRY created and abandoned; the
/// checkpoint's own image of `POLLER` had never been exercised, because no
/// production `@init` spawns a task, so every game's checkpoint captured an
/// empty poller.
///
/// Two arms, and it needs both. The counter's VALUE shows the boot image came
/// back rather than the predecessor's accumulation; its ADVANCE shows the
/// restored registration is live. A delta alone cannot decide this -- the boot
/// task ticks in a fresh process too, so a delta agrees whether or not the
/// restore happened, which is the opposite discriminant from the abandoned-task
/// test above.
///
/// The provenance guard is load-bearing here rather than decorative: driving
/// this pair as a comma-chain boots TWICE, so the probe reads creation state as
/// a fresh process and passes with the restore deleted (measured).
#[test]
fn a_boot_owned_task_is_restored_and_still_polled_in_the_next_entry() {
    let (isolated, worker) = differential(&["b0_boot_task_seed"], "b0_boot_task_probe");
    let c = compare(&isolated, &worker);
    assert_equivalent("b0_boot_task_probe", &c);

    assert_eq!(isolated.seq, 0, "the isolated arm is a one-entry queue");
    assert!(
        worker.seq > 0,
        "the probe ran first in its process, so it crossed no boundary and \
         certifies nothing about the restore"
    );
    assert!(
        isolated.diagnostics.contains("Initialized successfully"),
        "the isolated arm must carry the boot log of the process it booted: {:?}",
        isolated.diagnostics
    );
    assert!(
        !worker.diagnostics.contains("Initialized successfully"),
        "the worker arm carries a process boot log, so it booted its own process \
         rather than following the seed: {:?}",
        worker.diagnostics
    );

    assert_eq!(worker.verdict(), "PASS", "{worker:?}");
    assert!(c.population("expects_pass") > 0, "the arms compared no expect records");
}
