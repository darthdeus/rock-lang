use super::*;

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

type JobsCase = (Option<usize>, bool, usize);

fn assert_jobs(cores: usize, budget: JobBudget, cases: &[JobsCase]) {
    for &(request, update, expected) in cases {
        assert_eq!(
            resolve_jobs_with(request, update, cores, budget).unwrap(),
            expected,
            "request {request:?}, update {update}"
        );
    }
}

fn stopped() -> SimSummary {
    SimSummary {
        saw_stop: true,
        ..SimSummary::default()
    }
}

fn run_with_production_emitter(body: &str) -> std::process::Output {
    let production = std::fs::read_to_string(
        rock_stdlib::package_dir("stone")
            .unwrap()
            .join("engine/sim.rock"),
    )
    .unwrap();
    let emitter = production
        .split_once("inline(lua) fn _sim_emit_impl(")
        .unwrap()
        .1
        .split_once("\"\"\"")
        .unwrap()
        .1
        .split_once("\"\"\"")
        .unwrap()
        .0;
    let script = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        script.path(),
        format!("local function emit(json_line, site)\n{emitter}\nend\n{body}\n"),
    )
    .unwrap();
    std::process::Command::new(rock_host::project::luajit_path())
        .arg(script.path())
        .env("STONE_HEADLESS", "1")
        .output()
        .unwrap()
}

#[cfg(target_os = "linux")]
extern "C" fn block_in_shared_address_space(_: *mut libc::c_void) -> libc::c_int {
    // Raw syscall only: this child runs on the test's heap and TLS.
    loop {
        unsafe {
            libc::syscall(
                libc::SYS_ppoll,
                std::ptr::null_mut::<libc::pollfd>(),
                0usize,
                std::ptr::null::<libc::timespec>(),
                std::ptr::null::<libc::sigset_t>(),
            );
        }
    }
}

/// `spawn` can return while the child is still inside execve on the
/// runner's address space; measured then, a fat runner reads as a fat
/// child and the cap kills a healthy sim worker before its first entry.
#[cfg(target_os = "linux")]
#[test]
fn a_fat_runner_does_not_trip_the_cap_of_a_small_child() {
    let ballast = vec![1u8; 512 << 20];
    let mut stack = vec![0u8; 256 << 10];
    let stack_top = (stack.as_mut_ptr() as usize + stack.len()) & !15;
    let pid = unsafe {
        libc::clone(
            block_in_shared_address_space,
            stack_top as *mut libc::c_void,
            libc::CLONE_VM | libc::SIGCHLD,
            std::ptr::null_mut(),
        )
    };
    assert!(pid > 0, "clone: {}", std::io::Error::last_os_error());
    let measured = process_memory_mb(pid as u32);
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    std::hint::black_box(&ballast);
    assert_eq!(
        measured, None,
        "a child on the runner's address space was measured as its own"
    );

    // Killed below; it must outlive the wait for its own exec.
    let mut small = std::process::Command::new("sleep")
        .arg("3600")
        .spawn()
        .expect("spawn sleep");
    let deadline = rock_core::clock::Instant::now() + test_utils::HANG_BACKSTOP;
    let measured = loop {
        if let Some(mb) = process_memory_mb(small.id()) {
            break Some(mb);
        }
        if rock_core::clock::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = small.kill();
    let _ = small.wait();
    let mb = measured.expect("an exec'd child is measured");
    assert!(mb < 256, "sleep measured at {mb} MB");
}

#[test]
fn sim_compile_child_uses_the_runner_output_directory() {
    const HELPER: &str = "ROCK_TEST_RECEIPT_RELEASE_HELPER";
    if let Some(root) = std::env::var_os(HELPER) {
        let root = PathBuf::from(root);
        do_list_sims(&root.join("project").to_string_lossy()).expect("compile sim registry");
        return;
    }
    let root = tempfile::tempdir().expect("receipt fixture");
    std::fs::create_dir(root.path().join("project")).unwrap();
    std::fs::write(root.path().join("tmp"), "no second output directory").unwrap();
    std::fs::create_dir(root.path().join("receipts")).unwrap();
    std::fs::write(
        root.path().join("project/main.rock"),
        "@nomangle fn answer() -> int { 41 }\n",
    )
    .unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "run_sim::tests::sim_compile_child_uses_the_runner_output_directory",
            "--nocapture",
        ])
        .current_dir(root.path())
        .env(HELPER, root.path())
        .env(SIM_COMPILE_RECEIPT_DIR_ENV, root.path().join("receipts"))
        .output()
        .expect("receipt test child");
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let registry = std::fs::read(root.path().join("receipts").join(SIM_REGISTRY_FILENAME))
        .expect("the child writes the sim registry");
    assert!(serde_json::from_slice::<Vec<RegisteredSim>>(&registry).is_ok());
    assert!(
        root.path()
            .join("receipts")
            .join(rock_host::project::MAIN_LUA_FILENAME)
            .is_file()
    );
}

#[test]
fn receipt_sweep_reclaims_dead_pid_roots_and_keeps_live_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let dead = tmp.path().join(format!("sim-receipt-{}-stale", u32::MAX));
    let live = tmp
        .path()
        .join(format!("sim-receipt-{}-running", std::process::id()));
    let legacy = tmp.path().join("sim-receipt-nopid");
    let other = tmp.path().join("sim-output-keep");
    for dir in [&dead, &live, &legacy, &other] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(dead.join("receipt.json"), "{}").unwrap();
    sweep_stale_sim_receipt_roots(tmp.path());
    assert!(live.is_dir(), "a concurrent live compile keeps its root");
    assert!(
        legacy.is_dir(),
        "an unparseable fresh root keeps the safe direction"
    );
    assert!(other.is_dir(), "outside the prefix nothing is touched");
    #[cfg(unix)]
    assert!(!dead.exists(), "a dead runner's root is reclaimed");
    #[cfg(not(unix))]
    assert!(
        dead.is_dir(),
        "without a liveness probe the safe direction keeps"
    );
}

