//! B5: boot-checkpoint workers composed with the bounded `-j` scheduler.
//!
//! Phase A runs fresh processes in parallel; Phase B runs many entries in one
//! booted process. B5 is both at once -- a bounded number of booted workers,
//! each serving one canonical boot signature. What has to survive the
//! composition is everything the two mechanisms already guarantee separately:
//! entry-granular parallelism, per-lane recovery, a hot-reload lane that
//! cannot reach an ordinary child, and positional result placement.
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
use crate::stone_harness::{fake_free_gpu_mib, HeavyCommandExt, SpawnBudget};

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

#[test]
fn renderer_excluded_entry_prints_one_verdict_beside_a_passing_sim() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let run = run_project_workers(
        "mag",
        &["menu_logo_reflection_wgpu_contract", "teleport_pad_arrival_phases"],
        "1",
    );
    let combined = run.combined();
    assert!(run.success, "{combined}");
    let exclusions: Vec<_> = combined.lines()
        .filter(|line| line.contains("mag::menu_logo_reflection_wgpu_contract: EXCLUDED"))
        .collect();
    assert_eq!(exclusions.len(), 1, "{combined}");
    assert_eq!(exclusions[0],
        "[sim] mag::menu_logo_reflection_wgpu_contract: EXCLUDED (requires wgpu renderer (active is gl))");
    assert_eq!(combined.lines()
        .filter(|line| line.starts_with("[sim] mag::teleport_pad_arrival_phases: PASS"))
        .count(), 1, "{combined}");
    assert!(combined.contains("1/1 sims passed (selected 2, 1 excluded)"), "{combined}");
}

/// A minimal stone project named `name` with an empty `tests/sims` directory.
fn sim_fixture_project(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    std::fs::create_dir_all(project.join("tests/sims")).unwrap();
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"fixture\"\n",
        workspace_root().join("stone")
    )).unwrap();
    std::fs::write(project.join("main.rock"), format!(
        "fn main() {{ stone_run_game(GameConfig {{ title: \"{name}\", width: 64, height: 64 }}) }}\n"
    )).unwrap();
    project
}

#[test]
fn single_and_multi_name_workers_boot_the_same_bytecode_format() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let fixture = tempfile::tempdir().expect("boot format fixture");
    let project = sim_fixture_project(fixture.path(), "boot_format_fixture");
    std::fs::write(project.join("tests/sims/boot.rock"), r#"
inline(lua) fn record_boot_format() = """
local input = assert(io.open(arg[0], 'rb'))
local magic = input:read(3)
input:close()
local output = assert(io.open(os.getenv('ROCK_FIXTURE_BOOT_FORMAT'), 'ab'))
output:write(string.format('%02x%02x%02x\n', magic:byte(1, 3)))
output:close()
"""
@sim
co fn boot_probe() -> Coroutine<Wait> {
    start_game "boot_format_fixture"
    record_boot_format()
    sim_expect(true, "boot format recorded from the running chunk")
    stop_sim()
}
"#).unwrap();
    let formats = |label: &str, specs: &[&str]| -> Vec<String> {
        let capture = fixture.path().join(format!("{label}.txt"));
        let out = Command::new(rock_bin()).arg("sim").arg(&project).args(specs)
            .args(["--isolated", "-j", "1"]).current_dir(workspace_root())
            .env("ROCK_FIXTURE_BOOT_FORMAT", &capture)
            .env("ROCK_SIM_LOG_DIR", fixture.path().join(label))
            .env("ROCK_SIM_COMPILE_CACHE_DIR", fixture.path().join("cache"))
            .env("ROCK_SIM_TIMEOUT_SECS", "60")
            .output_with_deadline().expect("run boot format fixture");
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{label}: {text}");
        assert_eq!(text.lines().filter(|line| line.contains("boot_probe: PASS")).count(),
            specs.len(), "{label}: {text}");
        std::fs::read_to_string(capture).expect("running chunk formats")
            .lines().map(str::to_owned).collect()
    };
    let single = formats("single", &["boot_probe"]);
    let multi = formats("multi", &["boot_probe", "boot_probe"]);
    assert_eq!(single.len(), 1);
    assert_eq!(multi.len(), 2);
    assert!(multi.iter().all(|format| format == &single[0]),
        "single-name and multi-name workers boot different formats: {single:?} vs {multi:?}");
    assert_eq!(single[0], "1b4c4a", "workers must boot LuaJIT bytecode");
}

