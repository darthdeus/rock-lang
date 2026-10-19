//! One child-thread default for every rock-cli integration harness.
use std::ffi::OsStr;
use std::process::Command;

pub fn rock_bin() -> Command {
    rock(env!("CARGO_BIN_EXE_rock-plain"))
}

pub fn rock(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("ROCK_NUM_THREADS", "1");
    command
}

#[test]
fn rock_children_default_to_one_thread() {
    let command = rock_bin();
    let value = command.get_envs().find(|(key, _)| *key == "ROCK_NUM_THREADS")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("1")));
}

#[test]
fn explicit_thread_width_overrides_the_harness_default() {
    let mut command = rock_bin();
    command.env("ROCK_NUM_THREADS", "4");
    let value = command.get_envs().find(|(key, _)| *key == "ROCK_NUM_THREADS")
        .and_then(|(_, value)| value);
    assert_eq!(value, Some(OsStr::new("4")));
}
