//! `ROCK_TEST_SHARDS` splits a package's `@test`s across several LuaJIT
//! processes over one emitted program. Every selected test must run exactly
//! once, the merged summary must keep the full population, and a shard that
//! dies must name the tests it never finished instead of shrinking the total.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().canonicalize().unwrap()
}

/// Each test appends `<name> <pid>` to `$SHARD_LOG`, so the host can see which
/// process ran it and how often.
const LIB: &str = r#"
pub inline(lua) fn record(name: string) = """
local ffi = require("ffi")
pcall(ffi.cdef, "int getpid(void);")
local f = assert(io.open(os.getenv("SHARD_LOG"), "a"))
f:write(name, " ", tostring(ffi.C.getpid()), "\n")
f:close()
"""
pub inline(lua) fn quit() = """ os.exit(0) """
pub inline(lua) fn crash() = """
io.stderr:write("shard fixture stderr marker\n")
os.exit(23)
"""
pub inline(lua) fn record_scratch(name: string) = """
local ffi = require("ffi")
pcall(ffi.cdef, "int getpid(void);")
local tmp = os.getenv("ROCK_TEST_TMP") or ""
local probe = io.open(tmp .. "/probe", "w")
if probe then probe:close() end
local f = assert(io.open(os.getenv("SHARD_LOG"), "a"))
f:write(name, " ", tostring(ffi.C.getpid()), " ", tmp, " ", tostring(probe ~= nil), "\n")
f:close()
"""
"#;

fn package(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("shard fixture package");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("stick.toml"),
        "[package]\nname = \"test-shards\"\ntype = \"lib\"\n\
         [language]\nmodules = \"v1\"\n").unwrap();
    std::fs::write(project.join("lib.rock"), LIB).unwrap();
    for (name, body) in files {
        std::fs::write(project.join("tests").join(name), body).unwrap();
    }
    (dir, project)
}

struct Run {
    success: bool,
    text: String,
    log: String,
    timings: String,
}

fn run(project: &Path, shards: &str) -> Run {
    run_filtered(project, Some(shards), None)
}

fn run_filtered(project: &Path, shards: Option<&str>, filter: Option<&str>) -> Run {
    run_filtered_with_verbose(project, shards, filter, true)
}

fn run_filtered_with_verbose(project: &Path, shards: Option<&str>, filter: Option<&str>, verbose: bool) -> Run {
    let log = project.join("shard.log");
    let timings = project.join("timings.jsonl");
    let mut command = crate::command::rock_bin();
    command.arg("test").arg(project.join("tests"))
        .current_dir(root())
        .env("ROCK_TEST_TIMINGS", &timings)
        .env("SHARD_LOG", &log)
        .env_remove("ROCK_TEST_FILTER");
    if verbose {
        command.env("ROCK_TEST_VERBOSE", "1");
    } else {
        command.env_remove("ROCK_TEST_VERBOSE");
    }
    match shards {
        Some(shards) => command.env("ROCK_TEST_SHARDS", shards),
        None => command.env_remove("ROCK_TEST_SHARDS"),
    };
    if let Some(filter) = filter {
        command.env("ROCK_TEST_FILTER", filter);
    }
    let output = command.output().expect("run shard fixture package test");
    Run {
        success: output.status.success(),
        text: format!("{}{}", String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)),
        log: std::fs::read_to_string(&log).unwrap_or_default(),
        timings: std::fs::read_to_string(&timings).unwrap_or_default(),
    }
}

fn summary(text: &str) -> (usize, usize, usize) {
    let line = text.lines().find(|l| l.contains("@tests passed"))
        .unwrap_or_else(|| panic!("no summary line:\n{text}"));
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>()
        .parse::<usize>().unwrap();
    let after = line.split("skull backend: ").nth(1).unwrap();
    let (passed, rest) = after.split_once('/').unwrap();
    let (total, rest) = rest.split_once(" @tests passed (").unwrap();
    let failed = rest.split_once(" failed").unwrap().0;
    (digits(passed), digits(total), digits(failed))
}