#[test]
fn duration_allocated_workers_preserve_isolated_results_in_three_orders() {
    let _spawn_budget = SpawnBudget::for_test(120);
    use serde_json::{json, Value};
    let fixture = tempfile::tempdir().expect("duration fixture");
    let project = sim_fixture_project(fixture.path(), "duration_fixture");
    std::fs::write(project.join("tests/sims/durations.rock"), r#"
@sim
co fn fast() -> Coroutine<Wait> {
    start_game "duration_fixture"
    await frame()
    sim_expect(frame_seq > 0, "fast body reached a host frame")
    stop_sim()
}
@sim
co fn slow() -> Coroutine<Wait> {
    start_game "duration_fixture"
    await frame()
    sim_expect(frame_seq > 0, "slow body reached a host frame")
    stop_sim()
}
"#).unwrap();
    let machine = Command::new("uname").arg("-srmn").output_with_deadline().unwrap();
    assert!(machine.status.success());
    let machine = String::from_utf8(machine.stdout).unwrap();
    let costs = [("fast", 100u64), ("slow", 20_000)];
    let run = |label: &str, mode: &str, specs: &[&str]| -> Vec<Value> {
        let history = fixture.path().join(label);
        let previous = history.join("00");
        std::fs::create_dir_all(&previous).unwrap();
        std::fs::write(previous.join("timings.json"), serde_json::to_vec(&json!({
            "schema_version": 1, "host": {"machine": machine}, "project": project,
            "entries": costs.iter().map(|(name, duration)| json!({
                "name": name, "duration_ms": duration, "completed": true
            })).collect::<Vec<_>>()
        })).unwrap()).unwrap();
        let out = Command::new(rock_bin()).arg("sim").arg(&project).args(specs)
            .args([mode, "-j", "4", "--jobs-force"]).current_dir(workspace_root())
            .env("ROCK_SIM_LOG_DIR", &history).env("ROCK_SIM_TIMEOUT_SECS", "60")
            .output_with_deadline().unwrap();
        assert!(out.status.success(), "{label}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let event_paths: Vec<_> = std::fs::read_dir(&history).unwrap().map(|entry| entry.unwrap().path().join("events.jsonl"))
            .filter(|path| path.is_file()).collect();
        assert_eq!(event_paths.len(), 1);
        std::fs::read_to_string(&event_paths[0]).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    };
    let original: Vec<&str> = (0..100).map(|i| if (53..57).contains(&i) { "slow" } else { "fast" }).collect();
    let results = |events: &[Value]| {
        let mut result: Vec<_> = events.iter().filter(|event| event["event"] == "entry_result")
            .map(|event| (event["occurrence_id"].as_u64().unwrap(), event["verdict"].clone(),
                event["expects_pass"].clone(), event["frame_count"].clone())).collect();
        result.sort_by_key(|row| row.0);
        result.into_iter().map(|(_, verdict, checks, frames)| (verdict, checks, frames)).collect::<Vec<_>>()
    };
    let reference = results(&run("reference", "--isolated", &original));
    assert_eq!(reference.len(), original.len());
    assert!(reference.iter().all(|(verdict, checks, _)| verdict == "PASS" && checks.as_u64().unwrap() > 0));
    let reverse: Vec<_> = original.iter().copied().rev().collect();
    let mut shuffle = original.clone();
    for i in (1..shuffle.len()).rev() {
        shuffle.swap(i, (i * 7919 + 104729) % (i + 1));
    }
    for (label, specs) in [("registry", original), ("reverse", reverse), ("shuffle", shuffle)] {
        let events = run(label, "--workers", &specs);
        let actual = results(&events);
        assert_eq!(actual, reference, "{label}: every result/check/frame agrees with a fresh process");
        let lanes: Vec<_> = events.iter().filter(|event| event["event"] == "lane_planned" && event["kind"] == "shared").collect();
        assert!(lanes.len() <= 4);
        let mut members = Vec::new();
        for lane in &lanes {
            let ids: Vec<_> = lane["entries"].as_array().unwrap().iter().map(|entry| entry["occurrence_id"].as_u64().unwrap()).collect();
            assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 1), "{label}: lane order");
            members.extend(ids);
        }
        members.sort_unstable();
        let planned_entries: Vec<_> = events.iter().filter(|event| event["event"] == "entry_planned").collect();
        let selected_names: Vec<_> = planned_entries.iter().map(|event| event["spec"].as_str().unwrap()).collect();
        let expected_names = specs.clone();
        assert_eq!(selected_names, expected_names, "{label}: selection order and repeated names");
        let planned: Vec<_> = planned_entries.iter().map(|event| event["occurrence_id"].as_u64().unwrap()).collect();
        assert_eq!(members, planned, "{label}: every selected occurrence exactly once");
        if label == "registry" {
            let feasible = [&specs[..54], &specs[54..55], &specs[55..56], &specs[56..]];
            let feasible_peak = feasible.iter().map(|part| part.iter().map(|name| costs.iter().find(|(key, _)| key == name).unwrap().1).sum::<u64>()).max().unwrap();
            assert!(lanes.iter().all(|lane| lane["estimated_ms"].as_u64().unwrap_or_else(|| panic!("missing live estimate: {lanes:?}; config {:?}", events.iter().find(|event| event["event"] == "run_config"))) <= feasible_peak), "production execution must use duration blocks: {lanes:?}");
        }
    }
}

