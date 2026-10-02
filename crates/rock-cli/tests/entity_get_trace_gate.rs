use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
}

fn metric(stdout: &str, name: &str) -> u64 {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(name))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("missing `{name}` line:\n{stdout}"))
}

// `Entity<T>.get()` is inlined into every LuaJIT trace that reads a row, and
// each validation guard adds IR, a snapshot and snapshot-map entries to that
// trace (ledger B2a, ENTITY READ TRACE COST). Before the getter was slimmed the
// witness loop recorded 43 exits and 1,186 map entries; after, 23 and 462.
// Folding occupancy into the generation column then took the loop from 210 IR
// instructions to 186 without changing its snapshots.
#[test]
fn entity_get_keeps_row_reading_traces_small() {
    let output = Command::new(env!("CARGO_BIN_EXE_rock"))
        .args(["run", "benchmarks/entity-get-trace.rock"])
        .current_dir(workspace_root())
        .output()
        .expect("failed to run entity get trace witness");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "entity get trace witness exited with {:?}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let ir = metric(&stdout, "entity get loop trace IR:");
    let exits = metric(&stdout, "entity get loop trace exits:");
    let map = metric(&stdout, "entity get loop trace snapshot map:");
    assert!(ir <= 195, "row-reading loop trace has {ir} IR instructions (gate: <= 195)\n{stdout}");
    assert!(exits <= 30, "row-reading loop trace has {exits} exits (gate: <= 30)\n{stdout}");
    assert!(map <= 700, "row-reading loop trace has {map} snapshot-map entries (gate: <= 700)\n{stdout}");
}