#[test]
fn reload_lane_reuses_the_compiled_session_and_isolates_its_chunk() {
    let project = tempfile::tempdir().expect("project");
    let shared = tempfile::tempdir().expect("shared output");
    let source = project.path().join("main.rock");
    std::fs::write(&source, "@nomangle fn answer() -> int { 41 }\n").unwrap();
    let main = shared.path().join(rock_host::project::MAIN_LUA_FILENAME);
    let stone = crate::skull_backend::IncrementalProject::init_with_tests(
        project.path().to_str().unwrap(),
        &main,
        &shared.path().join("incremental.lua"),
    )
    .expect("compile once");
    let original = std::fs::read(&main).unwrap();
    let fingerprint = stone.artifact_fingerprint().unwrap();
    std::fs::remove_file(&source).unwrap();
    let lane = reload_lane_session(project.path(), true, shared.path(), Some(stone), true)
        .expect("reuse without reading source")
        .expect("selected lane");
    assert_eq!(lane.stone.artifact_fingerprint().unwrap(), fingerprint);
    let lane_main = Path::new(&lane.output).join(rock_host::project::MAIN_LUA_FILENAME);
    assert_eq!(std::fs::read(&lane_main).unwrap(), original);
    std::fs::write(&lane_main, "fixture rewrite").unwrap();
    assert_eq!(std::fs::read(&main).unwrap(), original);
}

#[test]
fn every_sim_child_stubs_its_assets_unless_asked_not_to() {
    assert_eq!(unstub_from(None), "1", "unset stays stubbed");
    assert_eq!(unstub_from(Some("0")), "0", "an inherited 0 unstubs");
    assert_eq!(
        unstub_from(Some("1")),
        "1",
        "an inherited 1 is the default anyway"
    );
    assert_eq!(
        unstub_from(Some("yes")),
        "1",
        "anything else is not an unstub request"
    );

    if std::env::var("STONE_ASSET_STUB").is_err() {
        assert_eq!(asset_stub_value(), "1", "the runner default is stubbed");
    }
}

#[test]
fn rendering_is_the_effective_last_wins_value() {
    let cases: &[(&[(&str, &str)], bool, &str)] = &[
        (&[], false, "declaring nothing does not render"),
        (&[("SIM_DRAW", "on_demand")], true, "@sim_render sugar"),
        (&[("SIM_DRAW", "none")], false, "explicit skip"),
        (
            &[("SIM_DRAW", "commands")],
            false,
            "submit needs no GPU result",
        ),
        (
            &[("SIM_DRAW", "every_frame")],
            true,
            "continuous GPU rendering",
        ),
        (
            &[("SIM_DRAW", "on_demand"), ("SIM_DRAW", "none")],
            false,
            "a later explicit @sim_env overrides the @sim_render sugar",
        ),
        (
            &[("SIM_DRAW", "none"), ("SIM_DRAW", "on_demand")],
            true,
            "... and the other way round",
        ),
        (
            &[("GAME_FORCE_VFX", "1")],
            false,
            "an unrelated key is not it",
        ),
    ];
    for (declared, renders, why) in cases {
        assert_eq!(entry_renders(&env(declared)), *renders, "{why}");
    }
}

#[test]
fn a_missing_image_golden_is_not_a_pass() {
    let dir = tempfile::tempdir().expect("tmp dir");
    let produced = dir.path().join("produced.png");
    // A 1x1 PNG; the bytes never get compared on the paths under test.
    std::fs::write(&produced, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).expect("write");
    let baseline = dir.path().join("golden.png");
    let img = ImageSnapshotOp {
        name: "golden".into(),
        path: produced.to_string_lossy().into_owned(),
        tol: 0.001,
        save_ok: true,
        site: String::new(),
    };

    let r = handle_image_snapshot(&img, &baseline, false).expect("no hard error");
    assert!(
        matches!(r, SnapResult::Fail(_)),
        "a missing golden must not pass; got {r:?}"
    );
    assert!(
        !baseline.exists(),
        "and it must NOT have been written behind our back"
    );

    let r = handle_image_snapshot(&img, &baseline, true).expect("no hard error");
    assert!(
        matches!(r, SnapResult::Wrote),
        "update writes the baseline; got {r:?}"
    );
    assert!(baseline.exists(), "the baseline is created under update");
}

#[test]
fn reload_lane_is_opt_in_via_stone_hot_reload() {
    let cases: &[(&[(&str, &str)], bool, &str)] = &[
        (&[("STONE_HOT_RELOAD", "1")], true, "the opt-in key"),
        (
            &[("SIM_DRAW", "on_demand")],
            false,
            "a busy sim env does not route",
        ),
        (&[], false, "no env"),
        (&[("ROCK_TEST_SEED", "42")], false, "an ordinary entry"),
        (
            &[("STONE_HOT_RELOAD", "0")],
            false,
            "an explicit disable: the child's watcher never polls",
        ),
        (
            &[("ROCK_TEST_SEED", "42"), ("STONE_HOT_RELOAD", "1")],
            true,
            "the key routes when it is not the only pair",
        ),
    ];
    for (declared, lane, why) in cases {
        let declared = env(declared);
        assert_eq!(is_reload_lane(&declared), *lane, "{why}");
        let registry = SimRegistry::from(vec![RegisteredSim {
            wire_name: "probe".into(),
            env: declared,
            source_file: "probe.rock".into(),
        }]);
        assert_eq!(registry.reloads("probe"), *lane, "{why}");
        assert!(!registry.reloads("unknown"));
    }
}

/// The reload lane's exclusion from the bytecode image is a correctness
/// requirement, not a tuning choice: `check_hot_reload` derives the overlay
/// path from the running script's own path by replacing `main.lua` with
/// `incremental.lua` (`stone/engine/hot_reload.rock:87`), and
/// `read_chunk_source_dir` (`:43`) reads line 1 of a chunk AS TEXT to decide
/// whether it belongs to this game. Both questions are unanswerable against
/// a bytecode image, so a reload child booted from one would silently stop
/// watching -- a reload sim would pass every assertion and observe nothing.
#[test]
fn only_a_non_reload_worker_boots_from_the_bytecode_image() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let main = dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let bytecode = dir.join(rock_host::project::MAIN_BYTECODE_FILENAME);
    std::fs::write(&main, "-- source").unwrap();

    assert_eq!(worker_chunk_path(dir, false), main);
    assert_eq!(worker_chunk_path(dir, true), main);

    std::fs::write(&bytecode, "-- image").unwrap();
    assert_eq!(worker_chunk_path(dir, false), bytecode);
    assert_eq!(
        worker_chunk_path(dir, true),
        main,
        "a reload child must keep the source path it derives the overlay from"
    );
}

