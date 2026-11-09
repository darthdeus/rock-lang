use super::*;

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
fn sim_receipt_prepare_releases_session() {
    const HELPER: &str = "ROCK_TEST_RECEIPT_RELEASE_HELPER";
    if let Some(root) = std::env::var_os(HELPER) {
        let root = PathBuf::from(root);
        let prepared = prepare_sims(
            crate::Backend::Skull,
            root.join("project"),
            root.join("output"),
        )
        .expect("prepare receipt");
        assert!(
            prepared.stone.is_none(),
            "receipt preparation retained its compiler session"
        );
        return;
    }
    let root = tempfile::tempdir().expect("receipt fixture");
    std::fs::create_dir(root.path().join("project")).unwrap();
    std::fs::create_dir(root.path().join("output")).unwrap();
    std::fs::write(
        root.path().join("project/main.rock"),
        "@nomangle fn answer() -> int { 41 }\n",
    )
    .unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "run_sim::tests::sim_receipt_prepare_releases_session",
            "--nocapture",
        ])
        .env(HELPER, root.path())
        .env(SIM_COMPILE_RECEIPT_DIR_ENV, root.path().join("receipts"))
        .env(SIM_COMPILE_RECEIPT_PREPARE_ENV, "1")
        .output()
        .expect("receipt test child");
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn sim_receipt_registry_requires_source_files() {
    let wire_names = vec!["probe".to_string()];
    let env_by_wire = HashMap::new();
    let missing = registry_json(&wire_names, &env_by_wire, &HashMap::new()).unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("compiled @sim `probe` has no recorded source file")
    );

    let malformed = serde_json::json!([{"wire_name":"probe", "env":[]}]);
    assert!(
        parse_registry(&malformed)
            .unwrap_err()
            .to_string()
            .contains("source file for `probe` is missing or invalid")
    );
}

#[test]
fn receipt_sweep_reclaims_dead_pid_roots_and_keeps_live_ones() {
    let tmp = tempfile::tempdir().unwrap();
    // `u32::MAX` converts to no unix pid: dead on every platform the
    // liveness probe runs on.
    let dead = tmp.path().join(format!("sim-receipt-{}-stale", u32::MAX));
    let live = tmp
        .path()
        .join(format!("sim-receipt-{}-running", std::process::id()));
    let legacy = tmp.path().join("sim-receipt-nopid");
    let other = tmp.path().join("sim-output-keep");
    for dir in [&dead, &live, &legacy, &other] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // A published receipt does not spare a dead runner's root: the
    // tempdir is one-run-owned, never shared.
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
    let fingerprint = stone.artifact_fingerprint().to_string();
    std::fs::remove_file(&source).unwrap();
    let lane = reload_lane_session(
        crate::Backend::Skull,
        project.path(),
        true,
        shared.path(),
        Some(stone),
        true,
    )
    .expect("reuse without reading source")
    .expect("selected lane");
    assert_eq!(lane.stone.artifact_fingerprint(), fingerprint);
    let lane_main = Path::new(&lane.output).join(rock_host::project::MAIN_LUA_FILENAME);
    assert_eq!(std::fs::read(&lane_main).unwrap(), original);
    std::fs::write(&lane_main, "fixture rewrite").unwrap();
    assert_eq!(std::fs::read(&main).unwrap(), original);
}

/// Assets are stubbed for EVERY sim child, rendering included: a stubbed
/// image is one white texel, which is what a sim's layout and gameplay
/// assertions sample. Only a declared or inherited "0" turns it off.
#[test]
fn every_sim_child_stubs_its_assets_unless_asked_not_to() {
    // The inherited escape hatch is one-way: it can ask for real assets,
    // never for stubs. A stray "1" must not strip pixels out of an
    // image snapshot.
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

    // No ambient request, so the runner's own default, which every entry now
    // takes. A fixture that needs the file's own texels declares
    // `@sim_env("STONE_ASSET_STUB", "0")` and `preset_env` carries it.
    if std::env::var("STONE_ASSET_STUB").is_err() {
        assert_eq!(asset_stub_value(), "1", "the runner default is stubbed");
    }
}

/// `entry_renders` reads the EFFECTIVE value, last-wins. `@sim_render` is
/// lowered to `SIM_DRAW=on_demand` before an explicit `@sim_env` is appended,
/// so a sim carrying both is whatever it says last -- and that is the value
/// its child actually boots with. Nothing schedules on it now, but the
/// census and the capability docs classify sims by it.
#[test]
fn rendering_is_the_effective_last_wins_value() {
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
    assert!(!entry_renders(&[]), "declaring nothing does not render");
    assert!(
        entry_renders(&[pair("SIM_DRAW", "on_demand")]),
        "@sim_render sugar"
    );
    assert!(!entry_renders(&[pair("SIM_DRAW", "none")]), "explicit skip");
    assert!(
        !entry_renders(&[pair("SIM_DRAW", "commands")]),
        "submit needs no GPU result"
    );
    assert!(
        entry_renders(&[pair("SIM_DRAW", "every_frame")]),
        "continuous GPU rendering"
    );
    // `@sim_render` then an explicit override, in lowering order.
    assert!(
        !entry_renders(&[pair("SIM_DRAW", "on_demand"), pair("SIM_DRAW", "none")]),
        "a later explicit @sim_env overrides the @sim_render sugar"
    );
    assert!(
        entry_renders(&[pair("SIM_DRAW", "none"), pair("SIM_DRAW", "on_demand")]),
        "... and the other way round"
    );
    assert!(
        !entry_renders(&[pair("GAME_FORCE_VFX", "1")]),
        "an unrelated key is not it"
    );
}