const ALPHA: &str = r#"
@test fn alpha_1() { record("alpha_1") }
@test fn alpha_2() { record("alpha_2") }
@test fn alpha_3() { record("alpha_3") }
"#;
const BETA: &str = r#"
@test fn beta_1() { record("beta_1") }
@test fn beta_2() { record("beta_2"); assert(false, "deliberate shard failure") }
@test fn beta_3() { record("beta_3") }
"#;
const GAMMA: &str = r#"
@test fn gamma_1() { record("gamma_1") }
@test fn gamma_2() { record("gamma_2") }
"#;

#[test]
fn three_shards_run_every_test_once_and_merge_results() {
    let (_dir, project) = package(&[("alpha.rock", ALPHA), ("beta.rock", BETA), ("gamma.rock", GAMMA)]);
    let run = run(&project, "3");
    let text = &run.text;

    let mut runs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pids = BTreeSet::new();
    for line in run.log.lines() {
        let (name, pid) = line.split_once(' ').unwrap();
        *runs.entry(name).or_default() += 1;
        pids.insert(pid);
    }
    let expected = ["alpha_1", "alpha_2", "alpha_3", "beta_1", "beta_2", "beta_3", "gamma_1", "gamma_2"];
    assert_eq!(runs.keys().copied().collect::<Vec<_>>(), expected, "every test ran:\n{text}");
    assert!(runs.values().all(|&n| n == 1), "every test ran exactly once: {runs:?}\n{text}");
    assert_eq!(pids.len(), 3, "three LuaJIT processes ran the tests: {pids:?}\n{text}");

    assert_eq!(summary(text), (7, 8, 1), "merged counts:\n{text}");
    assert!(text.contains("beta_2: error:") && text.contains("deliberate shard failure"),
        "the failing test is reported by name:\n{text}");
    assert!(text.contains("@test shards: 3"), "the shard count is stated:\n{text}");
    assert!(!run.success, "a failing test fails the run:\n{text}");
    assert_eq!(run.timings.lines().count(), 8, "one timing row per test:\n{}", run.timings);
}

const DYING: &str = r#"
@test fn d_0() { record("d_0") }
@test fn d_1() { record("d_1"); quit() }
@test fn d_2() { record("d_2") }
@test fn d_3() { record("d_3") }
@test fn d_4() { record("d_4") }
@test fn d_5() { record("d_5") }
"#;

