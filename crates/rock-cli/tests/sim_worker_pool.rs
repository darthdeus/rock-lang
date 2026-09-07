//! B5: boot-checkpoint workers composed with the bounded `-j` scheduler.
//!
//! Phase A runs fresh processes in parallel; Phase B runs many entries in one
//! booted process. B5 is both at once -- a bounded number of booted workers,
//! each serving one canonical boot signature. What has to survive the
//! composition is everything the two mechanisms already guarantee separately:
//! entry-granular parallelism, per-lane recovery, the exclusive hot-reload
//! lane, and positional result placement.
//!
//! Overlap is read off `ROCK_SIM_LIFECYCLE_LOG`, which the parent writes around
//! each child spawn. A worker emits a `start` for every entry it was handed
//! when it spawns and an `end` for each when it exits, so the count of live
//! entries at any point is the number of booted processes times their queue
//! length -- which is what makes "more entries live than one lane holds" a
//! statement about actual process overlap.
//!
//! Gated (compiles + runs a game): excluded from the default nextest profile.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

struct Run {
    stdout: String,
    stderr: String,
    success: bool,
    trace: String,
    _dir: tempfile::TempDir,
}

impl Run {
    fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// `rock sim <project> --workers <specs> -j <jobs>` with a lifecycle trace.
fn run_project_workers(project: &str, specs: &[&str], jobs: &str) -> Run {
    let dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = dir.path().join("lifecycle.log");

    let mut args = vec!["sim", project];
    args.extend_from_slice(specs);
    args.extend_from_slice(&["--workers", "-j", jobs]);

    let out = Command::new(rock_bin())
        .args(&args)
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .env("ROCK_SIM_LIFECYCLE_LOG", &trace_path)
        .env_remove("SIM_SKIP_RENDER")
        .output()
        .expect("run rock sim --workers");

    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
        trace,
        _dir: dir,
    }
}

fn run_workers(specs: &[&str], jobs: &str) -> Run {
    run_project_workers("testbed", specs, jobs)
}

/// The high-water mark of simultaneously live CHILD PROCESSES, plus how many
/// there were in total.
///
/// Processes and not entries: a worker is handed its whole queue at spawn, so
/// every entry it owns reads live for the worker's whole lifetime and an
/// entry-level peak cannot tell one long queue from several short concurrent
/// ones. `proc_start`/`proc_end` bracket the process itself.
fn peak_procs(trace: &str) -> (usize, usize) {
    let (mut live, mut peak, mut total) = (0isize, 0usize, 0usize);
    for line in trace.lines() {
        let (event, _name) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("malformed lifecycle event `{line}`"));
        match event {
            "proc_start" => {
                live += 1;
                total += 1;
                peak = peak.max(live as usize);
            }
            "proc_end" => {
                live -= 1;
                assert!(live >= 0, "a process ended without starting\n{trace}");
            }
            // Entry granularity; `the_reload_lane_runs_after_the_worker_pool_drains`
            // is what reads those.
            "start" | "end" => {}
            _ => panic!("unknown lifecycle event `{line}`"),
        }
    }
    assert_eq!(live, 0, "children left live\n{trace}");
    (peak, total)
}

/// Eight ordinary testbed sims that agree on their boot signature, so
/// `--workers` puts all eight in ONE group and the only thing that can split
/// them into concurrent processes is the `-j` composition.
const EIGHT: [&str; 8] = [
    "co_join2_tuple",
    "co_join3_tuple",
    "co_join_all_list",
    "co_field_poller",
    "co_wait_for_result",
    "b0_leak_probe",
    "b0_rng_probe",
    "b0_static_called_probe",
];