#[test]
fn resolve_full_asset_jobs_is_bounded_by_jobs_and_defaults_to_two() {
    assert_eq!(resolve_full_asset_jobs(None, 12), DEFAULT_FULL_ASSET_JOBS);
    assert_eq!(resolve_full_asset_jobs(None, 1), 1);
    assert_eq!(resolve_full_asset_jobs(Some(4), 12), 4);
    assert_eq!(resolve_full_asset_jobs(Some(40), 12), 12);
    assert_eq!(resolve_full_asset_jobs(Some(0), 12), 1);
    assert_eq!(resolve_full_asset_jobs(Some(3), 1), 1);
}

#[test]
fn the_workload_env_applies_defaults_last_wins_and_keeps_out_literal() {
    assert_eq!(
        workload_env(&[]),
        workload_env(&env(&[("SIM_DRAW", "none")])),
        "omitting a runner default and declaring it are the same workload"
    );
    assert_ne!(
        workload_env(&[]),
        workload_env(&env(&[("SIM_DRAW", "on_demand")]))
    );
    let last = workload_env(&env(&[("SIM_DRAW", "none"), ("SIM_DRAW", "on_demand")]));
    assert!(last.contains(&("SIM_DRAW".into(), "on_demand".into())));
    let out = workload_env(&env(&[("GAME_HISTORY_DIR", "$OUT/history")]));
    assert!(out.contains(&("GAME_HISTORY_DIR".into(), "$OUT/history".into())));
}

#[test]
fn entry_full_assets_needs_real_assets_and_no_image_cap() {
    const STUB_OFF: (&str, &str) = ("STONE_ASSET_STUB", "0");
    const UNCAPPED: (&str, &str) = ("STONE_IMAGE_MAX_SIDE", "0");
    let cases: &[(&[(&str, &str)], &str, bool, &str)] = &[
        (&[], "1", false, "declaring nothing is stubbed and capped"),
        (&[STUB_OFF], "1", false, "real assets alone stay capped"),
        (&[UNCAPPED], "1", false, "an uncapped stub is one texel"),
        (&[STUB_OFF, UNCAPPED], "1", true, "the pair marks the lane"),
        (
            &[UNCAPPED, ("SIM_DRAW", "commands")],
            "0",
            true,
            "an inherited STONE_ASSET_STUB=0 counts, and submit is not consulted",
        ),
        (
            &[STUB_OFF, UNCAPPED, ("STONE_IMAGE_MAX_SIDE", "512")],
            "1",
            false,
            "last declaration wins",
        ),
    ];
    for (declared, inherited, full, why) in cases {
        assert_eq!(entry_full_assets(&env(declared), inherited), *full, "{why}");
    }
}

#[test]
fn the_classed_pool_caps_live_capped_tasks_and_runs_everything_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let renders: Vec<bool> = (0..40).map(|i| i % 3 == 0).collect();
    let live = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let results = SimPool::new(8, renders.len(), 2, &renders, false).run(|i| {
        if renders[i] {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(3));
            live.fetch_sub(1, Ordering::SeqCst);
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok::<usize, anyhow::Error>(i)
    });
    let mut seen: Vec<usize> = results.iter().map(|(i, _)| *i).collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..40).collect::<Vec<_>>(), "every task exactly once");
    assert!(
        results
            .iter()
            .all(|(i, r)| r.as_ref().map(|v| v == i).unwrap_or(false))
    );
    assert!(
        peak.load(Ordering::SeqCst) <= 2,
        "peak live rendering {}",
        peak.load(Ordering::SeqCst)
    );
    assert!(peak.load(Ordering::SeqCst) >= 1);
}

#[test]
fn a_panicking_sim_releases_its_full_asset_slot() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (finished, outcome) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let second_ran = AtomicBool::new(false);
        let panic = std::panic::catch_unwind(|| {
            SimPool::new(2, 2, 1, &[true; 2], false).run(|i| {
                assert_ne!(i, 0, "worker panic");
                second_ran.store(true, Ordering::Relaxed);
            });
        });
        finished
            .send((panic.is_err(), second_ran.into_inner()))
            .unwrap();
    });
    assert_eq!(
        outcome
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("a panicking worker must release the slot so the pool can finish"),
        (true, true)
    );
}

#[test]
fn the_pool_takes_the_reload_lanes_slot_back_when_the_lane_finishes() {
    let capped: Vec<bool> = (0..60).map(|i| i % 4 == 0).collect();
    reload_lane_slot_returns(3, &[]);
    reload_lane_slot_returns(1, &capped);
}

fn reload_lane_slot_returns(full_asset_jobs: usize, full: &[bool]) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let deadline = rock_core::clock::Instant::now() + test_utils::HANG_BACKSTOP;
    let wait_for = |done: &dyn Fn() -> bool| {
        while !done() && rock_core::clock::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    let live = AtomicUsize::new(0);
    let peak_beside_lane = AtomicUsize::new(0);
    let peak_after_lane = AtomicUsize::new(0);
    let lane_done = AtomicBool::new(false);
    let task = |i: usize| {
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        if lane_done.load(Ordering::SeqCst) {
            peak_after_lane.fetch_max(now, Ordering::SeqCst);
            wait_for(&|| peak_after_lane.load(Ordering::SeqCst) >= 3);
        } else {
            peak_beside_lane.fetch_max(now, Ordering::SeqCst);
            wait_for(&|| lane_done.load(Ordering::SeqCst));
        }
        live.fetch_sub(1, Ordering::SeqCst);
        Ok::<usize, anyhow::Error>(i)
    };
    let (pool, ()) = SimPool::new(3, 60, full_asset_jobs, full, true).beside_reload(task, || {
        wait_for(&|| live.load(Ordering::SeqCst) >= 2);
        std::thread::sleep(std::time::Duration::from_millis(40));
        lane_done.store(true, Ordering::SeqCst);
    });
    assert_eq!(pool.len(), 60, "every task exactly once");
    assert_eq!(
        peak_beside_lane.load(Ordering::SeqCst),
        2,
        "the lane holds one of three jobs while it runs"
    );
    assert_eq!(
        peak_after_lane.load(Ordering::SeqCst),
        3,
        "the pool widens to every job once the lane is done"
    );
}

