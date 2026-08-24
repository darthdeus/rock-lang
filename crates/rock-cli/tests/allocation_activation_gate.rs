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
            let mut fields = line.trim_start().split('\t');
            let id = fields.next()?;
            (id.len() == 16 && fields.clone().count() == 3).then_some(id)
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
}
