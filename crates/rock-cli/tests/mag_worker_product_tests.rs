use rock_cli::run_sim::{run_differential, run_worker_suite, DifferentialPlan};
use rock_cli::sim_record::compare;
use rock_cli::Backend;

fn rock() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_BIN_EXE_rock"))
}

fn without_absent_agent_daemon_diagnostic(text: &str) -> String {
    text.lines()
        .filter(|line| {
            !(line.contains("[IPC] agent daemon startup failed")
                && line.contains("cannot locate rock-agent-daemon; set ROCK_AGENT_DAEMON_BIN"))
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn mag_worker_reset_discards_unfinished_save() {
    // The seed needs a clean worker (its save request must admit), so two
    // seeds in one worker prove the boundary restore: the second seed runs
    // on the first seed's unfinished save unless the reset discards it.
    // (The former probe sim's checks are package tests in
    // mag/tests/async_save_rules.rock.)
    let entries = vec![
        "mag::async_save_worker_reset_seed".to_string(),
        "mag::async_save_worker_reset_seed".to_string(),
    ];
    let worker = run_worker_suite(Backend::Skull, "mag", rock(), &entries)
        .expect("save reset worker queue must run");
    assert_eq!(worker.len(), 2, "both save reset entries must run");
    assert!(worker[1].1, "second seed must cross the first seed's live worker boundary");
    for (entry, (record, _)) in entries.iter().zip(&worker) {
        assert_eq!(record.verdict(), "PASS", "{entry}: {record:?}");
        assert!(record.errors.is_empty(), "{entry}: {record:?}");
        assert!(!record.expects_pass.is_empty(), "{entry} asserted nothing");
    }
}

/// MAG session framing, gauges and counters gameplay code reads (telemetry run
/// and shop edge, world-damage dirty and in-flight gauges, the replay reconcile
/// flag) are boot state to the next entry, not a predecessor's live value.
#[test]
fn mag_worker_boundary_restores_game_diagnostics() {
    let entries = vec![
        "mag::worker_boundary_game_diagnostics_seed".to_string(),
        "mag::worker_boundary_game_diagnostics_probe".to_string(),
    ];
    let worker = run_worker_suite(Backend::Skull, "mag", rock(), &entries)
        .expect("game diagnostics worker queue must run");
    assert_eq!(worker.len(), 2, "both entries must run");
    assert!(worker[1].1, "probe must cross the seed's live worker boundary");
    let isolated = run_differential(
        Backend::Skull,
        "mag",
        rock(),
        &DifferentialPlan { predecessors: Vec::new(), target: entries[1].clone() },
    )
    .expect("fresh-process control must run")
    .0;
    let (probe, _) = &worker[1];
    assert_eq!(isolated.verdict(), "PASS", "the probe passes in a fresh process: {isolated:?}");
    // Verdicts and assertion populations rather than the whole comparison: the
    // worker arm also carries process diagnostics (an absent agent daemon) that
    // are the harness environment's, not the boundary's.
    assert_eq!(probe.verdict(), "PASS", "the probe passes behind the seed: {probe:?}");
    assert!(probe.errors.is_empty(), "the probe raised behind the seed: {probe:?}");
    assert!(!isolated.expects_pass.is_empty(), "the probe asserted nothing");
    assert_eq!(
        probe.expects_pass.len(),
        isolated.expects_pass.len(),
        "the probe asserted the same population in a worker as alone"
    );
    assert_eq!(
        isolated.artifacts["session.txt"], probe.artifacts["session.txt"],
        "the probe read the same telemetry framing in a worker as alone"
    );
}

#[test]
fn mag_bootimage_queue_isolates_scene_and_gameplay_entries() {
    // Package tests cover asset installation. This seed/probe pair keeps the
    // live worker boundary: authored Scene roots and gameplay state must reset.
    let entries = vec![
        "mag::boot_image_scene_registry_npcs_seed".to_string(),
        "mag::boot_image_scene_registry_npcs_probe".to_string(),
        "mag::combat_report_production_hierarchy".to_string(),
    ];
    let worker = run_worker_suite(Backend::Skull, "mag", rock(), &entries)
        .expect("MAG BootImage product queue must run");
    assert_eq!(worker.len(), entries.len(), "one record per queued entry");
    for (entry, (record, crossed)) in entries.iter().zip(&worker) {
        eprintln!(
            "BootImage {entry}: {} crossed={crossed} checks={} failures={}",
            record.verdict(), record.expects_pass.len(), record.expects_fail.len()
        );
    }
    assert_eq!(
        worker.iter().filter(|(_, crossed)| *crossed).count(),
        entries.len() - 1,
        "both successor entries must cross a live worker boundary"
    );

    assert!(worker[1].1, "the NPC probe must cross the seed's live worker boundary");

    for (entry, (worker_record, _)) in entries.iter().zip(&worker) {
        assert_eq!(worker_record.verdict(), "PASS", "worker entry `{entry}` failed: {worker_record:?}");
        assert!(
            worker_record.errors.is_empty(),
            "worker entry `{entry}` emitted errors: {worker_record:?}"
        );
        assert!(
            !worker_record.expects_pass.is_empty(),
            "worker entry `{entry}` asserted no player-visible property"
        );

        let isolated = run_differential(
            Backend::Skull,
            "mag",
            rock(),
            &DifferentialPlan {
                predecessors: Vec::new(),
                target: entry.clone(),
            },
        )
        .unwrap_or_else(|err| panic!("fresh-process control `{entry}` failed: {err:#}"))
        .0;
        let mut isolated_for_compare = isolated.clone();
        let mut worker_for_compare = worker_record.clone();
        isolated_for_compare.entry_diagnostics =
            without_absent_agent_daemon_diagnostic(&isolated.entry_diagnostics);
        worker_for_compare.entry_diagnostics =
            without_absent_agent_daemon_diagnostic(&worker_record.entry_diagnostics);
        let comparison = compare(&isolated_for_compare, &worker_for_compare);
        assert!(
            comparison.is_equivalent(),
            "`{entry}` differs between fresh process and BootImage worker:\n{}",
            comparison.report()
        );
        assert!(
            comparison.population("expects_pass") > 0,
            "`{entry}` comparison had no assertion population"
        );
    }
}