#[test]
fn renderer_exclusions_print_once_in_named_runs_and_implicit_suites() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let fixture = tempfile::tempdir().expect("renderer exclusion fixture");
    let project = sim_fixture_project(fixture.path(), "renderer_fixture");
    std::fs::write(project.join("tests/sims/exclusions.rock"), r#"
@sim
co fn routine() -> Coroutine<Wait> {
    start_game "renderer_fixture"
    sim_expect(true, "routine executed")
    stop_sim()
}
@sim_env("SIM_BACKEND", "wgpu")
@sim
co fn planned() -> Coroutine<Wait> {
    sim_expect(false, "renderer-excluded body must not execute")
    stop_sim()
}
@sim
co fn runtime() -> Coroutine<Wait> {
    start_game "renderer_fixture"
    if !sim_require_renderer("wgpu") { return }
    sim_expect(false, "renderer-excluded body must not continue")
    stop_sim()
}
"#).unwrap();
    for mode in ["--isolated", "--workers"] {
        for names in [&[][..], &["planned", "runtime", "routine"][..], &["planned"][..]] {
            let out = Command::new(rock_bin()).arg("sim").arg(&project).args(names)
                .args([mode, "-j", "1"])
                .current_dir(workspace_root())
                .env("ROCK_SIM_TIMEOUT_SECS", "180")
                .env("ROCK_SIM_LOG_DIR", fixture.path().join("history"))
                .output_with_deadline().expect("run renderer fixture");
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success(), "{mode} {names:?}:\n{text}");
            for name in if names.len() == 1 { &["planned"][..] } else { &["planned", "runtime"][..] } {
                let verdict = format!("[sim] {name}: EXCLUDED (requires wgpu renderer (active is gl))");
                assert_eq!(text.lines().filter(|line| *line == verdict).count(), 1,
                    "{mode} {names:?}:\n{text}");
            }
            if names.len() != 1 {
                assert_eq!(text.lines().filter(|line| line.starts_with("[sim] routine: PASS")).count(), 1, "{text}");
                assert!(text.contains("1/1 sims passed (selected 3, 2 excluded)"), "{text}");
            }
        }
    }
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
    run_project_workers_with_history(project, specs, jobs).0
}