#[test]
fn lanes_dispatch_longest_first_with_untimed_lanes_last_in_plan_order() {
    let estimates = [
        Some(1_000),
        None,
        Some(30_000),
        Some(1_000),
        None,
        Some(8_000),
    ];
    assert_eq!(longest_first(&estimates), vec![2, 5, 0, 3, 1, 4]);
}

#[test]
fn resolve_jobs_is_bounded_and_serial_for_snapshot_updates() {
    let (cores, budget) = (32, JobBudget { ram: 4, gpu: 4 });
    assert_jobs(
        cores,
        budget,
        &[
            (None, false, 4),
            (Some(1), false, 1),
            (Some(4), false, 4),
            (Some(0), false, 1),
            (None, true, 1),
            (Some(8), true, 1),
        ],
    );
    let error = resolve_jobs_with(Some(10), false, cores, budget).unwrap_err();
    assert!(error.to_string().contains("RAM budget of 4"));
}

#[test]
fn a_request_above_only_the_free_gpu_budget_runs_at_that_budget() {
    let (cores, budget) = (32, JobBudget { ram: 10, gpu: 2 });
    assert_jobs(
        cores,
        budget,
        &[(Some(8), false, 2), (Some(10), false, 2), (None, false, 2)],
    );
    let error = resolve_jobs_with(Some(11), false, cores, budget).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("--jobs 11 exceeds the RAM budget of 10"),
        "{error}"
    );
}

#[test]
fn a_request_above_the_known_lanes_is_narrowed_before_the_budget() {
    let (cores, budget) = (32, JobBudget { ram: 1, gpu: 1 });
    assert_eq!(
        resolve_jobs_with(narrow_to_lanes(Some(4), Some(1)), false, cores, budget).unwrap(),
        1
    );
    let error =
        resolve_jobs_with(narrow_to_lanes(Some(4), Some(2)), false, cores, budget).unwrap_err();
    assert!(error.to_string().contains("--jobs 2 exceeds"), "{error}");
    assert_eq!(narrow_to_lanes(Some(4), None), Some(4));
    assert_eq!(narrow_to_lanes(None, Some(2)), None);
    assert_eq!(narrow_to_lanes(Some(0), Some(2)), Some(0));
}

#[test]
fn the_default_job_count_leaves_a_fixed_gpu_memory_reserve() {
    assert_eq!(
        vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB + 4 * SIM_LANE_VRAM_MIB + 1)),
        4
    );
    assert_eq!(
        vram_budgeted_jobs(2, Some(SIM_VRAM_RESERVE_MIB + 64 * SIM_LANE_VRAM_MIB)),
        2
    );
    assert_eq!(vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB)), 1);
    assert_eq!(vram_budgeted_jobs(32, Some(SIM_LANE_VRAM_MIB / 4)), 1);
    assert_eq!(vram_budgeted_jobs(14, None), 14);
}

#[test]
fn default_jobs_fit_ram_and_gpu_budgets() {
    let studio = memory_budgets(14, Some(36 * 1024), None);
    assert_eq!(studio.lanes(), 10);
    assert_jobs(14, studio, &[(None, false, 10), (Some(14), true, 1)]);
    assert_eq!(memory_budgets(32, Some(62 * 1024), Some(17851)).lanes(), 5);
    assert_eq!(memory_budgets(32, Some(62 * 1024), None).lanes(), 17);
    assert_eq!(memory_budgets(2, Some(36 * 1024), None).lanes(), 2);
    assert_eq!(memory_budgets(14, Some(0), None).lanes(), 1);
    assert_eq!(memory_budgets(0, Some(36 * 1024), None).lanes(), 1);
    assert_eq!(memory_budgets(14, None, None).lanes(), 14);
}

#[test]
fn nvidia_smi_free_memory_reads_the_first_gpu() {
    assert_eq!(parse_nvidia_smi_free_mib("19115\n"), Some(19115));
    assert_eq!(parse_nvidia_smi_free_mib(" 19115\n4000\n"), Some(19115));
    assert_eq!(parse_nvidia_smi_free_mib(""), None);
    assert_eq!(parse_nvidia_smi_free_mib("[N/A]\n"), None);
}

#[test]
fn a_declared_frame_count_is_the_entrys_budget() {
    const COUNT: &str = "FRAME_COUNT";
    let cases: &[(&[(&str, &str)], usize)] = &[
        (&[], DEFAULT_FRAME_BUDGET),
        (&[(COUNT, "90")], 90),
        (&[(COUNT, "100000")], 100000),
        (&[("STONE_FX_OFF", "1")], DEFAULT_FRAME_BUDGET),
        (&[(COUNT, "80"), (COUNT, "90")], 90),
        (&[(COUNT, "bad"), (COUNT, "90")], 90),
    ];
    for (declared, budget) in cases {
        assert_eq!(
            frame_budget_for("s", &env(declared)).expect("valid"),
            *budget,
            "{declared:?}"
        );
    }
    assert!(
        frame_budget_for("s", &env(&[(COUNT, "90"), (COUNT, "bad")])).is_err(),
        "the effective last declaration is the one that must be validated"
    );
}

#[test]
fn a_declared_timeout_is_the_entrys_wall_budget() {
    const KEY: &str = "ROCK_SIM_TIMEOUT_SECS";
    let cases: &[(&[(&str, &str)], u64)] = &[
        (&[], sim_timeout_secs()),
        (&[(KEY, "1200")], 1200),
        (&[("FRAME_COUNT", "90")], sim_timeout_secs()),
        (&[(KEY, "90"), (KEY, "1200")], 1200),
    ];
    for (declared, secs) in cases {
        assert_eq!(
            timeout_for("s", &env(declared)).expect("valid"),
            Duration::from_secs(*secs),
            "{declared:?}"
        );
    }
}

