//! Allocation gate for the unified `Metric` (stdlib). `metric_add` is called
//! per-frame per-metric by the profiler, so it must allocate zero bytes on the
//! steady-state hot path. Runs benchmarks/stats/metric_alloc.rock (a GC-stopped
//! fixed-workload bench) and asserts the measured bytes/call stays at noise.
//!
//! This is the trustworthy oracle per CLAUDE.md: a fixed standalone workload,
//! not the in-game true_alloc (which swings ~80 KB/frame run-to-run).

use std::path::Path;
use std::process::Command;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
}

fn run_bench(backend: &str) -> String {
    let rock_bin = env!("CARGO_BIN_EXE_rock");
    let output = Command::new(rock_bin)
        .args([
            "run",
            "benchmarks/stats/metric_alloc.rock",
            "--backend",
            backend,
        ])
        .current_dir(workspace_root())
        .output()
        .expect("failed to spawn rock run");

    assert!(
        output.status.success(),
        "rock run metric_alloc ({backend}) exited with {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn bytes_per_call(stdout: &str) -> f64 {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("metric_add bytes/call:"))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or_else(|| panic!("no bytes/call line in bench output:\n{stdout}"))
}

fn assert_alloc_free(backend: &str) {
    let stdout = run_bench(backend);
    let per_call = bytes_per_call(&stdout);
    assert!(
        per_call < 0.5,
        "metric_add ({backend}) allocates {per_call} bytes/call (gate: < 0.5)\n{stdout}"
    );
    assert!(
        stdout.contains("RESULT: OK"),
        "bench did not report OK ({backend}):\n{stdout}"
    );
}

#[test]
fn metric_add_is_allocation_free_skull() {
    // skull is the default (production game) backend.
    assert_alloc_free("skull");
}