fn run_project_workers_with_history(project: &str, specs: &[&str], jobs: &str) -> (Run, PathBuf) {
    let dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = dir.path().join("lifecycle.log");
    let history = dir.path().join("history");

    let mut args = vec!["sim", project];
    args.extend_from_slice(specs);
    // These fixtures witness explicit worker counts independently of free VRAM.
    args.extend_from_slice(&["--workers", "-j", jobs, "--jobs-force"]);

    // A GPU probe reporting no free memory pins the planner to one lane, so
    // every overlap witness exercises the forced path on any host. A private
    // admission namespace keeps other concurrently running tests' workers
    // from occupying the slots the forced lanes are admitted through.
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("fake tool dir");
    let nvidia = bin.join("nvidia-smi");
    std::fs::write(&nvidia, "#!/usr/bin/env bash\nprintf '0\\n'\n").expect("fake nvidia-smi");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&nvidia, std::fs::Permissions::from_mode(0o755))
            .expect("fake nvidia-smi mode");
    }
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").expect("PATH")));

    let out = Command::new(rock_bin())
        .args(&args)
        .current_dir(workspace_root())
        .env("PATH", std::env::join_paths(paths).expect("PATH join"))
        .env("ROCK_SIM_WORKER_SLOT_DIR", dir.path().join("worker-slots"))
        .env_remove("ROCK_SIM_WORKER_SLOTS")
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .env("ROCK_SIM_LIFECYCLE_LOG", &trace_path)
        .env("ROCK_SIM_LOG_DIR", &history)
        .env_remove("SIM_SKIP_RENDER")
        .output_with_deadline()
        .expect("run rock sim --workers");

    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    (
        Run {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
            trace,
            _dir: dir,
        },
        history,
    )
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
            // Entry granularity; `the_reload_lane_runs_beside_the_worker_pool_in_its_own_chunk`
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
    "testbed::co_join2_tuple",
    "testbed::co_join3_tuple",
    "testbed::co_join_all_list",
    "testbed::co_field_poller",
    "testbed::co_wait_for_result",
    "testbed::b0_leak_probe",
    "testbed::b0_rng_probe",
    "testbed::b0_static_called_probe",
];

