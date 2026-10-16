use std::path::Path;

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
}

#[test]
fn arena_free_slot_reuse_is_allocation_free() {
    let output = crate::command::rock_bin()
        .args(["run", "benchmarks/arena-reuse-alloc.rock"])
        .current_dir(workspace_root())
        .output()
        .expect("failed to run arena reuse allocation bench");
    assert!(
        output.status.success(),
        "arena reuse bench exited with {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let bytes = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("arena reuse bytes/op:"))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .unwrap_or_else(|| panic!("missing bytes/op line:\n{stdout}"));
    assert!(
        bytes < 1.0,
        "arena free-slot reuse allocates {bytes} bytes/op (gate: < 1.0)\n{stdout}"
    );
    assert!(stdout.contains("RESULT: OK"), "bench did not report OK:\n{stdout}");
}