/// The composition itself: eight entries of one boot signature under `-j 4`
/// must run in several booted processes AT ONCE.
///
/// Two facts, and both are needed. The peak says the lanes overlapped rather
/// than ran back to back; the total says they are still WORKERS and not eight
/// fresh processes -- an implementation that gave up on sharing a boot would
/// also show a peak above one.
#[test]
fn several_booted_workers_run_at_once() {
    let run = run_workers(&EIGHT, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    assert!(
        run.stdout.contains(&format!("[sim] {}/{} sims passed", EIGHT.len(), EIGHT.len())),
        "all entries must be accounted for:\n{}",
        run.stdout
    );

    let (peak, total) = peak_procs(&run.trace);
    assert!(
        peak > 1,
        "a peak of {peak} live worker process(es) means the lanes ran one after \
         another instead of overlapping\n{}",
        run.trace
    );
    assert!(
        total < EIGHT.len(),
        "{total} processes for {} entries: the entries stopped sharing a boot, which \
         is the whole point of a worker\n{}",
        EIGHT.len(),
        run.trace
    );
}

#[test]
fn small_signature_groups_do_not_buy_singleton_boots() {
    let specs = [
        "b0_budget_pred",
        "b0_budget_probe",
        "b0_env_pred",
        "b0_env_probe",
    ];
    let run = run_workers(&specs, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    assert!(
        run.stdout.contains("[sim] 4/4 sims passed"),
        "all entries must be accounted for:\n{}",
        run.stdout
    );
    let result_lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("[sim] ") && line.contains(": PASS ("))
        .collect();
    assert_eq!(result_lines.len(), specs.len(), "every entry emitted a result:\n{}", run.stderr);
    assert!(
        result_lines.iter().all(|line| !line.contains("(0/0 expects passed)")),
        "the scheduling witness must contain real assertions:\n{}",
        run.stderr
    );

    let (peak, total) = peak_procs(&run.trace);
    assert!(peak > 1, "the independent groups must still overlap:\n{}", run.trace);
    assert_eq!(
        total, 2,
        "two compatible groups should use one two-entry worker each, not four singleton boots:\n{}",
        run.trace
    );
}

#[test]
fn omitted_skip_render_shares_with_explicit_default() {
    let specs = [
        "spell_damages",
        "target_cycle_sim",
        "cure_base_removes_one_poison_level",
        "cure_unpumped_still_uses_entity_targeting",
    ];
    let run = run_project_workers("mag", &specs, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    assert!(
        run.stdout.contains("[sim] 4/4 sims passed"),
        "all entries must be accounted for:\n{}",
        run.stdout
    );
    let result_lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("[sim] ") && line.contains(": PASS ("))
        .collect();
    assert_eq!(result_lines.len(), specs.len(), "every entry emitted a result:\n{}", run.stderr);
    assert!(
        result_lines.iter().all(|line| !line.contains("(0/0 expects passed)")),
        "the signature witness must contain real assertions:\n{}",
        run.stderr
    );

    let (peak, total) = peak_procs(&run.trace);
    assert_eq!(peak, 1, "the equivalent defaults should share one worker:\n{}", run.trace);
    assert_eq!(
        total, 1,
        "omitted and explicit SIM_SKIP_RENDER=1 should not split a boot:\n{}",
        run.trace
    );
}

#[test]
fn mag_world_restore_preserves_successor_boot_fonts() {
    for specs in [
        ["mag_live_run_world_image_rewinds", "direct_session_state_world_image_restores_the_authority"],
        ["world_environment_state_round_trips_through_world_image"; 2],
    ] {
        let run = run_project_workers("mag", &specs, "1");
        let output = run.combined();
        assert!(run.success, "world restores preserve gameplay state:\n{output}");
        assert!(run.stdout.contains("[sim] 2/2 sims passed"), "{output}");
        let results: Vec<_> = run.stderr.lines()
            .filter(|line| line.starts_with("[sim] ") && line.contains(": PASS ("))
            .collect();
        assert_eq!(results.len(), specs.len(), "{output}");
        assert!(results.iter().all(|line| !line.contains("(0/0 expects passed)")), "{output}");
        assert_eq!(peak_procs(&run.trace), (1, 1), "world restore must retain the worker's fonts:\n{output}\n{}", run.trace);
        assert!(!output.contains("worker_ineligible"), "{output}");
    }
}

#[test]
fn mag_run_stop_preserves_successor_boot_assets() {
    let specs = [
        "build_watermark_layout", "target_cycle_sim",
        "build_watermark_layout", "target_cycle_sim",
        "build_watermark_layout", "target_cycle_sim",
    ];
    let run = run_project_workers("mag", &specs, "1");
    assert!(run.success, "repeated run stops preserve successors:\n{}", run.combined());
    assert!(
        run.stdout.contains("[sim] 6/6 sims passed"),
        "all occurrences must be accounted for:\n{}",
        run.stdout
    );
    let result_lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("[sim] ") && line.contains(": PASS ("))
        .collect();
    assert_eq!(result_lines.len(), specs.len(), "every entry emitted a result:\n{}", run.stderr);
    assert!(
        result_lines.iter().all(|line| !line.contains("(0/0 expects passed)")),
        "the ownership witness must contain real assertions:\n{}",
        run.stderr
    );
    let (peak, total) = peak_procs(&run.trace);
    assert_eq!(peak, 1, "-j 1 keeps the recovery serial:\n{}", run.trace);
    assert_eq!(
        total, 1,
        "a run stop must preserve boot-owned images for the successor:\n{}",
        run.trace
    );

}