/// Missing timings retain the count fallback: more than one quantum of one
/// boot signature under `-j 4` must run in several booted processes AT ONCE.
///
/// Two facts, and both are needed. The peak says the lanes overlapped rather
/// than ran back to back; the total says they are still WORKERS and not eight
/// fresh processes -- an implementation that gave up on sharing a boot would
/// also show a peak above one.
#[test]
fn several_booted_workers_run_at_once() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // These testbed names have no seeded timings. An empty private history
    // exercises the count fallback: 64 entries buy two blocks at -j 4.
    let specs: Vec<&str> = EIGHT.iter().copied().cycle().take(64).collect();
    let run = run_workers(&specs, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    assert!(
        run.stdout.contains(&format!("[sim] {0}/{0} sims passed", specs.len())),
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
    assert_eq!(
        total, 2,
        "{total} processes for {} entries: two lanes of one shared boot each is the \
         whole point of a worker\n{}",
        specs.len(),
        run.trace
    );
}

#[test]
fn small_signature_groups_do_not_buy_singleton_boots() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let specs = [
        "testbed::b0_budget_pred",
        "testbed::b0_budget_probe",
        "testbed::b0_env_pred",
        "testbed::b0_env_probe",
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
    let _spawn_budget = SpawnBudget::for_test(120);
    // A fixture project, not game sims: the property is the worker pool's boot
    // signature, and game sims come and go with their rules.
    let fixture = tempfile::tempdir().expect("skip-render fixture");
    let project = sim_fixture_project(fixture.path(), "skip_render_fixture");
    let mut source = String::new();
    let specs = ["omitted_a", "omitted_b", "explicit_a", "explicit_b"];
    for name in specs {
        if name.starts_with("explicit") {
            source.push_str("@sim_env(\"SIM_SKIP_RENDER\", \"1\")\n");
        }
        source.push_str(&format!(
            "@sim\nco fn {name}() -> Coroutine<Wait> {{\n    start_game \"skip_render_fixture\"\n    sim_expect(true, \"{name} ran\")\n    stop_sim()\n}}\n"
        ));
    }
    std::fs::write(project.join("tests/sims/skip_render.rock"), source).unwrap();

    let run = run_project_workers(project.to_str().unwrap(), &specs, "4");
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
fn world_restore_does_not_leave_keyframes_in_the_successor() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let run = run_workers(&[
        "testbed::scene_owned_task_survives_world_image_restore",
        "testbed::t1a_worker_registries_stay_bounded",
    ], "1");
    assert!(run.success, "the successor owns only its boot state:\n{}", run.combined());
    assert!(run.stdout.contains("[sim] 2/2 sims passed"), "{}", run.combined());
    assert_eq!(peak_procs(&run.trace), (1, 1), "{}", run.trace);
}

#[test]
fn mag_world_restore_preserves_successor_boot_fonts() {
    let _spawn_budget = SpawnBudget::for_test(120);
    for specs in [
        ["mob_cast_world_image_and_arena_reuse", "direct_session_state_world_image_restores_the_authority"],
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
    let _spawn_budget = SpawnBudget::for_test(120);
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

fn history_events(history: &Path) -> Vec<serde_json::Value> {
    let entries = std::fs::read_dir(history).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(entries.len(), 1, "one recorded run is expected");
    let events = std::fs::read_to_string(entries[0].path().join("events.jsonl")).unwrap();
    events.lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect()
}

fn entry_end_frame_counts(events: &[serde_json::Value]) -> Vec<(String, u64)> {
    events
        .iter()
        .filter(|e| e["event"] == "entry_end")
        .map(|e| (e["name"].as_str().unwrap().to_string(), e["frame_count"].as_u64().unwrap()))
        .collect()
}

#[test]
fn shared_worker_records_each_entry_frame_count_before_restore() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let specs = ["build_watermark_layout", "target_cycle_sim"];
    let (run, history) = run_project_workers_with_history("mag", &specs, "1");
    assert!(run.success, "shared worker entries pass:\n{}", run.combined());
    assert_eq!(peak_procs(&run.trace), (1, 1), "the entries must share one worker:\n{}", run.trace);

    let events = history_events(&history);
    assert!(events.iter().any(|event| event["event"] == "worker_spawn" && event["kind"] == "shared"));

    // Each entry's own count comes from a process that ran only that entry; a
    // shared worker that carried its predecessor's frames would exceed it.
    let fresh = specs
        .iter()
        .map(|spec| {
            let (alone, alone_history) = run_project_workers_with_history("mag", &[spec], "1");
            assert!(alone.success, "{spec} passes alone:\n{}", alone.combined());
            let counts = entry_end_frame_counts(&history_events(&alone_history));
            assert_eq!(counts.len(), 1, "{spec} alone records one entry: {counts:?}");
            counts[0].clone()
        })
        .collect::<Vec<_>>();
    assert!(fresh.iter().all(|(_, frames)| *frames > 0), "fresh entries must run frames: {fresh:?}");

    assert_eq!(
        entry_end_frame_counts(&events),
        fresh,
        "each shared entry must retain its own pre-restore frame count"
    );
    let result_counts = events
        .iter()
        .filter(|event| event["event"] == "entry_result")
        .map(|event| event["frame_count"].as_u64().unwrap())
        .collect::<Vec<_>>();
    let fresh_counts = fresh.iter().map(|(_, frames)| *frames).collect::<Vec<_>>();
    assert_eq!(result_counts, fresh_counts, "each shared entry result must report its own frame count");
}

/// `-j 1` is the serial path, and it has to STAY the serial path: the composed
/// scheduler must not overlap work the user asked not to overlap. This is the
/// control for the arm above -- without it, a peak above one would be evidence
/// only that something ran, not that `-j` chose it.
#[test]
fn one_job_still_runs_one_worker_at_a_time() {
    let _spawn_budget = SpawnBudget::for_test(120);
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
    let _spawn_budget = SpawnBudget::for_test(120);
    // Lanes split a signature group per 56 entries, so the neighbours repeat to
    // 60: 61 entries make two contiguous lanes at -j 3, the crash in the first.
    const NEIGHBOURS: [&str; 5] = [
        "testbed::b0_leak_probe",
        "testbed::co_join2_tuple",
        "testbed::co_join3_tuple",
        "testbed::co_join_all_list",
        "testbed::co_field_poller",
    ];
    let mut specs = vec!["testbed::b2_crash_head,testbed::b2_crash_tail"];
    specs.extend(NEIGHBOURS.iter().copied().cycle().take(60));
    let run = run_workers(&specs, "3");
    let combined = run.combined();

    assert!(
        !run.success,
        "the crash entry fails, so the invocation fails:\n{combined}"
    );
    // The neighbours are the point: each keeps its own PASS record.
    for name in NEIGHBOURS {
        assert!(
            combined.contains(&format!("{name}: PASS")),
            "`{name}` shares no process with the crash and must still pass:\n{combined}"
        );
    }
    assert!(
        run.stdout.contains("[sim] 60/61 sims passed -- FAILED: testbed::b2_crash_head,testbed::b2_crash_tail"),
        "exactly the crash entry fails, and it is the one named:\n{}",
        run.stdout
    );
    let (peak, _) = peak_procs(&run.trace);
    assert!(peak > 1, "the lanes have to have actually overlapped\n{}", run.trace);
}

/// The hot-reload lane runs BESIDE the pool, in a chunk of its own.
///
/// Reload fixtures and ordinary workers must boot separate files while sharing
/// the shipped bound-callee emission shape.
#[test]
fn the_reload_lane_runs_beside_the_worker_pool_in_its_own_chunk() {
    let _spawn_budget = SpawnBudget::for_test(120);
    const RELOAD: &str = "testbed::reload_probe";
    let dir = tempfile::tempdir().expect("trace tempdir");
    let trace_path = dir.path().join("lifecycle.log");
    let barrier_dir = dir.path().join("reload-lane-barrier");
    std::fs::create_dir(&barrier_dir).expect("reload lane barrier dir");
    let mut args = vec!["sim", "testbed", RELOAD];
    args.extend_from_slice(&EIGHT);
    args.extend_from_slice(&["--workers", "-j", "2"]);
    let mut command = Command::new(rock_bin());
    command.args(&args)
        .current_dir(workspace_root())
        .env("ROCK_SIM_TIMEOUT_SECS", "180")
        .env("ROCK_SIM_LIFECYCLE_LOG", &trace_path)
        .env("ROCK_SIM_RELOAD_LANE_TEST_BARRIER", &barrier_dir)
        .env("ROCK_SKULL_CALLEE_BINDING", "upvalue")
        .env_remove("SIM_SKIP_RENDER");
    // Room for both lanes whatever the host's card is doing.
    fake_free_gpu_mib(&mut command, dir.path(), 6144 + 2 * 2048);
    let out = command
        .output_with_deadline()
        .expect("run rock sim --workers");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "every sim here passes standalone:\n{combined}");
    assert!(
        combined.contains("[sim] hot-reload lane: isolated chunk"),
        "the lane must not share the pool's chunk:\n{combined}"
    );

    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let mut saw_reload = false;
    let mut runner_events = Vec::new();
    for line in trace.lines() {
        let (event, name) = line.split_once('\t').expect("lifecycle event");
        match (event, name == RELOAD) {
            ("proc_start" | "proc_end", _) => {}
            ("reload_lane_dispatched" | "pool_finished", _) => {
                runner_events.push(event);
            }
            ("start", true) => {
                saw_reload = true;
            }
            ("start", false) => {}
            ("end", _) => {}
            _ => unreachable!(),
        }
    }
    assert!(saw_reload, "the reload child never ran\n{trace}");
    assert_eq!(
        runner_events,
        ["reload_lane_dispatched", "pool_finished"],
        "the runner must dispatch the reload lane before the pool can finish; the test barrier holds the pool until dispatch\n{trace}"
    );
}

/// Result PLACEMENT survives the pool. Naming one sim twice is two entries with
/// two results, and records are emitted in spec order however the lanes
/// finished -- completion order under a pool is not deterministic, so it cannot
/// be the reporting order.
#[test]
fn results_stay_positional_and_ordered_under_the_pool() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let specs = ["testbed::b0_rng_probe", "testbed::b0_leak_probe", "testbed::b0_rng_probe", "testbed::co_join2_tuple"];
    let run = run_workers(&specs, "4");
    assert!(run.success, "every sim here passes standalone:\n{}", run.combined());
    assert!(
        run.stdout.contains("[sim] 4/4 sims passed"),
        "each occurrence is its own entry:\n{}",
        run.stdout
    );
    assert_eq!(
        run.stderr.matches("testbed::b0_rng_probe: PASS").count(),
        2,
        "the duplicated spec owns two records:\n{}",
        run.stderr
    );

    let combined = run.combined();
    let order: Vec<&str> = combined
        .lines()
        .filter_map(|l| l.strip_prefix("[sim] "))
        .filter_map(|l| l.rsplit_once(':'))
        .map(|(name, _)| name)
        .filter(|n| specs.contains(n))
        .collect();
    assert_eq!(
        order,
        specs.to_vec(),
        "records are emitted in spec order:\n{}",
        combined
    );
}