#[test]
fn an_unusable_timeout_is_rejected_not_defaulted() {
    for bad in ["", "0", "-1", "abc", "1.5"] {
        assert!(
            timeout_for("s", &env(&[("ROCK_SIM_TIMEOUT_SECS", bad)])).is_err(),
            "`ROCK_SIM_TIMEOUT_SECS={bad}` must reject rather than silently become the default"
        );
    }
}

#[test]
fn an_unusable_frame_count_is_rejected_not_defaulted() {
    for bad in ["abc", "", "0", "-5", "12.5"] {
        assert!(
            frame_budget_for("s", &env(&[("FRAME_COUNT", bad)])).is_err(),
            "`FRAME_COUNT={bad}` must reject rather than silently become the default"
        );
    }
}

fn lines(raw: &[&str]) -> Vec<String> {
    raw.iter().map(|s| s.to_string()).collect()
}

fn entry_stream(name: &str, lines: impl IntoIterator<Item = String>) -> EntryStream {
    let mut stream = EntryStream::new(name.to_owned(), true);
    for line in lines {
        stream.observe(&line, |_, _| {});
    }
    stream
}

fn checked_summary(stderr: &str) -> SimSummary {
    let stream = entry_stream(
        "fixture",
        std::iter::once(start("fixture"))
            .chain(stderr.lines().map(str::to_owned))
            .chain(std::iter::once(end("fixture"))),
    );
    assert!(
        stream.protocol_error.is_none(),
        "{:?}",
        stream.protocol_error
    );
    stream.summary
}

#[test]
fn the_deadline_guard_resets_only_on_the_accepted_start() {
    for first in [
        start("a"),
        expect_ok("orphan"),
        "__SIM__ invalid".to_owned(),
    ] {
        let mut stream = EntryStream::new("a".to_string(), false);
        assert_eq!(
            matches!(
                stream.observe(&first, |_, _| {}).boundary,
                Some(EntryBoundary::Started)
            ),
            first == start("a")
        );
        for line in [expect_ok("ordinary op"), "raw log".to_owned(), start("a")] {
            assert!(!matches!(
                stream.observe(&line, |_, _| {}).boundary,
                Some(EntryBoundary::Started)
            ));
        }
        assert!(stream.protocol_error.is_some());
    }
}

#[test]
fn entry_frame_timing_is_validated_and_retained() {
    let mut stream = entry_stream("a", [start("a")]);
    let valid = end_with_frame_timing(
        "a",
        3,
        r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
    );
    assert!(matches!(
        stream.observe(&valid, |_, _| {}).boundary,
        Some(EntryBoundary::Ended(
            Some(3),
            Some(FrameTiming {
                frames: 3,
                draw_frames: 2,
                submitted_frames: 1,
                logic_us: 10,
                render_cpu_us: 20,
                present_us: 30,
            }),
        ))
    ));
    assert_eq!(stream.summary.frame_count, Some(3));

    for timing in [
        r#"{"frames":3,"draw_frames":4,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
        r#"{"frames":2,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
        r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":-1,"render_cpu_us":20,"present_us":30}"#,
        r#"[3,2,1,10,20,30]"#,
    ] {
        let stream = entry_stream("a", [start("a"), end_with_frame_timing("a", 3, timing)]);
        assert!(
            stream.protocol_error.is_some(),
            "invalid frame timing must reject: {timing}"
        );
    }
}

