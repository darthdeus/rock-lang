//! Cargo subprocess invocation shared by compiler rebuilds and host clients.
use anyhow::Result;
use std::process::{Command, Stdio};

/// Cargo harness variables describing the package cargo itself is building or
/// testing, rather than real user settings. `cargo test` exports these into the test
/// process. Nested Cargo commands must remove them so dependency build scripts
/// fingerprint the actual invocation rather than their parent's package.
///
/// `CARGO_TARGET_DIR` is deliberately NOT here: it is a real user setting
/// that Cargo honors.
pub(crate) const CARGO_HARNESS_ENV_VARS: &[&str] = &[
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_MANIFEST_LINKS",
    "CARGO_CRATE_NAME",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_TMPDIR",
];
/// Prefix of the per-package harness variables (`CARGO_PKG_NAME`,
/// `CARGO_PKG_VERSION_*`, ...), which share the fate above.
pub(crate) const CARGO_HARNESS_ENV_PREFIX: &str = "CARGO_PKG_";

/// Remove the harness variables from a nested cargo invocation so it
/// fingerprints like a plain-shell build.
pub fn strip_cargo_harness_env(cmd: &mut Command) {
    for var in CARGO_HARNESS_ENV_VARS {
        cmd.env_remove(var);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with(CARGO_HARNESS_ENV_PREFIX) {
            cmd.env_remove(name);
        }
    }
}

/// Run `cargo <args>` with inherited stdio, bailing with `fail_msg`
/// if cargo exits non-zero. Single place for the build-invoke +
/// status-check shared by compiler and native-library builders.
pub fn run_cargo(args: &[&str], fail_msg: &str) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.args(args).current_dir(crate::rock_dir());
    strip_cargo_harness_env(&mut cmd);
    let status = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        anyhow::bail!("{}", fail_msg);
    }
    Ok(())
}