#[test]
fn manual_sims_are_excluded_only_from_implicit_suites() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let fixture = tempfile::tempdir().expect("manual sim fixture");
    let project = fixture.path().join("manual_fixture");
    std::fs::create_dir_all(project.join("tests/sims")).unwrap();
    std::fs::write(project.join("stick.toml"), format!(
        "[package]\nname = \"manual_fixture\"\nversion = \"0.1.0\"\ntype = \"bin\"\n[dependencies]\nstone = {{ path = {:?} }}\n[build.vars]\nGIT_SHA = \"fixture\"\n",
        workspace_root().join("stone")
    )).unwrap();
    std::fs::write(project.join("main.rock"),
        "fn main() { stone_run_game(GameConfig { title: \"manual_fixture\", width: 64, height: 64 }) }\n"
    ).unwrap();
    std::fs::write(project.join("tests/sims/selection.rock"), r#"
@sim
co fn routine() -> Coroutine<Wait> {
    start_game "manual_fixture"
    sim_expect(true, "routine executed")
    stop_sim()
}
@sim_env("SIM_MANUAL", "1")
@sim
co fn gallery() -> Coroutine<Wait> {
    start_game "manual_fixture"
    sim_expect(false, "explicit gallery executed")
    stop_sim()
}
"#).unwrap();
    for mode in ["--workers", "--isolated"] {
        let history = fixture.path().join(mode.trim_start_matches('-'));
        let run = |names: &[&str]| {
            Command::new(rock_bin()).arg("sim").arg(&project).args(names)
                .args([mode, "-j", "1"])
                .current_dir(workspace_root())
                .env("ROCK_SIM_TIMEOUT_SECS", "180")
                .env("ROCK_SIM_LOG_DIR", &history)
                .env("ROCK_SIM_WARN_SECS", "0.001")
                .output_with_deadline().expect("run manual fixture")
        };
        let implicit = run(&[]);
        let text = format!("{}{}", String::from_utf8_lossy(&implicit.stdout), String::from_utf8_lossy(&implicit.stderr));
        assert!(implicit.status.success(), "{text}");
        assert!(text.contains("1/1 sims passed (selected 2, 1 excluded)"), "{text}");
        assert!(text.contains("gallery: EXCLUDED (manual; run by name)"), "{text}");
        let entries = std::fs::read_dir(&history).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(entries.len(), 1);
        let events = std::fs::read_to_string(entries[0].path().join("events.jsonl")).unwrap();
        let events = events.lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| event["event"] == "entry_result" && event["verdict"] == "EXCLUDED"));
        assert_eq!(events.last().unwrap()["complete"], true);
        assert!(events.iter().any(|event| event["event"] == "timing_warning"));
        let timings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(entries[0].path().join("timings.json")).unwrap()).unwrap();
        assert_eq!(timings["entries"].as_array().unwrap().len(), 1);
        assert_eq!(timings["entries"][0]["completed"], true);
        let runtime = events.iter().find(|event| event["event"] == "entry_runtime_context").expect("native timing context");
        assert!(runtime["context"]["width"].as_u64().unwrap() > 0);
        assert!(runtime["context"]["jit"].as_str().unwrap().starts_with("on"));
        let explicit = run(&["gallery"]);
        let text = format!("{}{}", String::from_utf8_lossy(&explicit.stdout), String::from_utf8_lossy(&explicit.stderr));
        assert!(!explicit.status.success(), "{text}");
        assert!(text.contains("explicit gallery executed"), "{text}");
        assert!(!text.contains("EXCLUDED"), "{text}");
    }
}

