//! Coverage for the `rock completions <shell>` subcommand.
//!
//! These shell out to the actual `rock` binary so we exercise the
//! full clap parse + dispatch path.
//!
//! `cargo nextest` builds the binary on demand via env! before running.

#[test]
fn completions_zsh_emits_compdef_block() {
    let out = crate::command::rock_bin()
        .args(["completions", "zsh"])
        .output()
        .expect("failed to spawn rock");
    assert!(
        out.status.success(),
        "rock completions zsh failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("non-utf8 zsh script");
    assert!(
        stdout.contains("#compdef rock"),
        "expected #compdef header in zsh completion script, got:\n{}",
        stdout
    );
}

#[test]
fn completions_bash_emits_complete_block() {
    let out = crate::command::rock_bin()
        .args(["completions", "bash"])
        .output()
        .expect("failed to spawn rock");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("non-utf8 bash script");
    // bash completion scripts always end with `complete -F _<name> <name>`.
    assert!(
        stdout.contains("complete -F") && stdout.contains("rock"),
        "expected `complete -F` in bash script, got:\n{}",
        stdout
    );
}

