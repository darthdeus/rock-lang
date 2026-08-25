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
    assert_eq!(lines.next(), Some("rock.allocation-activation.v1"));
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
            let (id, count) = line.split_once('\t').expect("id/count row");
            let count: u64 = count.parse().expect("numeric activation count");
            (id, count)
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
        rows.iter().any(|(_, count)| *count > 1),
        "no site repeated across two frames"
    );
    let manifest_rows = manifest_text
        .lines()
        .filter_map(|line| {
            let fields = line.trim_start().split('\t').collect::<Vec<_>>();
            (fields.len() == 5
                && fields[0].len() == 16
                && fields[4].parse::<u32>().is_ok_and(|line| line > 0))
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
    assert!(joined.starts_with("rock.allocation-join.v4\n"));
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
