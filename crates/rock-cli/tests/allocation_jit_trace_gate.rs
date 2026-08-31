use std::process::Command;

#[test]
fn luajit_trace_maps_sunk_and_retained_allocations_to_source_lines() {
    let scratch = tempfile::tempdir().expect("scratch");
    let probe = scratch.path().join("probe.lua");
    let trace = scratch.path().join("trace.tsv");
    std::fs::write(
        &probe,
        r#"local retained = {}

function sunk_value(i)
  local value = {i, i + 1}
  return value[1] + value[2]
end

function retained_value(i)
  local value = {i, i + 1}
  retained[i] = value
end

local sum = 0
for i = 1, 1000 do
  sum = sum + sunk_value(i)
end
for i = 1, 1000 do
  retained_value(i)
end
assert(sum == 1002000)
"#,
    )
    .expect("probe source");

    let output = Command::new(rock_host::project::luajit_path())
        .arg("-e")
        .arg(
            "local a=require('tools.luajit.allocation_trace'); \
             a.start(os.getenv('ROCK_TRACE'), os.getenv('ROCK_PROBE')); \
             dofile(os.getenv('ROCK_PROBE')); a.stop()",
        )
        .env("LUA_PATH", rock_host::project::lua_path_value())
        .env("ROCK_TRACE", &trace)
        .env("ROCK_PROBE", &probe)
        .output()
        .expect("run LuaJIT allocation trace probe");
    assert!(
        output.status.success(),
        "trace probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let trace = std::fs::read_to_string(trace).expect("trace artifact");
    assert!(trace.starts_with("rock.jit-allocation-trace.v2\n"));
    let rows = trace.lines().skip(3).collect::<Vec<_>>();
    assert!(!rows.is_empty(), "trace contains no allocation IR rows");
    assert!(
        rows.iter().any(|row| row.contains("\tsunk\tTNEW\t")),
        "trace did not identify the scalar-consumed table as sunk:\n{trace}",
    );
    assert!(
        rows.iter().any(|row| row.contains("\tretained\tTNEW\t")),
        "trace did not identify the retained table allocation:\n{trace}",
    );
}

#[test]
fn luajit_trace_distinguishes_trace_ids_reused_after_flush() {
    let scratch = tempfile::tempdir().expect("scratch");
    let probe = scratch.path().join("probe.lua");
    let trace = scratch.path().join("trace.tsv");
    std::fs::write(
        &probe,
        r#"local retained = {}

local function retained_value(first, last)
  for i = first, last do
    retained[i] = {i, i + 1}
  end
end

retained_value(1, 1000)
jit.flush()
retained_value(1001, 2000)
assert(retained[2000][1] == 2000)
"#,
    )
    .expect("trace reuse probe source");

    let output = Command::new(rock_host::project::luajit_path())
        .arg("-e")
        .arg(
            "local a=require('tools.luajit.allocation_trace'); \
             a.start(os.getenv('ROCK_TRACE'), os.getenv('ROCK_PROBE')); \
             dofile(os.getenv('ROCK_PROBE')); a.stop()",
        )
        .env("LUA_PATH", rock_host::project::lua_path_value())
        .env("ROCK_TRACE", &trace)
        .env("ROCK_PROBE", &probe)
        .output()
        .expect("run trace reuse probe");
    assert!(
        output.status.success(),
        "trace reuse probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let trace = std::fs::read_to_string(trace).expect("trace artifact");
    assert!(trace.starts_with("rock.jit-allocation-trace.v2\n"));
    let rows = trace.lines().skip(3).collect::<Vec<_>>();
    assert!(!rows.is_empty(), "trace contains no allocation IR rows");
    assert!(
        rows.iter().any(|row| row.starts_with("0\t"))
            && rows.iter().any(|row| row.starts_with("1\t")),
        "trace did not record both generations around jit.flush():\n{trace}",
    );
}
