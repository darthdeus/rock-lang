use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

#[test]
#[ignore = "spawns an instrumented bounded game (-P full)"]
fn bounded_run_writes_activation_counts_joined_to_the_compiler_manifest() {
    let scratch = tempfile::tempdir().expect("scratch");
    let counts = scratch.path().join("activation.tsv");
    let manifest = scratch.path().join("activation.manifest.txt");
    let lua = scratch.path().join("main.lua");

    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(repo_root())
        .args(["frame", "games/match3", "-n", "2", "--output"])
        .arg(&lua)
        .env("ROCK_FRAME_TIMEOUT_SECS", "120")
        .env("SKULL_ALLOCATION_ACTIVATION", &counts)
        .output()
        .expect("spawn instrumented bounded run");
    assert!(
        output.status.success(),
        "instrumented run failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let counts_text = std::fs::read_to_string(&counts).expect("activation counts");
    let manifest_text = std::fs::read_to_string(&manifest).expect("allocation manifest");
    let mut lines = counts_text.lines();
    assert_eq!(lines.next(), Some("rock.allocation-activation.v2"));
    assert_eq!(
        lines.next(),
        Some("frames\t2"),
        "the rate denominator must match the bounded run"
    );
    let declared: usize = lines
        .next()
        .and_then(|line| line.strip_prefix("sites\t"))
        .expect("site population")
        .parse()
        .expect("numeric site population");
    let rows = lines
        .map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 6, "frame-aware activation row: {line}");
            let values = fields[1..]
                .iter()
                .map(|value| value.parse::<u64>().expect("numeric activation field"))
                .collect::<Vec<_>>();
            assert_eq!(values[0], values[1] + values[4], "classified total: {line}");
            assert!(values[2] <= 2, "active-frame population: {line}");
            assert!(values[3] <= values[1], "per-frame maximum: {line}");
            (fields[0], values)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        declared,
        "declared and recorded site populations differ"
    );
    assert!(
        declared > 0,
        "the bounded workload activated no compiler site"
    );
    assert!(
        rows.iter().any(|(_, values)| values[1] > 1),
        "no site repeated across two frames"
    );
    let manifest_rows = manifest_text
        .lines()
        .filter_map(|line| {
            let fields = line.trim_start().split('\t').collect::<Vec<_>>();
            (fields.len() == 10
                && fields[0].len() == 16
                && fields[4].parse::<u32>().is_ok_and(|line| line > 0)
                && fields[5].parse::<u32>().is_ok()
                && fields[7].parse::<u32>().is_ok()
                && fields[9].parse::<u32>().is_ok())
            .then_some(fields[0])
        })
        .collect::<Vec<_>>();
    let manifest_ids = manifest_rows
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        manifest_rows.len(),
        manifest_ids.len(),
        "an artifact-local ID must identify exactly one emitted site"
    );
    assert!(
        rows.iter().all(|(id, _)| manifest_ids.contains(id)),
        "a runtime site was absent from the compiler manifest",
    );

    let lua_artifact = if lua.is_dir() {
        lua.join("main.lua")
    } else {
        lua.clone()
    };
    assert!(lua_artifact.is_file(), "generated Lua artifact is missing");
    let emitted = std::fs::read_to_string(&lua_artifact).expect("emitted Lua");
    assert!(emitted.lines().any(|line| line.starts_with("__rock_mk_")
        && line.contains(" return function(")), "default callee-bound emission is required");
    let joined = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(repo_root())
        .args(["allocation-join"])
        .arg(&manifest)
        .arg(&counts)
        .arg("all")
        .arg(&lua_artifact)
        .output()
        .expect("join allocation sites to generated Lua");
    assert!(
        joined.status.success(),
        "allocation join failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&joined.stdout),
        String::from_utf8_lossy(&joined.stderr),
    );
    let joined = String::from_utf8(joined.stdout).expect("UTF-8 joined report");
    assert!(joined.starts_with("rock.allocation-join.v12\n"));
    let mut lines = joined.lines().skip(4);
    let header = lines.next().expect("joined report header");
    let artifact_line = header
        .split('\t')
        .position(|column| column == "lua_artifact_line")
        .expect("generated Lua column");
    let direct_caller_count = header
        .split('\t')
        .position(|column| column == "direct_caller_count")
        .expect("direct caller count column");
    let direct_callers = header
        .split('\t')
        .position(|column| column == "direct_callers")
        .expect("direct callers column");
    let joined_rows = lines
        .map(|line| line.split('\t').collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(joined_rows.len(), declared, "joined active-site population");
    assert!(
        joined_rows
            .iter()
            .all(|row| row[artifact_line].parse::<u32>().is_ok_and(|line| line > 0)),
        "every active site must resolve to an exact generated Lua line",
    );
    assert!(
        joined_rows.iter().any(|row| row[direct_caller_count]
            .parse::<usize>()
            .is_ok_and(|count| count > 1)),
        "the populated artifact must expose at least one shared allocation body",
    );
    assert!(
        joined_rows.iter().all(|row| {
            let count = row[direct_caller_count]
                .parse::<usize>()
                .expect("numeric direct caller count");
            let callers = row[direct_callers];
            let names = if callers.is_empty() {
                0
            } else {
                callers.split(" | ").count()
            };
            count == names
        }),
        "every caller count must match its named caller population",
    );
}

#[test]
#[ignore = "spawns an instrumented sim (-P full)"]
fn sim_command_writes_one_frame_aware_join_per_selected_sim() {
    let scratch = tempfile::tempdir().expect("scratch");
    let heatmap = scratch.path().join("heatmap");
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(repo_root())
        .args(["sim", "--allocation-sites"])
        .arg(&heatmap)
        .args(["testbed", "testbed::sim_allocation_allocating", "testbed::sim_allocation_control"])
        .env("ROCK_SIM_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn allocation-site sim matrix");
    assert!(
        output.status.success(),
        "allocation-site command failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(heatmap.join("index.json")).expect("heat-map index"))
            .expect("valid heat-map index");
    assert_eq!(index["schema"], "rock.sim-allocation-sites.v1");
    assert_eq!(index["examined"], 2);
    assert_eq!(index["sensor"]["jit"], true);
    assert_eq!(index["sensor"]["trace_observer"], false);
    assert_eq!(index["sensor"]["frame_envelope"], "update_and_draw");
    assert_eq!(index["sensor"]["text_layout_sim_probes"], false);
    assert_eq!(index["sensor"]["imgui_sim_probes"], false);
    let records = index["records"].as_array().expect("records");
    assert_eq!(records.len(), 2);
    for record in records {
        assert_eq!(record["status"], "PASS", "record: {record}");
        assert!(record["completed_frames"].as_u64().is_some_and(|n| n > 0));
        assert!(record["active_sites"].as_u64().is_some_and(|n| n > 0));
        let activation = heatmap.join(record["activation"].as_str().expect("activation path"));
        let joined = heatmap.join(record["joined"].as_str().expect("joined path"));
        assert!(activation.is_file(), "missing {}", activation.display());
        let joined = std::fs::read_to_string(&joined)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", joined.display()));
        assert!(joined.starts_with("rock.allocation-join.v12\n"));
        assert!(joined.contains("frame_coverage"));
        assert!(joined.contains("structural_loop_depth"));
    }
    assert!(heatmap.join("manifest.txt").is_file());
}