/// `-j 1` is the serial path, and it has to STAY the serial path: the composed
/// scheduler must not overlap work the user asked not to overlap. This is the
/// control for the arm above -- without it, a peak above one would be evidence
/// only that something ran, not that `-j` chose it.
#[test]
fn one_job_still_runs_one_worker_at_a_time() {
    let run = run_workers(&EIGHT, "1");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    let (peak, total) = peak_procs(&run.trace);
    assert_eq!(peak, 1, "at -j 1 nothing overlaps\n{}", run.trace);
    assert_eq!(
        total, 1,
        "and the whole group is ONE worker queue, so one process serves all eight\n{}",
        run.trace
    );
}

/// A worker death is contained by its own lane. `b2_crash_head` kills the
/// process inside entry 0, so its lane loses the worker and reruns what never
/// started -- and every other lane, which is a different process, must be
/// untouched.
#[test]
fn a_dead_worker_does_not_reach_another_lane() {
    let specs = [
        "b2_crash_head,b2_crash_tail",
        "b0_leak_probe",
        "co_join2_tuple",
        "co_join3_tuple",
        "co_join_all_list",
        "co_field_poller",
    ];
    let run = run_workers(&specs, "3");
    let combined = run.combined();

    assert!(
        !run.success,
        "the crash entry fails, so the invocation fails:\n{combined}"
    );
    // The neighbours are the point: each keeps its own PASS record.
    for name in ["b0_leak_probe", "co_join2_tuple", "co_join3_tuple", "co_join_all_list", "co_field_poller"] {
        assert!(
            combined.contains(&format!("{name}: PASS")),
            "`{name}` shares no process with the crash and must still pass:\n{combined}"
        );
    }
    assert!(
        run.stdout.contains("[sim] 5/6 sims passed -- FAILED: b2_crash_head,b2_crash_tail"),
        "exactly the crash entry fails, and it is the one named:\n{}",
        run.stdout
    );
    let (peak, _) = peak_procs(&run.trace);
    assert!(peak > 1, "the lanes have to have actually overlapped\n{}", run.trace);
}

/// The hot-reload lane stays exclusive under the composed scheduler. It drives
/// the one live compile session and the one `incremental.lua`, so it may not
/// overlap a worker -- and `--workers` is precisely where a booted process
/// could be left running beside it.
#[test]
fn the_reload_lane_runs_after_the_worker_pool_drains() {
    const RELOAD: &str = "reload_probe";
    let mut specs = vec![RELOAD];
    specs.extend_from_slice(&EIGHT);
    let run = run_workers(&specs, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());

    let mut live: HashSet<&str> = HashSet::new();
    let mut reload_live = false;
    let mut saw_reload = false;
    for line in run.trace.lines() {
        let (event, name) = line.split_once('\t').expect("lifecycle event");
        if event == "proc_start" || event == "proc_end" {
            continue; // process granularity; this property is about entries
        }
        match (event, name == RELOAD) {
            ("start", true) => {
                assert!(
                    live.is_empty(),
                    "the reload child started while worker entries were live: {live:?}\n{}",
                    run.trace
                );
                saw_reload = true;
                reload_live = true;
            }
            ("start", false) => {
                assert!(!reload_live, "`{name}` started while reload was live\n{}", run.trace);
                live.insert(name);
            }
            ("end", true) => reload_live = false,
            ("end", false) => {
                live.remove(name);
            }
            _ => unreachable!(),
        }
    }
    assert!(saw_reload, "the reload child never ran\n{}", run.trace);
}

/// Result PLACEMENT survives the pool. Naming one sim twice is two entries with
/// two results, and records are emitted in spec order however the lanes
/// finished -- completion order under a pool is not deterministic, so it cannot
/// be the reporting order.
#[test]
fn results_stay_positional_and_ordered_under_the_pool() {
    let specs = ["b0_rng_probe", "b0_leak_probe", "b0_rng_probe", "co_join2_tuple"];
    let run = run_workers(&specs, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    assert!(
        run.stdout.contains("[sim] 4/4 sims passed"),
        "each occurrence is its own entry:\n{}",
        run.stdout
    );
    assert_eq!(
        run.stderr.matches("b0_rng_probe: PASS").count(),
        2,
        "the duplicated spec owns two records:\n{}",
        run.stderr
    );

    let order: Vec<&str> = run
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[sim] "))
        .filter_map(|l| l.split_once(':'))
        .map(|(name, _)| name)
        .filter(|n| specs.contains(n))
        .collect();
    assert_eq!(
        order,
        specs.to_vec(),
        "records are emitted in spec order:\n{}",
        run.stderr
    );
}