/// A shard that exits midway keeps every test it owned in the total and names
/// each one it did not finish; the other shard's results still count.
#[test]
fn a_dying_shard_names_its_unfinished_tests() {
    for shards in ["1", "2"] {
        let (_dir, project) = package(&[("dying.rock", DYING)]);
        let run = run(&project, shards);
        let text = &run.text;
        let (passed, total, failed) = summary(text);
        assert_eq!(total, 6, "shards={shards}: the total keeps every declared test:\n{text}");
        assert_eq!(passed + failed, 6, "shards={shards}:\n{text}");
        assert!(!run.success, "shards={shards}: a truncated run fails:\n{text}");
        let finished = run.timings.lines()
            .map(|row| row.split("\"name\":\"").nth(1).unwrap().split('"').next().unwrap()
                .rsplit("::").next().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        for name in ["d_0", "d_1", "d_2", "d_3", "d_4", "d_5"] {
            assert!(finished.contains(name) || text.contains(&format!("{name}: never reported")),
                "shards={shards}: `{name}` neither finished nor named as unfinished:\n{text}");
        }
        assert!(text.contains("d_1: never reported"), "shards={shards}: the dying test is named:\n{text}");
        assert_eq!(passed, finished.len(), "shards={shards}: finished tests passed:\n{text}");
    }
}

#[test]
fn a_dying_shard_reports_cause_without_verbose_mode() {
    let source = r#"
@test fn crash_test() { crash() }
@test fn after_crash() { record("after_crash") }
"#;
    let (_dir, project) = package(&[("crash.rock", source)]);
    let run = run_filtered_with_verbose(&project, Some("1"), None, false);
    let text = &run.text;
    let oracle = Command::new("sh")
        .arg("-c")
        .arg("printf 'shard fixture stderr marker\\n' >&2; exit 23")
        .output()
        .expect("run independent child-status and stderr oracle");
    let exit_code = oracle.status.code().expect("shell reports its exit code");
    let stderr = String::from_utf8_lossy(&oracle.stderr);

    assert!(!run.success, "the crashed shard must fail the command:\n{text}");
    assert!(text.contains("crash_test: never reported"), "the active test is named:\n{text}");
    assert!(text.contains("stopped: child process stopped"), "the stop cause is shown:\n{text}");
    assert!(
        !oracle.status.success() && text.contains(&format!("exit code {exit_code}")),
        "the child's observed exit status is shown:\n{text}"
    );
    let test_module = module_path!().rsplit("::").next().expect("test module path");
    assert!(
        text.contains(&format!("running test `{test_module}::crash_test`")),
        "the running test is identified:\n{text}"
    );
    assert!(text.contains(stderr.trim()), "the independently captured child stderr is shown:\n{text}");
    assert!(!text.contains("=== failure details ==="), "the fixture did not enable verbose mode:\n{text}");
}

/// The filter selects first and the shards split only the selection: each
/// selected test runs once, no skipped test runs, and the summary counts the
/// selection.
#[test]
fn shards_split_only_the_filtered_selection() {
    let (_dir, project) = package(&[("alpha.rock", ALPHA), ("beta.rock", BETA), ("gamma.rock", GAMMA)]);
    let run = run_filtered(&project, Some("2"), Some("alpha_,gamma.rock"));
    let text = &run.text;
    let mut runs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pids = BTreeSet::new();
    for line in run.log.lines() {
        let (name, pid) = line.split_once(' ').unwrap();
        *runs.entry(name).or_default() += 1;
        pids.insert(pid);
    }
    let expected = ["alpha_1", "alpha_2", "alpha_3", "gamma_1", "gamma_2"];
    assert_eq!(runs.keys().copied().collect::<Vec<_>>(), expected, "only selected tests ran:\n{text}");
    assert!(runs.values().all(|&n| n == 1), "every selected test ran once: {runs:?}\n{text}");
    assert_eq!(pids.len(), 2, "two LuaJIT processes split the selection: {pids:?}\n{text}");
    assert_eq!(summary(text), (5, 5, 0), "the summary counts the selection:\n{text}");
    assert!(text.contains("5 selected, 3 skipped"), "skips are reported:\n{text}");
    assert!(run.success, "the selection passes:\n{text}");
}

/// A dying shard under a filter names only selected tests as unfinished;
/// skipped tests are neither run nor reported as never-reported.
#[test]
fn a_dying_filtered_shard_names_only_selected_tests() {
    for shards in ["1", "2"] {
        let (_dir, project) = package(&[("dying.rock", DYING)]);
        let run = run_filtered(&project, Some(shards), Some("d_1,d_2,d_3"));
        let text = &run.text;
        let (passed, total, failed) = summary(text);
        assert_eq!(total, 3, "shards={shards}: the total is the selection:\n{text}");
        assert_eq!(passed + failed, 3, "shards={shards}:\n{text}");
        assert!(!run.success, "shards={shards}: a truncated run fails:\n{text}");
        assert!(text.contains("d_1: never reported"), "shards={shards}: the dying test is named:\n{text}");
        for skipped in ["d_0", "d_4", "d_5"] {
            assert!(!text.contains(&format!("{skipped}: never reported")),
                "shards={shards}: skipped `{skipped}` is not unfinished:\n{text}");
            assert!(!run.log.lines().any(|l| l.starts_with(&format!("{skipped} "))),
                "shards={shards}: skipped `{skipped}` ran:\n{}", run.log);
        }
        assert!(text.contains("3 selected, 3 skipped"), "shards={shards}: skips are reported:\n{text}");
    }
}

const SCRATCH: &str = r#"
@test fn s_0() { record_scratch("s_0") }
@test fn s_1() { record_scratch("s_1") }
@test fn s_2() { record_scratch("s_2") }
@test fn s_3() { record_scratch("s_3") }
"#;

/// Shards share one worktree, so each test process gets its own writable
/// `ROCK_TEST_TMP`, removed after the run.
#[test]
fn each_shard_process_gets_its_own_scratch_dir() {
    let (_dir, project) = package(&[("scratch.rock", SCRATCH)]);
    let run = run(&project, "2");
    let text = &run.text;
    assert!(run.success, "scratch fixture passes:\n{text}");
    let mut by_pid: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in run.log.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields.len(), 4, "row: {line}");
        assert_eq!(fields[3], "true", "the scratch dir is writable during the run: {line}");
        by_pid.entry(fields[1].to_owned()).or_default().insert(fields[2].to_owned());
    }
    assert_eq!(by_pid.len(), 2, "two processes ran:\n{}", run.log);
    let dirs: BTreeSet<&String> = by_pid.values().flatten().collect();
    assert!(by_pid.values().all(|d| d.len() == 1), "one scratch dir per process:\n{}", run.log);
    assert_eq!(dirs.len(), 2, "processes do not share a scratch dir:\n{}", run.log);
    for dir in dirs {
        assert!(!Path::new(dir).exists(), "`{dir}` is removed after the run");
    }
}

