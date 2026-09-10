use rock_cli::run_sim::{run_differential, run_worker_suite, DifferentialPlan};
use rock_cli::sim_record::compare;
use rock_cli::Backend;

#[test]
fn mag_worker_reset_discards_unfinished_save() {
    let entries = vec![
        "async_save_worker_reset_seed".to_string(),
        "async_save_worker_reset_probe".to_string(),
    ];
    let worker = run_worker_suite(Backend::Skull, "mag", &entries)
        .expect("save reset worker queue must run");
    assert_eq!(worker.len(), 2, "both save reset entries must run");
    assert!(worker[1].1, "probe must cross the seed's live worker boundary");
    for (entry, (record, _)) in entries.iter().zip(&worker) {
        assert_eq!(record.verdict(), "PASS", "{entry}: {record:?}");
        assert!(record.errors.is_empty(), "{entry}: {record:?}");
        assert!(!record.expects_pass.is_empty(), "{entry} asserted nothing");
    }
}

#[test]
fn mag_bootimage_queue_isolates_scene_and_gameplay_entries() {
    let entries = vec![
        "boot_image_scene_registry_npcs_seed".to_string(),
        "boot_image_scene_registry_npcs_probe".to_string(),
        "spell_damages".to_string(),
    ];
    let worker = run_worker_suite(Backend::Skull, "mag", &entries)
        .expect("MAG BootImage product queue must run");
    assert_eq!(worker.len(), entries.len(), "one record per queued entry");
    assert_eq!(
        worker.iter().filter(|(_, crossed)| *crossed).count(),
        2,
        "both successor entries must cross a live worker boundary"
    );

    for (entry, (worker_record, _)) in entries.iter().zip(&worker) {
        assert_eq!(worker_record.verdict(), "PASS", "worker entry `{entry}` failed");
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
            &DifferentialPlan {
                predecessors: Vec::new(),
                target: entry.clone(),
            },
        )
        .unwrap_or_else(|err| panic!("fresh-process control `{entry}` failed: {err:#}"))
        .0;
        let comparison = compare(&isolated, worker_record);
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
