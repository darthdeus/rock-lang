//! `tests/flat_element_ref.rock` writes through every `ref` borrow form of a
//! flat-stored container element (Vec/Map/Arena value columns, strided rows,
//! `Arena.borrow`, `Entity.borrow`). Its in-program oracle is the value each
//! mutator reads back through the `ref`; LuaJIT and C must both satisfy it.

use std::process::Command;

const TESTS: &[&str] = &[
    "fer_vec_whole_element_arg_writes_the_element",
    "fer_vec_nested_field_arg_writes_the_element",
    "fer_vec_deep_prefix_args_write_the_element",
    "fer_vec_ref_param_passed_on_writes_the_element",
    "fer_increments_through_ref_and_index_write_the_element",
    "fer_tuple_returning_callee_writes_the_element",
    "fer_map_whole_element_arg_writes_the_element",
    "fer_map_nested_field_arg_writes_the_element",
    "fer_arena_borrow_writes_the_element",
    "fer_entity_borrow_writes_the_element",
    "fer_arena_borrow_of_a_removed_entity_is_null",
    "fer_strided_row_args_write_the_element",
    "fer_recursive_ref_return_writes_the_element",
    "fer_mutually_recursive_ref_return_writes_the_element",
];

fn run_on(backend: Option<&str>) {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rock"));
    cmd.arg("test");
    if let Some(backend) = backend {
        cmd.args(["--backend", backend]);
    }
    cmd.arg("tests/flat_element_ref.rock");
    // `--require-test` is C-only; the Lua runner names each result in its
    // timings file instead.
    let timings = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("flat_element_ref.timings.jsonl");
    match backend {
        Some(_) => {
            for name in TESTS {
                cmd.args(["--require-test", name]);
            }
        }
        None => {
            let _ = std::fs::remove_file(&timings);
            cmd.env("ROCK_TEST_TIMINGS", &timings);
        }
    }
    let output = cmd.current_dir(root).output().expect("run rock test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let label = backend.unwrap_or("lua");
    assert!(
        output.status.success(),
        "{label}: flat element ref tests failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let passed = format!("{n}/{n} @tests passed (0 failed", n = TESTS.len());
    assert!(stdout.contains(&passed), "{label}: expected `{passed}`:\n{stdout}");
    if backend.is_none() {
        let lines = std::fs::read_to_string(&timings).expect("read Lua test timings");
        for name in TESTS {
            let quoted = format!("\"name\":\"{name}\"");
            assert!(
                lines.lines().any(|l| l.contains(&quoted) && l.contains("\"status\":\"pass\"")),
                "lua: `{name}` did not pass:\n{lines}"
            );
        }
    }
}

#[test]
fn flat_element_ref_writes_pass_on_luajit() {
    run_on(None);
}

#[test]
fn flat_element_ref_writes_pass_on_c() {
    run_on(Some("c"));
}