fn start(name: &str) -> String {
    format!(r#"__SIM__ {{"op":"entry_start","name":"{name}"}}"#)
}

fn end(name: &str) -> String {
    format!(r#"__SIM__ {{"op":"entry_end","name":"{name}"}}"#)
}

fn end_with_frame_count(name: &str, frame_count: &str) -> String {
    format!(r#"__SIM__ {{"op":"entry_end","name":"{name}","frame_count":{frame_count}}}"#)
}

fn end_with_frame_timing(name: &str, frame_count: usize, timing: &str) -> String {
    format!(
        r#"__SIM__ {{"op":"entry_end","name":"{name}","frame_count":{frame_count},"frame_timing":{timing}}}"#
    )
}

fn expect_ok(msg: &str) -> String {
    format!(r#"__SIM__ {{"op":"expect_ok","message":"{msg}","site":"f.rock:1"}}"#)
}

#[test]
fn the_envelope_partitions_operations_and_logging() {
    let stream = vec![
        "[   0.1s INFO ] booting".to_string(),
        start("a"),
        "[   0.2s INFO ] inside a".to_string(),
        expect_ok("from a"),
        end_with_frame_count("a", "7"),
        "[   0.4s INFO ] teardown".to_string(),
    ];

    let out = entry_stream("a", stream);
    assert!(out.protocol_error.is_none());
    assert_eq!(out.summary.expects_pass[0].message, "from a");
    assert!(out.started && out.terminated);
    assert_eq!(out.summary.frame_count, Some(7));
    assert!(out.summary.diagnostics.contains("booting"));
    assert!(out.summary.diagnostics.contains("teardown"));
}

#[test]
fn a_killed_child_leaves_its_entry_unterminated() {
    let out = entry_stream("a", [start("a"), expect_ok("partial")]);
    assert!(out.started && !out.terminated);
    assert!(out.protocol_error.is_none());
    assert_eq!(out.summary.expects_pass[0].message, "partial");
}

#[test]
fn live_and_buffered_assertions_use_the_same_message_and_site() {
    let assertion = r#"__SIM__ {"op":"expect_fail","site":"project/tests/sims/sample.rock:9","message":"count","actual":"3","expected":"4"}"#;
    let buffered = checked_summary(assertion);
    let failure = &buffered.expects_fail[0];
    assert_eq!(failure.message, "count");
    assert_eq!(failure.site, "sample:9");
    let mut live = EntryStream::new("fixture".into(), false);
    live.observe(&start("fixture"), |_, _| {});
    live.observe(assertion, |_, event| {
        let progress = event.unwrap().progress().unwrap();
        assert!(progress.contains(&failure.site) && progress.contains(&failure.message));
        for (line, expected) in progress.lines().zip(["FAIL", "3", "4"]) {
            assert!(line.contains(expected), "{progress}");
        }
        assert_eq!(progress.lines().count(), 3);
    });
    assert_eq!(live.summary.expects_fail, buffered.expects_fail);
}

#[test]
fn stderr_reader_preserves_completed_lines_on_read_error_and_panic() {
    for fail_read in [true, false] {
        let mut input =
            format!("{}\nboot details\n{}\n", start("a"), expect_ok("kept")).into_bytes();
        if fail_read {
            input.extend_from_slice(&[0xff, b'\n']);
        } else {
            input.extend_from_slice(end("a").as_bytes());
        }
        let mut stream = EntryStream::new("a".to_string(), true);
        let warning = stream
            .read(std::io::Cursor::new(input), |line, _| {
                assert!(
                    !matches!(line.boundary, Some(EntryBoundary::Ended(..))),
                    "reporting failed"
                );
            })
            .unwrap();
        assert!(warning.contains(if fail_read {
            "error reading game stderr"
        } else {
            "stderr reader thread panicked"
        }));
        assert!(stream.started && !stream.terminated);
        assert!(stream.protocol_error.is_none());
        assert_eq!(stream.summary.expects_pass[0].message, "kept");
        assert_eq!(stream.summary.diagnostics, "boot details\n");
    }
}

#[test]
fn malformed_entry_frame_counts_are_rejected() {
    for raw in ["-1", "1.5", "null", "\"seven\""] {
        assert!(
            entry_stream("a", [start("a"), end_with_frame_count("a", raw)])
                .protocol_error
                .is_some(),
            "invalid frame_count {raw} was accepted"
        );
    }
}

#[test]
fn malformed_envelopes_are_rejected_not_guessed_at() {
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("a second start", vec![start("a"), start("a")]),
        (
            "a restart after the end",
            vec![start("a"), end("a"), start("a")],
        ),
        ("an end with no entry open", vec![end("a")]),
        ("an end naming another sim", vec![start("a"), end("b")]),
        ("a start naming another sim", vec![start("wrong")]),
        (
            "a start with no name",
            lines(&[r#"__SIM__ {"op":"entry_start"}"#]),
        ),
        ("an operation before the entry", vec![expect_ok("orphan")]),
        (
            "an operation after the entry",
            vec![start("a"), end("a"), expect_ok("orphan")],
        ),
    ];
    for (what, stream) in cases {
        assert!(
            entry_stream("a", stream).protocol_error.is_some(),
            "{what} was accepted"
        );
    }
}

#[test]
fn requested_sim_names_resolve_to_registered_wire_names() {
    let wires: Vec<String> = [
        "game::boots",
        "game::ui::hover",
        "game::hover",
        "legacy_sim",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let resolve = |specs: &[&str]| {
        resolve_sim_specs(
            &wires,
            &specs.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(resolve(&["boots"]).unwrap(), ["game::boots"]);
    assert_eq!(
        resolve(&["legacy_sim", "game::hover"]).unwrap(),
        ["legacy_sim", "game::hover"]
    );
    assert_eq!(resolve(&["ui::hover"]).unwrap(), ["game::ui::hover"]);
    let error = resolve(&["boots,ui::hover"]).unwrap_err().to_string();
    assert!(error.contains("contains a comma"), "{error}");
    let error = resolve(&["oots"]).unwrap_err().to_string();
    assert!(
        error.contains("unknown sim: oots"),
        "only whole segments match: {error}"
    );
    let error = resolve(&["hover"]).unwrap_err().to_string();
    assert!(
        error.contains("`hover` is ambiguous")
            && error.contains("game::ui::hover")
            && error.contains("game::hover"),
        "{error}"
    );
}

#[test]
fn requested_sim_names_that_are_not_sims_are_all_refused_before_boot() {
    let wires: Vec<String> = ["game::boots", "game::hover"]
        .into_iter()
        .map(String::from)
        .collect();
    let specs: Vec<String> = [
        "boots",
        "helper_probe",
        "boots\nhover",
        "missing_part",
        "other_helper",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let error = resolve_sim_specs(&wires, &specs).unwrap_err().to_string();
    for expected in [
        "unknown sim: helper_probe",
        "unknown sim: missing_part",
        "unknown sim: other_helper",
        "\"boots\\nhover\" contains whitespace",
    ] {
        assert!(
            error.contains(expected),
            "missing `{expected}` in:\n{error}"
        );
    }
    assert!(
        !error.contains("unknown sim: boots"),
        "a valid sim must not be named unknown:\n{error}"
    );
}

#[test]
fn entry_verdict_requires_stop_and_checks_without_failures_or_exclusion() {
    let mut checked = stopped();
    checked.expects_pass.push(SiteMessage {
        message: "ok".into(),
        site: "f:0".into(),
    });
    let mut failed = stopped();
    failed.expects_pass = checked.expects_pass.clone();
    failed.expects_fail.push(SiteMessage {
        message: "x".into(),
        site: "f:1".into(),
    });
    let mut declared = stopped();
    declared.no_checks = Some("a seed; its probe checks".into());
    let mut image = stopped();
    image.image_snapshots.push(ImageSnapshotOp {
        name: "n".into(),
        path: "frame.png".into(),
        tol: 0.001,
        save_ok: true,
        site: String::new(),
    });
    let mut timed_out = stopped();
    timed_out.timed_out = true;
    let mut died = stopped();
    died.process_failure_code = Some(1);
    let mut excluded = stopped();
    excluded.excluded = Some("opt-in fixture".into());

    let aborted = SimSummary::default();
    assert!(!aborted.has_failures());
    assert!(sim_outcome(&stopped(), "s").is_err());
    for (name, summary, verdict) in [
        ("aborted", aborted, "FAIL"),
        ("no checks", stopped(), "FAIL"),
        ("checked", checked, "PASS"),
        ("failed check", failed, "FAIL"),
        ("declared check-free", declared, "PASS"),
        ("image check", image, "PASS"),
        ("timeout", timed_out, "FAIL"),
        ("process failure", died, "FAIL"),
        ("excluded", excluded, "EXCLUDED"),
    ] {
        assert_eq!(entry_passed(&summary), verdict == "PASS", "{name}");
        assert_eq!(entry_excluded(&summary), verdict == "EXCLUDED", "{name}");
        assert_eq!(verdict_word(&summary), verdict, "{name}");
        assert_eq!(
            sim_outcome(&summary, "s").is_ok(),
            verdict != "FAIL",
            "{name}"
        );
    }
}

#[test]
fn a_timeout_names_the_budget_that_fired() {
    let mut summary = stopped();
    summary.timed_out = true;
    summary.timeout_budget = Some(Duration::from_secs(2));
    let error = sim_outcome(&summary, "declared_timeout_slow")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("sim `declared_timeout_slow` timed out after 2s"),
        "the timeout must name the sim and its declared budget: {error}"
    );
}

#[test]
fn manual_gallery_selection_is_explicit_and_last_wins() {
    assert!(manual_sim(&[("SIM_MANUAL".into(), "1".into())]).unwrap());
    assert!(!manual_sim(&[]).unwrap());
    assert!(
        !manual_sim(&[
            ("SIM_MANUAL".into(), "1".into()),
            ("SIM_MANUAL".into(), "0".into())
        ])
        .unwrap()
    );
    assert!(manual_sim(&[("SIM_MANUAL".into(), "yes".into())]).is_err());
}

#[test]
fn sim_summary_from_stderr_parses_exclude_op() {
    let stderr = "__SIM__ {\"op\":\"exclude\",\"reason\":\"opt-in fixture\"}\n";
    let s = checked_summary(stderr);
    assert_eq!(s.excluded.as_deref(), Some("opt-in fixture"));
    assert!(s.saw_stop);
}

#[test]
fn summary_counts_expects() {
    let stderr = r#"
__SIM__ {"op":"start_game","name":"fixture","site":"foo.rock:1"}
__SIM__ {"op":"expect_ok","message":"a","site":"foo.rock:2"}
__SIM__ {"op":"expect_ok","message":"b","site":"foo.rock:3"}
__SIM__ {"op":"expect_fail","message":"c","site":"foo.rock:4"}
__SIM__ {"op":"stop","site":"foo.rock:5"}
"#;
    let s = checked_summary(stderr);
    assert_eq!(s.expects_pass.len(), 2);
    assert_eq!(s.expects_fail.len(), 1);
    assert_eq!(s.expects_fail[0].message, "c");
    assert_eq!(s.expects_fail[0].site, "foo:4");
    assert!(s.saw_stop);
    assert!(s.has_failures());
}

#[test]
fn foreign_stderr_prefixes_preserve_envelopes_deadlines_and_verdicts() {
    let mut stream = EntryStream::new("a".to_string(), true);
    for (index, line) in [
        start("a"),
        expect_ok("checked"),
        r#"__SIM__ {"op": "stop"}"#.to_string(),
        end("a"),
    ]
    .into_iter()
    .enumerate()
    {
        let line = format!("foreign stderr: {line}");
        let observed = stream.observe(&line, |_, _| {});
        assert_eq!(observed.diagnostic, Some("foreign stderr: "));
        assert_eq!(
            matches!(observed.boundary, Some(EntryBoundary::Started)),
            index == 0
        );
    }
    assert!(stream.protocol_error.is_none());
    assert!(stream.started && stream.terminated);
    assert_eq!(stream.summary.diagnostics, "foreign stderr: \n".repeat(4));
    assert_eq!(verdict_word(&stream.summary), "PASS");
    for line in [
        "noise __SIM__",
        "noise __SIM__ broken JSON",
        r#"noise __SIM__ {"op":"stop"}junk"#,
    ] {
        let error = entry_stream("a", [start("a"), line.to_string()])
            .protocol_error
            .unwrap();
        assert!(error.to_string().contains("invalid sim marker"));
    }
}

#[test]
fn summary_stop_survives_foreign_unterminated_stderr() {
    let child = run_with_production_emitter(
        r#"
emit('{"op":"expect_ok","message":"checked"}', 'test.rock:1')
io.stderr:write('audio stream error: A backend-specific error has occurred: ')
io.stderr:flush()
emit('{"op":"stop"}', 'test.rock:2')
io.stderr:write('`alsa::poll()` returned POLLERR\n')
"#,
    );
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let summary = checked_summary(&String::from_utf8(child.stderr).unwrap());
    assert_eq!(
        verdict_word(&summary),
        "PASS",
        "no stop_sim() observed: {}",
        !summary.saw_stop
    );
}

#[test]
fn many_sim_markers_do_not_exhaust_ffi_ctypes() {
    let child = run_with_production_emitter(
        r#"
for i = 1, 20000 do emit('{"op":"expect_ok","message":"checked"}', 'test.rock:1') end
emit('{"op":"stop"}', 'test.rock:2')
"#,
    );
    let stderr = String::from_utf8(child.stderr).unwrap();
    assert!(
        child.status.success(),
        "{}",
        stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n")
    );
    let summary = checked_summary(&stderr);
    assert_eq!(summary.expects_pass.len(), 20000);
    assert_eq!(verdict_word(&summary), "PASS");
}

#[test]
fn malformed_sim_markers_fail_the_summary() {
    for marker in [
        r#"{"op":"stop""#,
        r#"{"op":"stop"} trailing"#,
        "{}",
        "[]",
        r#"{"op":false}"#,
    ] {
        let stream = entry_stream(
            "a",
            [
                start("a"),
                expect_ok("checked"),
                format!("noise __SIM__ {marker}"),
                r#"__SIM__ {"op":"stop"}"#.to_string(),
                end("a"),
            ],
        );
        assert!(
            stream
                .protocol_error
                .unwrap()
                .to_string()
                .contains("invalid sim marker")
        );
        assert_eq!(stream.summary.expects_pass.len(), 1);
        assert!(!stream.terminated);
    }
}

#[test]
fn summary_flags_missing_stop() {
    let stderr = r#"__SIM__ {"op":"expect_ok","message":"a","site":"foo.rock:1"}"#;
    let s = checked_summary(stderr);
    assert_eq!(s.expects_pass.len(), 1);
    assert!(!s.saw_stop);
    assert!(!s.has_failures());
}

#[test]
fn summary_parses_allocation_census_and_preserves_frame_window() {
    let stderr = "__SIM__ {\"op\":\"allocation_census\",\"build_sha\":\"abc123\",\"rendering_enabled\":false,\"warmup_frames_requested\":2,\"warmup_frames\":2,\"frame_count\":6,\"measured_frames\":4,\"initialization_gross_kb\":3.5,\"outside_frame_gross_kb\":1.25,\"warmup_gross_kb\":8.0,\"measured_gross_kb\":24.0,\"total_gross_kb\":36.75,\"measured_allocator_calls\":40,\"measured_allocation_calls\":32,\"measured_reallocation_calls\":8,\"stop_sim_reached\":true,\"measurement_complete\":true}\n__SIM__ {\"op\":\"stop\"}";
    let invalid =
        checked_summary(&stderr.replace(r#""measured_frames":4"#, r#""measured_frames":-1"#));
    assert!(invalid.allocation_census.is_none());
    assert!(invalid.errors[0].message.contains("measured_frames"));
    let summary = checked_summary(stderr);
    let census = summary.allocation_census.as_ref().expect("census result");
    assert_eq!(census.build_sha, "abc123");
    assert!(!census.rendering_enabled);
    assert_eq!(census.frame_count, 6);
    assert_eq!(census.measured_frames, 4);
    assert_eq!(census.measured_allocation_calls, 32);

    let (record, rank_values) = allocation_census_record("fixture", &summary);
    assert_eq!(record["status"], "PASS");
    assert_eq!(record["rendering_skipped"], true);
    assert_eq!(record["measured_frames"], 4);
    assert_eq!(record["gross_kb_per_frame"], 6.0);
    assert_eq!(record["allocation_calls_per_frame"], 8.0);
    assert_eq!(rank_values, Some((6.0, 8.0)));
}

#[test]
fn census_missing_or_incomplete_result_is_not_a_zero_success() {
    let missing = stopped();
    let (record, rank_values) = allocation_census_record("missing", &missing);
    assert_eq!(census_status(&missing), "no-verdict");
    assert_eq!(record["status"], "no-verdict");
    assert_eq!(record["measured_frames"], 0);
    assert!(rank_values.is_none());

    let incomplete = AllocationCensus {
        build_sha: "abc123".into(),
        rendering_enabled: false,
        warmup_frames_requested: 2,
        warmup_frames: 2,
        frame_count: 2,
        measured_frames: 0,
        initialization_gross_kb: 0.0,
        outside_frame_gross_kb: 0.0,
        warmup_gross_kb: 1.0,
        measured_gross_kb: 0.0,
        total_gross_kb: 1.0,
        measured_allocator_calls: 0,
        measured_allocation_calls: 0,
        measured_reallocation_calls: 0,
        stop_sim_reached: false,
        measurement_complete: false,
    };
    let incomplete = SimSummary {
        saw_stop: true,
        allocation_census: Some(incomplete),
        ..SimSummary::default()
    };
    assert_eq!(census_status(&incomplete), "no-verdict");
    assert!(
        allocation_census_record("incomplete", &incomplete)
            .1
            .is_none()
    );
}

#[test]
fn census_rankings_use_the_requested_per_frame_metric() {
    let rows = vec![
        ("gross".to_string(), "PASS", 20.0, 1.0, 4),
        ("calls".to_string(), "PASS", 10.0, 3.0, 4),
    ];
    assert_eq!(census_rankings(&rows, true)[0]["sim_name"], "gross");
    assert_eq!(census_rankings(&rows, false)[0]["sim_name"], "calls");
}

#[test]
fn resolve_sim_sources_keeps_same_named_sims_in_their_own_files() {
    let root = tempfile::tempdir().unwrap();
    let proj = root.path().join("proj");
    let sims = proj.join("tests").join("sims");
    std::fs::create_dir_all(&sims).unwrap();
    std::fs::create_dir_all(root.path().join(".git")).unwrap();
    std::fs::write(
        root.path().join(".git").join("HEAD"),
        "ref: refs/heads/master\n",
    )
    .unwrap();
    std::fs::write(
        sims.join("a.rock"),
        "@sim\nco fn only() -> Coroutine<Wait> {\n}\n",
    )
    .unwrap();
    std::fs::write(
        sims.join("b.rock"),
        "mod b;\n@sim\nco fn only() -> Coroutine<Wait> {\n}\n",
    )
    .unwrap();
    let registry = SimRegistry::from(Vec::from(
        [
            ("game::only", "proj/tests/sims/a.rock"),
            ("game::b::only", "proj/tests/sims/b.rock"),
        ]
        .map(|(name, source)| RegisteredSim {
            wire_name: name.into(),
            env: Vec::new(),
            source_file: source.into(),
        }),
    ));
    let root = source_root_for_sim_project(&proj).unwrap();
    let resolved = |name| registry.source_for(name, &root).unwrap();
    assert_eq!(
        resolved("game::only"),
        sims.join("a.rock").canonicalize().unwrap()
    );
    assert_eq!(
        resolved("game::b::only"),
        sims.join("b.rock").canonicalize().unwrap()
    );
}

#[test]
fn image_snapshot_source_for_reports_a_wire_missing_from_the_registry() {
    let part = WorkerPart {
        name: "game::missing".to_string(),
        out_dir: PathBuf::from("/tmp/out"),
        frame_budget: 60,
        timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        env: Vec::new(),
        occurrence_id: 0,
        source_file: None,
    };
    let error = snapshot_source_for(&part).unwrap_err();
    assert!(error.contains("game::missing"), "{error}");
}

#[test]
fn prefix_with_site_keeps_origin_in_message() {
    assert_eq!(
        prefix_with_site("smoke:10", "diff 0.006 > tol 0.001"),
        "at smoke:10: diff 0.006 > tol 0.001"
    );
    assert_eq!(prefix_with_site("", "msg"), "msg");
}
