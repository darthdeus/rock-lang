//! JS coroutine emission: the echo shape (two yields, resume x3),
//! resume-with-value, terminal return via result(), status/kill/last_error.
//! Runs identically on luajit; the JS trampoline emission must match -- no
//! skips.

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// The coroutine corpus, inline: the harness writes it to a temp file (it must
// not live under a scanned tests/ directory) and runs it in-process.
const CORPUS: &str = r#"// JS-backend coroutine coverage: the echo shape (two yields, resume x3),
// resume-with-value, terminal return via result(), status/kill/last_error.
// Runs identically on luajit; the JS trampoline emission must match.

co fn echo_two() -> Coroutine<string> {
    yield "hello"
    yield "world"
}

@test(output = "hello\nworld\nnil")
fn two_yield_resume() {
    let c = echo_two()
    print(c.resume())
    print(c.resume())
    print(c.resume())
}

co fn gen_from(start: int) -> Coroutine<int> {
    let i = start
    loop {
        yield i
        i = i + 1
    }
}

@test(output = "5\n6\n7")
fn counter_loop_yields() {
    let c = gen_from(5)
    print(c.resume())
    print(c.resume())
    print(c.resume())
}

co fn sum_in(n: int) -> Coroutine<int, int, int> {
    let total = 0
    let i = 0
    while i < n {
        let got = yield i
        total = total + got
        i = i + 1
    }
    return total
}

@test(output = "0\n1\n2\nnil\n60")
fn resume_with_value_and_result() {
    let c = sum_in(3)
    print(c.resume(0))
    print(c.resume(10))
    print(c.resume(20))
    print(c.resume(30))
    print(c.result())
}

@test(output = "suspended\ndead\nnil")
fn status_and_kill() {
    let c = echo_two()
    c.resume()
    print(c.status())
    c.kill()
    print(c.status())
    print(c.resume())
}

co fn watched() -> Coroutine<int> {
    on_killed(() => { print("killed!") })
    on_resume(() => { print("pumped") })
    defer { print("cleanup ran") }
    yield 1
    yield 2
}

@test(output = "pumped\n1\npumped\n2\ncleanup ran\nkilled!")
fn hooks_fire_on_kill() {
    let c = watched()
    print(c.resume())
    print(c.resume())
    c.kill()
}

co fn deferred() -> Coroutine<int, int> {
    defer { print("d1") }
    defer { print("d2") }
    yield 1
    return 99
}

@test(output = "1\nd2\nd1\nnil\n99")
fn defers_lifo_on_terminal() {
    let c = deferred()
    print(c.resume())
    print(c.resume())
    print(c.result())
}

co fn boom() -> Coroutine<int> {
    yield 1
    error("kaboom")
}

// `last_error` is null-checked only: Lua prefixes `file:line:` to the raised
// message while JS carries the bare message, so exact-print would diverge
// (substring `contains` pulls stdlib `find`, out of the JS subset).
@test(output = "1\nnil\nerrored\ntrue")
fn errored_state_and_last_error() {
    let c = boom()
    print(c.resume())
    print(c.resume())
    print(c.status())
    print(c.last_error() != null)
}

// Map iteration inside a co fn (materialize_frame must rewrite MapNext's framed
// cursor/value cells, else the cursor reads the frame field but writes a bare
// local and the loop never advances). Order-insensitive (running sum).
global CO_SCORES: Map<string, int> = map_new()

co fn yield_score_total() -> Coroutine<int> {
    let total = 0
    for (_k, v) in CO_SCORES {
        total = total + v
    }
    yield total
}

@test(output = "6")
fn map_iter_inside_coroutine() {
    CO_SCORES.clear()
    CO_SCORES["a"] = 1
    CO_SCORES["b"] = 2
    CO_SCORES["c"] = 3
    let c = yield_score_total()
    print(c.resume() ?? -1)
}
"#;

#[test]
fn js_coroutines_run_green_under_node() {
    if !node_available() {
        eprintln!("SKIP js_coroutines_run_green_under_node: `node` not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("fixture dir");
    let file = dir.path().join("js_coroutines.rock");
    std::fs::write(&file, CORPUS).expect("write fixture");
    let paths = vec![file.to_string_lossy().into_owned()];
    let summary = rock_cli::js_test::js_test_run(&paths, false).expect("js test run");

    assert_eq!(summary.unit_failures, 0, "no unit should fail to build/run");
    assert_eq!(summary.tests_failed, 0, "no test should FAIL under node");
    assert_eq!(
        summary.tests_skipped,
        0,
        "coroutine tests must emit, not skip. Skip histogram:\n{}",
        summary
            .skip_reasons
            .iter()
            .map(|(r, c)| format!("  {c:>4}  {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(summary.tests_passed, 8);
}