/// A bank watcher sim runs beside ordinary entries, which is safe only because
/// it edits a private copy: the tracked bank every booting sim reads must come
/// out byte-identical and untouched, and the ordinary pair still shares a boot.
fn assert_bank_writer_leaves_the_tracked_bank(name: &str, bank: &str) {
    let tracked = workspace_root().join(bank);
    let before = std::fs::read(&tracked).expect("read tracked bank");
    let before_mtime = std::fs::metadata(&tracked).and_then(|m| m.modified()).expect("bank mtime");
    let run = run_project_workers("mag", &[name, "teleport_pad_arrival_phases", "target_cycle_sim"], "2");
    assert!(run.success, "{}", run.combined());
    assert_eq!(std::fs::read(&tracked).expect("reread tracked bank"), before,
        "`{name}` changed the tracked {bank}");
    assert_eq!(std::fs::metadata(&tracked).and_then(|m| m.modified()).expect("bank mtime"), before_mtime,
        "`{name}` rewrote the tracked {bank} in place");

    let mut live = HashSet::new();
    let mut overlapped = false;
    let writer_wire = format!("mag::{name}");
    for line in run.trace.lines() {
        let (event, entry) = line.split_once('\t').expect("lifecycle event");
        match event {
            "start" => {
                overlapped |= live.iter().any(|other| {
                    (*other == writer_wire.as_str()) != (entry == writer_wire.as_str())
                });
                live.insert(entry);
            }
            "end" => {
                live.remove(entry);
            }
            "proc_start" | "proc_end" => {}
            _ => panic!("unexpected lifecycle event: {line}"),
        }
    }
    assert!(overlapped, "`{name}` never ran beside an ordinary entry:\n{}", run.trace);
    assert_eq!(peak_procs(&run.trace).1, 2, "both ordinary entries must retain their shared boot:\n{}", run.trace);
}

