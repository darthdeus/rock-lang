//! One child-thread default for every rock-cli integration harness.
use std::ffi::OsStr;
use std::process::Command;

/// Set to skip, instead of fail, the tests that run emitted JS when `node` is
/// not on PATH. CI has node, so a missing binary there must not read as a pass.
pub const SKIP_NODE_ENV: &str = "ROCK_SKIP_NODE_TESTS";

/// True when `node` runs. When it does not, panics unless [`SKIP_NODE_ENV`] is
/// set, in which case it logs the skip and returns false.
pub fn require_node() -> bool {
    if Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return true;
    }
    assert!(
        std::env::var_os(SKIP_NODE_ENV).is_some(),
        "`node` is not on PATH; install it or set {SKIP_NODE_ENV}=1 to skip the tests that run JS"
    );
    eprintln!("SKIP: `node` not on PATH ({SKIP_NODE_ENV} is set)");
    false
}

pub fn rock_bin() -> Command {
    rock(env!("CARGO_BIN_EXE_rock"))
}

pub fn rock(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("ROCK_NUM_THREADS", "1");
    command
}

#[test]
fn rock_children_default_to_one_thread() {
    let command = rock_bin();
    let value = command
        .get_envs()
        .find(|(key, _)| *key == "ROCK_NUM_THREADS")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("1")));
}

#[test]
fn explicit_thread_width_overrides_the_harness_default() {
    let mut command = rock_bin();
    command.env("ROCK_NUM_THREADS", "4");
    let value = command
        .get_envs()
        .find(|(key, _)| *key == "ROCK_NUM_THREADS")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("4")));
}
