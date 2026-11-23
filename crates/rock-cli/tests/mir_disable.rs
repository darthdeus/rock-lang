use crate::support::{rock, text};

#[test]
fn unknown_disabled_pass_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rock"), "fn main() { print(1) }").unwrap();
    let out = rock(dir.path())
        .args(["run", "main.rock"])
        .env("ROCK_MIR_DISABLE", "no_such_pass")
        .output()
        .unwrap();
    assert!(!out.status.success() && text(&out).contains("unknown pass `no_such_pass`"));
}