/// A MISSING image golden used to bootstrap itself from the current frame
/// and count as a passing expect, so a sim whose goldens are untracked
/// (`game/tests/sims/vfx_capture.snap/`, gitignored as display-specific)
/// self-passed on any machine that had never run it -- the assertion was
/// never evaluated. Only `--update-snapshots` may write a baseline.
#[test]
fn a_missing_image_golden_is_not_a_pass() {
    let dir = std::env::temp_dir().join(format!(
        "rock-golden-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let produced = dir.join("produced.png");
    // A 1x1 PNG; the bytes never get compared on the paths under test.
    std::fs::write(&produced, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).expect("write");
    let baseline = dir.join("golden.png");
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

    // Both sides: with --update-snapshots, writing it is the point.
    let r = handle_image_snapshot(&img, &baseline, true).expect("no hard error");
    assert!(
        matches!(r, SnapResult::Wrote),
        "update writes the baseline; got {r:?}"
    );
    assert!(baseline.exists(), "the baseline is created under update");

    std::fs::remove_dir_all(&dir).ok();
}

/// The lane discriminant decides whether a child may touch the one live
/// `IncrementalProject` session and the one `incremental.lua`. Getting it wrong in
/// the permissive direction lets an ordinary child mutate build state
/// shared with every other child once the suite runs in parallel.
#[test]
fn reload_lane_is_opt_in_via_stone_hot_reload() {
    let pair = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];

    assert!(is_reload_lane(&pair("STONE_HOT_RELOAD", "1")));
    // Only the opt-in key routes to the lane; a busy sim env does not.
    assert!(!is_reload_lane(&pair("SIM_DRAW", "on_demand")));
    assert!(!is_reload_lane(&[]));
    // An explicit disable is not an opt-in: the child's watcher does not
    // poll, so it can never observe a reload.
    assert!(!is_reload_lane(&pair("STONE_HOT_RELOAD", "0")));
    // The key still routes when it is not the only pair.
    let mut mixed = pair("ROCK_TEST_SEED", "42");
    mixed.extend(pair("STONE_HOT_RELOAD", "1"));
    assert!(is_reload_lane(&mixed));
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
    let dir = std::env::temp_dir().join(format!("rock-chunk-path-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join(rock_host::project::MAIN_LUA_FILENAME);
    let bytecode = dir.join(rock_host::project::MAIN_BYTECODE_FILENAME);
    std::fs::write(&main, "-- source").unwrap();

    // No image yet: both lanes run the source file. The fallback is what
    // makes the dump best-effort rather than a new failure mode.
    assert_eq!(worker_chunk_path(&dir, false), main);
    assert_eq!(worker_chunk_path(&dir, true), main);

    std::fs::write(&bytecode, "-- image").unwrap();
    assert_eq!(worker_chunk_path(&dir, false), bytecode);
    assert_eq!(
        worker_chunk_path(&dir, true),
        main,
        "a reload child must keep the source path it derives the overlay from"
    );

    std::fs::remove_dir_all(&dir).ok();
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
    let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(
        workload_env(&[]),
        workload_env(&pairs(&[("SIM_DRAW", "none")])),
        "omitting a runner default and declaring it are the same workload"
    );
    assert_ne!(
        workload_env(&[]),
        workload_env(&pairs(&[("SIM_DRAW", "on_demand")]))
    );
    let last = workload_env(&pairs(&[("SIM_DRAW", "none"), ("SIM_DRAW", "on_demand")]));
    assert!(last.contains(&("SIM_DRAW".into(), "on_demand".into())));
    let out = workload_env(&pairs(&[("GAME_HISTORY_DIR", "$OUT/history")]));
    assert!(out.contains(&("GAME_HISTORY_DIR".into(), "$OUT/history".into())));
}

/// Only the pair marks a lane: real assets with the cap declined. The
/// runner's stub default and the baseline cap stand in for what a fixture
/// does not say, last declaration wins, and the render flag is not read.
#[test]
fn entry_full_assets_needs_real_assets_and_no_image_cap() {
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
    assert!(
        !entry_full_assets(&[], "1"),
        "declaring nothing is stubbed and capped"
    );
    assert!(
        !entry_full_assets(&[pair("STONE_ASSET_STUB", "0")], "1"),
        "real assets alone stay capped"
    );
    assert!(
        !entry_full_assets(&[pair("STONE_IMAGE_MAX_SIDE", "0")], "1"),
        "an uncapped stub is one texel"
    );
    assert!(entry_full_assets(
        &[
            pair("STONE_ASSET_STUB", "0"),
            pair("STONE_IMAGE_MAX_SIDE", "0")
        ],
        "1"
    ));
    assert!(
        entry_full_assets(
            &[
                pair("STONE_IMAGE_MAX_SIDE", "0"),
                pair("SIM_DRAW", "commands")
            ],
            "0"
        ),
        "an inherited STONE_ASSET_STUB=0 counts, and submit is not consulted"
    );
    assert!(
        !entry_full_assets(
            &[
                pair("STONE_ASSET_STUB", "0"),
                pair("STONE_IMAGE_MAX_SIDE", "0"),
                pair("STONE_IMAGE_MAX_SIDE", "512")
            ],
            "1"
        ),
        "last declaration wins"
    );
}

#[test]
fn the_classed_pool_caps_live_capped_tasks_and_runs_everything_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let renders: Vec<bool> = (0..40).map(|i| i % 3 == 0).collect();
    let live = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let results = run_bounded_classed(&PoolWidth::fixed(8), 2, &renders, |i| {
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

/// Both pools the lane runs beside: the plain one and the worker-mode
/// pool with a capped class.
#[test]
fn the_pool_takes_the_reload_lanes_slot_back_when_the_lane_finishes() {
    let capped: Vec<bool> = (0..60).map(|i| i % 4 == 0).collect();
    reload_lane_slot_returns(|width, task| run_bounded(width, 60, task));
    reload_lane_slot_returns(|width, task| run_bounded_classed(width, 1, &capped, task));
}

fn reload_lane_slot_returns(
    pool: impl FnOnce(
        &PoolWidth,
        &(dyn Fn(usize) -> Result<usize> + Sync),
    ) -> Vec<(usize, Result<usize>)>
    + Send,
) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    // Ordered by events, not sleeps: tasks beside the lane hold until it
    // ends, the lane ends once the narrowed pool is full, and after it
    // three tasks must be live together. A missing widening leaves that
    // barrier at two until the hang backstop, which every wait shares.
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
    let (pool, ()) = beside_reload_lane(
        3,
        true,
        |width| pool(width, &task),
        || {
            wait_for(&|| live.load(Ordering::SeqCst) >= 2);
            // A pool that ignored the lane's slot would start a third
            // task in this window.
            std::thread::sleep(std::time::Duration::from_millis(40));
            lane_done.store(true, Ordering::SeqCst);
        },
    );
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
fn the_classed_pool_with_no_cap_is_the_plain_pool() {
    let renders = vec![true; 5];
    let results = run_bounded_classed(&PoolWidth::fixed(4), 4, &renders, |i| {
        Ok::<usize, anyhow::Error>(i * 2)
    });
    let mut got: Vec<(usize, usize)> = results.into_iter().map(|(i, r)| (i, r.unwrap())).collect();
    got.sort_unstable();
    assert_eq!(got, (0..5).map(|i| (i, i * 2)).collect::<Vec<_>>());
}

#[test]
fn sim_environment_cap_reaches_default_lane_resolution() {
    const CHILD: &str = "ROCK_SIM_CAP_PROBE";
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(resolve_jobs(None, None, false, false).unwrap(), 1);
        assert_eq!(resolve_pool_jobs(None, None, false, false).unwrap(), 1);
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "run_sim::tests::sim_environment_cap_reaches_default_lane_resolution",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("ROCK_SIM_JOBS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sim_environment_caps_defaults_but_explicit_flags_win() {
    let budget = JobBudget { ram: 10, gpu: 8 };
    assert_eq!(
        resolve_jobs_with(None, false, 32, budget, false, Some("2")).unwrap(),
        2
    );
    assert_eq!(
        resolve_jobs_with(Some(6), false, 32, budget, false, Some("2")).unwrap(),
        6
    );
    assert_eq!(
        resolve_jobs_with(Some(12), false, 32, budget, true, Some("2")).unwrap(),
        12
    );
    assert!(resolve_jobs_with(Some(12), false, 32, budget, false, Some("2")).is_err());
    assert_eq!(
        resolve_jobs_with(Some(6), true, 32, budget, true, Some("2")).unwrap(),
        1
    );
    assert_eq!(
        resolve_jobs_with(None, false, 32, budget, false, Some("99")).unwrap(),
        budget.lanes()
    );
    for cap in [
        None,
        Some("0"),
        Some("invalid"),
        Some("-1"),
        Some("184467440737095516160"),
    ] {
        assert_eq!(
            resolve_jobs_with(None, false, 32, budget, false, cap).unwrap(),
            budget.lanes()
        );
    }
}

/// The job count is a safety bound, not a preference: it must never exceed
/// what the machine can run, and `--update-snapshots` must pin to serial
/// because it writes baseline files whose destinations are not proven
/// unique before spawning.
#[test]
fn resolve_jobs_is_bounded_and_serial_for_snapshot_updates() {
    let (cores, budget) = (32, JobBudget { ram: 4, gpu: 4 });

    assert_eq!(
        resolve_jobs_with(None, false, cores, budget, false, None).unwrap(),
        4,
        "no request takes the memory budget"
    );
    assert_eq!(
        resolve_jobs_with(Some(1), false, cores, budget, false, None).unwrap(),
        1
    );
    let error = resolve_jobs_with(Some(10), false, cores, budget, false, None).unwrap_err();
    assert!(error.to_string().contains("RAM budget of 4"));
    assert!(error.to_string().contains("--jobs-force"));
    assert_eq!(
        resolve_jobs_with(Some(4), false, cores, budget, false, None).unwrap(),
        4
    );
    assert_eq!(
        resolve_jobs_with(Some(8), true, cores, budget, false, None).unwrap(),
        1
    );
    // A forced request may exceed the budget, never the cores.
    assert_eq!(
        resolve_jobs_with(Some(10), false, cores, budget, true, None).unwrap(),
        10
    );
    assert_eq!(
        resolve_jobs_with(Some(usize::MAX), false, cores, budget, true, None).unwrap(),
        cores
    );
    // 0 is meaningless; clamp up rather than deadlock on an empty pool.
    assert_eq!(
        resolve_jobs_with(Some(0), false, cores, budget, false, None).unwrap(),
        1
    );
    // --update-snapshots wins over ANY explicit request.
    assert_eq!(
        resolve_jobs_with(None, true, cores, budget, true, None).unwrap(),
        1
    );
    assert_eq!(
        resolve_jobs_with(Some(8), true, cores, budget, true, None).unwrap(),
        1
    );
}

#[test]
fn a_request_above_only_the_free_gpu_budget_runs_at_that_budget() {
    let (cores, budget) = (32, JobBudget { ram: 10, gpu: 2 });
    assert_eq!(
        resolve_jobs_with(Some(8), false, cores, budget, false, None).unwrap(),
        2
    );
    assert_eq!(
        resolve_jobs_with(Some(10), false, cores, budget, false, None).unwrap(),
        2
    );
    let error = resolve_jobs_with(Some(11), false, cores, budget, false, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("--jobs 11 exceeds the RAM budget of 10"),
        "{error}"
    );
    // Force still takes the request as asked.
    assert_eq!(
        resolve_jobs_with(Some(8), false, cores, budget, true, None).unwrap(),
        8
    );
    assert_eq!(
        resolve_jobs_with(None, false, cores, budget, false, None).unwrap(),
        2
    );
}

#[test]
fn a_request_above_the_known_lanes_is_narrowed_before_the_budget() {
    let (cores, budget) = (32, JobBudget { ram: 1, gpu: 1 });
    // One name under a one-lane budget runs in its one lane.
    assert_eq!(
        resolve_jobs_with(
            narrow_to_lanes(Some(4), Some(1)),
            false,
            cores,
            budget,
            false,
            None
        )
        .unwrap(),
        1
    );
    // Two names still need two lanes, which the RAM budget refuses.
    let error = resolve_jobs_with(
        narrow_to_lanes(Some(4), Some(2)),
        false,
        cores,
        budget,
        false,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("--jobs 2 exceeds"), "{error}");
    // Unknown lanes leave the request alone; no request stays no request.
    assert_eq!(narrow_to_lanes(Some(4), None), Some(4));
    assert_eq!(narrow_to_lanes(None, Some(2)), None);
    assert_eq!(narrow_to_lanes(Some(0), Some(2)), Some(0));
}

#[test]
fn the_default_job_count_leaves_a_fixed_gpu_memory_reserve() {
    // Four lanes' worth above the reserve: four lanes.
    assert_eq!(
        vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB + 4 * SIM_LANE_VRAM_MIB + 1)),
        4
    );
    // A small machine with a large card stays bounded by its cores.
    assert_eq!(
        vram_budgeted_jobs(2, Some(SIM_VRAM_RESERVE_MIB + 64 * SIM_LANE_VRAM_MIB)),
        2
    );
    // Nothing above the reserve still runs one lane rather than none.
    assert_eq!(vram_budgeted_jobs(32, Some(SIM_VRAM_RESERVE_MIB)), 1);
    assert_eq!(vram_budgeted_jobs(32, Some(SIM_LANE_VRAM_MIB / 4)), 1);
    // Without a reading (no NVIDIA tooling, e.g. macOS) the cores decide.
    assert_eq!(vram_budgeted_jobs(14, None), 14);
}

#[test]
fn default_jobs_fit_ram_and_gpu_budgets() {
    let studio = memory_budgets(14, Some(36 * 1024), None);
    assert_eq!(studio.lanes(), 10);
    assert_eq!(
        resolve_jobs_with(None, false, 14, studio, false, None).unwrap(),
        10
    );
    assert_eq!(
        resolve_jobs_with(Some(14), false, 14, studio, true, None).unwrap(),
        14
    );
    assert_eq!(
        resolve_jobs_with(Some(14), true, 14, studio, true, None).unwrap(),
        1
    );
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
fn a_hot_reload_sim_is_reload_lane() {
    let mut env: HashMap<String, Vec<(String, String)>> = HashMap::new();
    env.insert("plain".into(), vec![("ROCK_TEST_SEED".into(), "42".into())]);
    env.insert(
        "reloader".into(),
        vec![("STONE_HOT_RELOAD".into(), "1".into())],
    );

    assert!(!entry_is_reload_lane("plain", &env));
    assert!(entry_is_reload_lane("reloader", &env));
    // A name with no recorded env is ordinary, not a panic.
    assert!(!entry_is_reload_lane("unknown", &env));
}

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn a_declared_frame_count_is_the_entrys_budget() {
    assert_eq!(
        frame_budget_for("s", &env(&[])).expect("default"),
        DEFAULT_FRAME_BUDGET
    );
    assert_eq!(
        frame_budget_for("s", &env(&[("FRAME_COUNT", "90")])).expect("declared"),
        90
    );
    // The two watchdog sims in sample declare this; a runner that quietly
    // capped them at the default would guard them with a bound nobody wrote.
    assert_eq!(
        frame_budget_for("s", &env(&[("FRAME_COUNT", "100000")])).expect("declared"),
        100000
    );
    // Other keys are not budgets.
    assert_eq!(
        frame_budget_for("s", &env(&[("STONE_FX_OFF", "1")])).expect("default"),
        DEFAULT_FRAME_BUDGET
    );
}

#[test]
fn a_declared_timeout_is_the_entrys_wall_budget() {
    assert_eq!(
        timeout_for("s", &env(&[])).expect("default"),
        Duration::from_secs(sim_timeout_secs())
    );
    assert_eq!(
        timeout_for("s", &env(&[("ROCK_SIM_TIMEOUT_SECS", "1200")])).expect("declared"),
        Duration::from_secs(1200)
    );
    // Other keys are not budgets, and the last declaration wins.
    assert_eq!(
        timeout_for("s", &env(&[("FRAME_COUNT", "90")])).expect("default"),
        Duration::from_secs(sim_timeout_secs())
    );
    assert_eq!(
        timeout_for(
            "s",
            &env(&[
                ("ROCK_SIM_TIMEOUT_SECS", "90"),
                ("ROCK_SIM_TIMEOUT_SECS", "1200")
            ])
        )
        .expect("declared"),
        Duration::from_secs(1200)
    );
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

#[test]
fn a_frame_count_uses_the_last_declaration() {
    assert_eq!(
        frame_budget_for("s", &env(&[("FRAME_COUNT", "80"), ("FRAME_COUNT", "90")]))
            .expect("last declaration is valid"),
        90
    );
    assert_eq!(
        frame_budget_for("s", &env(&[("FRAME_COUNT", "bad"), ("FRAME_COUNT", "90")]))
            .expect("an overridden invalid declaration is inert"),
        90
    );
    assert!(
        frame_budget_for("s", &env(&[("FRAME_COUNT", "90"), ("FRAME_COUNT", "bad")])).is_err(),
        "the effective last declaration is the one that must be validated"
    );
}

#[test]
fn buffered_runs_never_write_live() {
    assert!(writes_live(false));
    assert!(!writes_live(true));
}

fn lines(raw: &[&str]) -> Vec<String> {
    raw.iter().map(|s| s.to_string()).collect()
}

/// The deadline anchor resets ONLY on `EnvelopeStep::Started`, so the
/// envelope's classification IS the guard's admission: a start the split
/// would reject -- malformed, for another sim, or a second one -- must not
/// read as Started.
#[test]
fn only_an_accepted_entry_start_reads_as_started() {
    let started =
        |t: &mut Envelope, line: &str| matches!(t.observe(line), Ok(EnvelopeStep::Started));

    let mut t = Envelope::new("a");
    assert!(
        started(&mut t, &start("a")),
        "the child's own start is Started"
    );
    assert!(
        t.observe(r#"__SIM__ {"op":"entry_start"}"#).is_err(),
        "malformed: no name"
    );
    assert!(
        t.observe(&start("a")).is_err(),
        "a second start while the entry is open"
    );
    assert!(matches!(
        t.observe(&end("a")),
        Ok(EnvelopeStep::Ended(None, None))
    ));
    assert!(
        t.observe(&start("a")).is_err(),
        "a closed entry cannot restart"
    );

    assert!(
        Envelope::new("a").observe(&start("b")).is_err(),
        "another sim's name"
    );
}

/// The wedged-child evasion, deterministically: a sim stuck inside one
/// tick can flood `entry_start`-shaped lines, and the deadline may reset on
/// none of them. After any violation the guard is poisoned, so even a line
/// that would otherwise be the valid start extends nothing.
#[test]
fn the_deadline_guard_resets_only_on_the_accepted_start() {
    let mut g = DeadlineGuard::new("a");
    assert!(g.observe_resets(&start("a")), "the child's start resets");
    assert!(
        !g.observe_resets(&expect_ok("inside a")),
        "an ordinary op does not"
    );
    assert!(
        !g.observe_resets("[   0.1s INFO ] raw logging"),
        "raw output does not"
    );
    assert!(!g.observe_resets(&start("a")), "a second start does not");

    let mut g = DeadlineGuard::new("a");
    assert!(
        !g.observe_resets(&expect_ok("before the start")),
        "an op before the start poisons"
    );
    assert!(
        !g.observe_resets(&start("a")),
        "a poisoned guard must not resume resetting on a later well-formed start"
    );
}

#[test]
fn entry_frame_timing_is_validated_and_retained() {
    let mut envelope = Envelope::new("a");
    assert!(envelope.observe(&start("a")).is_ok());
    let valid = end_with_frame_timing(
        "a",
        3,
        r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
    );
    assert!(matches!(
        envelope.observe(&valid),
        Ok(EnvelopeStep::Ended(
            Some(3),
            Some(EntryFrameTiming {
                frames: 3,
                draw_frames: 2,
                submitted_frames: 1,
                logic_us: 10,
                render_cpu_us: 20,
                present_us: 30,
            }),
        ))
    ));

    for timing in [
        r#"{"frames":3,"draw_frames":4,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
        r#"{"frames":2,"draw_frames":2,"submitted_frames":1,"logic_us":10,"render_cpu_us":20,"present_us":30}"#,
        r#"{"frames":3,"draw_frames":2,"submitted_frames":1,"logic_us":-1,"render_cpu_us":20,"present_us":30}"#,
    ] {
        let mut envelope = Envelope::new("a");
        assert!(envelope.observe(&start("a")).is_ok());
        assert!(
            envelope
                .observe(&end_with_frame_timing("a", 3, timing))
                .is_err(),
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

/// Operations inside the envelope are the sim's; boot and teardown
/// logging outside it is not part of the entry but stays in the debugging
/// stream.
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

    let out = split_child_stream(&stream, "a").expect("well-formed stream");
    assert_eq!(out.sim, vec![expect_ok("from a")]);
    assert!(out.started && out.terminated);
    assert_eq!(out.frame_count, Some(7));
    assert!(out.raw_all.iter().any(|l| l.contains("booting")));
    assert!(out.raw_all.iter().any(|l| l.contains("teardown")));
}

/// A child killed mid-entry leaves its entry open. That is the ONE
/// tolerated gap, and it is recorded rather than read as a clean close.
#[test]
fn a_killed_child_leaves_its_entry_unterminated() {
    let out = split_child_stream(&[start("a"), expect_ok("partial")], "a")
        .expect("a truncated stream is not malformed");
    assert!(out.started && !out.terminated);
    assert_eq!(out.sim, vec![expect_ok("partial")]);
}

#[test]
fn malformed_entry_frame_counts_are_rejected() {
    for raw in ["-1", "1.5", "null", "\"seven\""] {
        assert!(
            split_child_stream(&[start("a"), end_with_frame_count("a", raw)], "a").is_err(),
            "invalid frame_count {raw} was accepted"
        );
    }
}

/// Every other shape is a protocol violation, and none may be quietly
/// absorbed into the record.
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
            split_child_stream(&stream, "a").is_err(),
            "{what} was accepted"
        );
    }
}

#[test]
fn extract_str_field_basic() {
    let json =
        serde_json::from_str::<serde_json::Value>(r#"{"op":"expect_fail","message":"1 segment"}"#)
            .unwrap();
    assert_eq!(extract_str_field(&json, "op"), Some("expect_fail".into()));
    assert_eq!(
        extract_str_field(&json, "message"),
        Some("1 segment".into())
    );
    assert_eq!(extract_str_field(&json, "missing"), None);
}

#[test]
fn extract_str_field_handles_escapes() {
    let json =
        serde_json::from_str::<serde_json::Value>(r#"{"message":"hello \"world\""}"#).unwrap();
    assert_eq!(
        extract_str_field(&json, "message"),
        Some(r#"hello "world""#.into())
    );
}

#[test]
fn extract_raw_field_object_args() {
    let line = serde_json::from_str::<serde_json::Value>(
        r#"{"op":"custom","name":"x","args":{"a":1,"b":[2,3]}}"#,
    )
    .unwrap();
    assert_eq!(
        extract_raw_field(&line, "args"),
        Some(r#"{"a":1,"b":[2,3]}"#.to_string())
    );
}

#[test]
fn extract_image_snapshot_tolerance_and_save_status() {
    let line = serde_json::from_str::<serde_json::Value>(
        r#"{"op":"image_snapshot","name":"x","tol":0.001,"ok":true,"path":"/tmp/x"}"#,
    )
    .unwrap();
    assert_eq!(extract_raw_field(&line, "tol"), Some("0.001".into()));
    assert_eq!(extract_raw_field(&line, "ok"), Some("true".into()));
}

#[test]
fn extract_raw_field_handles_quotes_inside_strings() {
    // The "args" field contains a string with an escaped `}`.
    // Naive depth tracking would stop early at the string's `}`.
    let line = serde_json::from_str::<serde_json::Value>(r#"{"args":{"msg":"abc}def"},"after":1}"#)
        .unwrap();
    assert_eq!(
        extract_raw_field(&line, "args"),
        Some(r#"{"msg":"abc}def"}"#.to_string())
    );
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

// A name the registry lacks used to pass through to the worker's entry
// plan, where a name holding a line break (one argv element of
// newline-joined names) split the plan line and killed the whole worker
// with `malformed entry plan line: <a valid sim>`.
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
fn entry_passed_requires_stop_and_no_failures() {
    // Shared pass-predicate for both multi-run paths. Default (no
    // stop_sim observed) is a fail even with zero expects.
    let mut s = SimSummary::default();
    assert!(!entry_passed(&s));
    s.saw_stop = true;
    s.expects_pass.push(SiteMessage {
        message: "ok".into(),
        site: "f:0".into(),
    });
    assert!(entry_passed(&s));
    s.expects_fail.push(SiteMessage {
        message: "x".into(),
        site: "f:1".into(),
    });
    assert!(!entry_passed(&s));
}

#[test]
fn a_stopped_sim_whose_checks_never_ran_fails() {
    // A fixture that returns before its checks (`STATE ?? return`) still
    // reaches stop_sim() and fails nothing; its coverage is gone.
    let mut empty = SimSummary {
        saw_stop: true,
        ..SimSummary::default()
    };
    assert!(!entry_passed(&empty));
    assert_eq!(verdict_word(&empty), "FAIL");
    assert!(sim_outcome(&empty, "s").is_err());

    let mut declared = SimSummary {
        saw_stop: true,
        ..SimSummary::default()
    };
    declared.no_checks = Some("a seed; its probe checks".into());
    assert!(
        entry_passed(&declared),
        "a declared check-free entry passes"
    );

    empty.image_snapshots.push(ImageSnapshotOp {
        name: "n".into(),
        path: "frame.png".into(),
        tol: 0.001,
        save_ok: true,
        site: String::new(),
    });
    assert!(entry_passed(&empty), "an image snapshot is a check");
}

#[test]
fn an_aborted_sim_prints_fail_not_pass() {
    // The imgui docking abort: zero expects ran, none failed, and the sim
    // never reached stop_sim(). The verdict word must follow the real
    // outcome, not the expect tally.
    let mut aborted = SimSummary::default();
    assert!(
        !aborted.has_failures(),
        "an abort fails no expect -- that is the trap"
    );
    assert_eq!(verdict_word(&aborted), "FAIL");

    aborted.saw_stop = true;
    aborted.expects_pass.push(SiteMessage {
        message: "ok".into(),
        site: "f:0".into(),
    });
    assert_eq!(verdict_word(&aborted), "PASS");

    let mut timed_out = SimSummary {
        saw_stop: true,
        ..SimSummary::default()
    };
    timed_out.timed_out = true;
    assert_eq!(verdict_word(&timed_out), "FAIL");

    let mut died = SimSummary {
        saw_stop: true,
        ..SimSummary::default()
    };
    died.process_failure_code = Some(1);
    assert_eq!(verdict_word(&died), "FAIL");
}

#[test]
fn a_timeout_names_the_budget_that_fired() {
    // The single-run message must state the entry's declared budget, not
    // the process-wide default it overrode.
    let mut summary = SimSummary {
        saw_stop: true,
        ..SimSummary::default()
    };
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
fn entry_excluded_is_not_passed_and_verdict_word_is_excluded() {
    let s = SimSummary {
        excluded: Some("opt-in fixture".to_string()),
        saw_stop: true,
        ..Default::default()
    };
    assert!(!entry_passed(&s));
    assert!(entry_excluded(&s));
    assert_eq!(verdict_word(&s), "EXCLUDED");
}

#[test]
fn sim_summary_from_stderr_parses_exclude_op() {
    let stderr = "__SIM__ {\"op\":\"exclude\",\"reason\":\"opt-in fixture\"}\n";
    let s = SimSummary::from_stderr(stderr);
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
    let s = SimSummary::from_stderr(stderr);
    assert_eq!(s.expects_pass.len(), 2);
    assert_eq!(s.expects_fail.len(), 1);
    assert_eq!(s.expects_fail[0].message, "c");
    assert_eq!(s.expects_fail[0].site, "foo:4");
    assert!(s.saw_stop);
    assert!(s.has_failures());
}

#[test]
fn foreign_stderr_prefixes_preserve_envelopes_deadlines_and_verdicts() {
    let mut guard = DeadlineGuard::new("a");
    let stream: Vec<String> = [
        start("a"),
        expect_ok("checked"),
        r#"__SIM__ {"op": "stop"}"#.to_string(),
        end("a"),
    ]
    .into_iter()
    .map(|line| format!("foreign stderr: {line}"))
    .collect();
    assert!(guard.observe_resets(&stream[0]));
    for line in &stream[1..] {
        assert!(!guard.observe_resets(line));
    }
    let entry = split_child_stream(&stream, "a").unwrap();
    assert!(entry.started && entry.terminated);
    assert_eq!(entry.raw_all, vec!["foreign stderr: "; 4]);
    assert_eq!(
        verdict_word(&SimSummary::from_stderr(&entry.sim.join("\n"))),
        "PASS"
    );
    for line in [
        "noise __SIM__",
        "noise __SIM__ broken JSON",
        r#"noise __SIM__ {"op":"stop"}junk"#,
    ] {
        let error = split_child_stream(&[start("a"), line.to_string()], "a")
            .err()
            .unwrap();
        assert!(error.to_string().contains("invalid sim marker"));
    }
}

#[test]
fn summary_stop_survives_foreign_unterminated_stderr() {
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
        format!(
            r#"
local function emit(json_line, site)
{emitter}
end
emit('{{"op":"expect_ok","message":"checked"}}', 'test.rock:1')
io.stderr:write('audio stream error: A backend-specific error has occurred: ')
io.stderr:flush()
emit('{{"op":"stop"}}', 'test.rock:2')
io.stderr:write('`alsa::poll()` returned POLLERR\n')
"#
        ),
    )
    .unwrap();
    let child = std::process::Command::new(rock_host::project::luajit_path())
        .arg(script.path())
        .env("STONE_HEADLESS", "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let summary = SimSummary::from_stderr(&String::from_utf8(child.stderr).unwrap());
    assert_eq!(
        verdict_word(&summary),
        "PASS",
        "no stop_sim() observed: {}",
        !summary.saw_stop
    );
}

// Each emission once ran two ffi.cdef parses, about 8 ctypes, so a long sim
// overflowed LuaJIT's ctype table near 8,000 markers.
#[test]
fn many_sim_markers_do_not_exhaust_ffi_ctypes() {
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
        format!(
            r#"
local function emit(json_line, site)
{emitter}
end
for i = 1, 20000 do emit('{{"op":"expect_ok","message":"checked"}}', 'test.rock:1') end
emit('{{"op":"stop"}}', 'test.rock:2')
"#
        ),
    )
    .unwrap();
    let child = std::process::Command::new(rock_host::project::luajit_path())
        .arg(script.path())
        .env("STONE_HEADLESS", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8(child.stderr).unwrap();
    assert!(
        child.status.success(),
        "{}",
        stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n")
    );
    let summary = SimSummary::from_stderr(&stderr);
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
        let summary = SimSummary::from_stderr(&format!(
            "__SIM__ {{\"op\":\"expect_ok\"}}\nnoise __SIM__ {marker}\n__SIM__ {{\"op\":\"stop\"}}"
        ));
        assert_eq!(verdict_word(&summary), "FAIL", "accepted {marker}");
        assert!(
            summary
                .errors
                .iter()
                .any(|error| error.message.contains("invalid sim marker"))
        );
    }
}

#[test]
fn summary_flags_missing_stop() {
    let stderr = r#"__SIM__ {"op":"expect_ok","message":"a","site":"foo.rock:1"}"#;
    let s = SimSummary::from_stderr(stderr);
    assert_eq!(s.expects_pass.len(), 1);
    assert!(!s.saw_stop);
    assert!(!s.has_failures());
}

#[test]
fn summary_parses_allocation_census_and_preserves_frame_window() {
    let stderr = "__SIM__ {\"op\":\"allocation_census\",\"build_sha\":\"abc123\",\"rendering_enabled\":false,\"warmup_frames_requested\":2,\"warmup_frames\":2,\"frame_count\":6,\"measured_frames\":4,\"initialization_gross_kb\":3.5,\"outside_frame_gross_kb\":1.25,\"warmup_gross_kb\":8.0,\"measured_gross_kb\":24.0,\"total_gross_kb\":36.75,\"measured_allocator_calls\":40,\"measured_allocation_calls\":32,\"measured_reallocation_calls\":8,\"stop_sim_reached\":true,\"measurement_complete\":true}\n__SIM__ {\"op\":\"stop\"}";
    let summary = SimSummary::from_stderr(stderr);
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
    let missing = SimSummary {
        saw_stop: true,
        ..Default::default()
    };
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
fn summary_compresses_site_to_stem_and_line() {
    let stderr = r#"__SIM__ {"op":"expect_fail","message":"x","site":"/work/repo/game/tests/sims/bandage.rock:29"}"#;
    let s = SimSummary::from_stderr(stderr);
    assert_eq!(s.expects_fail[0].site, "bandage:29");
}

#[test]
fn resolve_sim_sources_keeps_same_named_sims_in_their_own_files() {
    // Two modules declare the same trailing segment; the receipt's
    // per-wire entries disambiguate where a decl-segment scan errored.
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
    let relative: HashMap<String, String> = [
        (
            "game::only".to_string(),
            "proj/tests/sims/a.rock".to_string(),
        ),
        (
            "game::b::only".to_string(),
            "proj/tests/sims/b.rock".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    let resolved = resolve_sim_sources(&proj, &relative).unwrap();
    let canonical = |name: &str| sims.join(name).canonicalize().unwrap();
    assert_eq!(resolved["game::only"], canonical("a.rock"));
    assert_eq!(resolved["game::b::only"], canonical("b.rock"));
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
    // Post-run image_snapshot failures clear `site` so
    // `print()`'s "already-streamed" guard prints the line, but the
    // origin still has to reach the user -- fold it into the
    // message. Regression for the bug that hid every `diff > tol`
    // line behind the streamed image_snapshot op announcement.
    assert_eq!(
        prefix_with_site("smoke:10", "diff 0.006 > tol 0.001"),
        "at smoke:10: diff 0.006 > tol 0.001"
    );
    assert_eq!(prefix_with_site("", "msg"), "msg");
}

#[test]
fn reload_completion_is_terminal_after_publication() {
    let mut ledger = ReloadLedger::default();
    ledger.book(7).expect("request books once");
    ledger.publish(7).expect("artifact publishes once");
    ledger
        .complete(7, ReloadTerminal::Applied)
        .expect("watcher completion follows publication");
    assert_eq!(ledger.summary().requests, 1);
    assert_eq!(ledger.summary().applied, 1);
    assert_eq!(ledger.summary().pending, 0);
    assert_eq!(ledger.summary().rejected, 0);
    assert_eq!(ledger.summary().failed, 0);
}

#[test]
fn reload_ledger_distinguishes_booked_and_published_pending() {
    let mut ledger = ReloadLedger::default();
    ledger.book(1).unwrap();
    ledger.book(2).unwrap();
    ledger.publish(2).unwrap();
    assert_eq!(ledger.summary().requests, 2);
    assert_eq!(ledger.summary().pending, 2);
    assert_eq!(ledger.validate().unwrap().pending, 2);
}

#[test]
fn reload_ledger_accounts_rejection_and_failed_publication() {
    let mut ledger = ReloadLedger::default();
    ledger.book(1).unwrap();
    ledger.reject(1).unwrap();
    ledger.book(2).unwrap();
    ledger.publish(2).unwrap();
    ledger.complete(2, ReloadTerminal::Failed).unwrap();
    assert_eq!(
        ledger.validate().unwrap(),
        ReloadLedgerSummary {
            requests: 2,
            applied: 0,
            pending: 0,
            rejected: 1,
            failed: 1
        }
    );
}

#[test]
fn reload_ledger_rejects_drop_duplicate_and_early_completion() {
    let mut dropped = ReloadLedger::default();
    dropped.book(1).unwrap();
    assert_eq!(
        dropped.summary().pending,
        1,
        "a dropped request cannot disappear from accounting"
    );

    let mut duplicate = ReloadLedger::default();
    duplicate.book(1).unwrap();
    assert!(duplicate.complete(1, ReloadTerminal::Applied).is_err());
    duplicate.publish(1).unwrap();
    assert!(duplicate.publish(1).is_err());
    duplicate.complete(1, ReloadTerminal::Applied).unwrap();
    assert!(duplicate.complete(1, ReloadTerminal::Applied).is_err());
}