#[test]
fn a_sound_bank_writer_leaves_the_tracked_bank() {
    let _spawn_budget = SpawnBudget::for_test(120);
    // The sound project owns this generated bank and a fresh checkout has none,
    // so prepare it as every `rock sim` does. It stays afterwards: sibling tests
    // boot MAG from this same file concurrently, and removing or rewinding it
    // under them boots a game with no sound bank.
    rock_cli::package::prepare_sound_bank(workspace_root().join("mag").to_str().expect("project path"))
        .expect("prepare production sound bank");
    assert_bank_writer_leaves_the_tracked_bank("sound_bank_transaction", "mag/assets/sound_bank.json");
}

#[test]
fn jit_boot_mode_enabled_is_restored_between_entries() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let run = run_workers(&["testbed::b0_jit_on_seed", "testbed::b0_jit_on_probe"], "1");
    assert_eq!(peak_procs(&run.trace), (1, 1), "must share one boot:\n{}", run.combined());
    assert!(run.success, "JIT boot mode must survive a predecessor toggle:\n{}", run.combined());
    assert!(run.stdout.contains("[sim] 2/2 sims passed"), "{}", run.combined());
}

#[test]
fn jit_boot_mode_explicitly_disabled_is_restored_between_entries() {
    let _spawn_budget = SpawnBudget::for_test(120);
    let run = run_workers(&["testbed::b0_jit_off_seed", "testbed::b0_jit_off_probe"], "1");
    assert_eq!(peak_procs(&run.trace), (1, 1), "must share one boot:\n{}", run.combined());
    assert!(run.success, "explicit JIT-off boot policy must survive a predecessor toggle:\n{}", run.combined());
    assert!(run.stdout.contains("[sim] 2/2 sims passed"), "{}", run.combined());
}