#[test]
fn default_small_packages_use_one_child() {
    for count in [3, 60] {
        let source = (0..count).map(|i| format!(
            "@test fn small_{i}() {{ record(\"small_{i}\") }}\n"
        )).collect::<String>();
        let (_dir, project) = package(&[("small.rock", &source)]);
        let run = run_filtered(&project, None, None);
        assert!(run.success, "{}", run.text);
        let pids = run.log.lines().map(|line| line.split_once(' ').unwrap().1)
            .collect::<BTreeSet<_>>();
        assert_eq!(pids.len(), 1, "count={count}: one child: {}", run.text);
        assert_eq!(summary(&run.text), (count, count, 0));
        assert_eq!(run.timings.lines().count(), count);
    }
}

#[test]
fn inserting_a_test_does_not_move_existing_tests_between_shards() {
    let existing = r#"
@test fn alpha() { record("alpha") }
@test fn bravo() { record("bravo") }
@test fn charlie() { record("charlie") }
@test fn delta() { record("delta") }
"#;
    let inserted = r#"
@test fn alpha() { record("alpha") }
@test fn bravo() { record("bravo") }
@test fn inserted() { record("inserted") }
@test fn charlie() { record("charlie") }
@test fn delta() { record("delta") }
"#;
    let (_before_dir, before_project) = package(&[("tests.rock", existing)]);
    let (_after_dir, after_project) = package(&[("tests.rock", inserted)]);
    let before = run(&before_project, "2");
    let after = run(&after_project, "2");
    assert!(before.success, "baseline shard run failed:\n{}", before.text);
    assert!(after.success, "inserted shard run failed:\n{}", after.text);

    fn same_shard(log: &str, left: &str, right: &str) -> bool {
        let pids: BTreeMap<_, _> = log
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?, fields.next()?))
            })
            .collect();
        pids[left] == pids[right]
    }
    for (left, right) in [("alpha", "bravo"), ("alpha", "charlie"), ("alpha", "delta"),
        ("bravo", "charlie"), ("bravo", "delta"), ("charlie", "delta")]
    {
        assert_eq!(
            same_shard(&before.log, left, right),
            same_shard(&after.log, left, right),
            "inserting a test changed whether {left} and {right} share a shard\nbefore:\n{}\nafter:\n{}",
            before.log,
            after.log,
        );
    }
}
