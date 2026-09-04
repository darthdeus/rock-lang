//! Resolution of `[build.vars]` into concrete values, once per session build,
//! before compilation (a live `rock stone` session re-resolves on every full
//! rebuild, never per incremental update). Precedence: declaration `default` (for a
//! channel-restricted var outside its channels) < the var's source < explicit
//! `--var NAME=value` override. There is deliberately NO ambient-env fallback:
//! the steam release wrapper's whole job was refusing ambient release identity
//! (tools/telemetry/test_steam_release.py pins that), and the declared
//! mechanism must not reintroduce the leak.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use super::StickConfig;

#[derive(Debug, Clone)]
pub struct ResolvedVar {
    pub value: String,
    /// Metadata hygiene, not confidentiality (docs/comptime-plan.md): hashed
    /// into cache keys, ALLOWED in the compiled program payload, and kept out
    /// of logs, diagnostics, manifests and cache metadata.
    pub secret: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedBuildVars {
    pub channel: String,
    pub vars: HashMap<String, ResolvedVar>,
    pub volatile_vars: std::collections::HashSet<String>,
}

pub fn resolve_build_vars(
    cfg: &StickConfig,
    project_dir: &Path,
    channel_arg: Option<&str>,
    target_os_arg: Option<&str>,
    overrides: &HashMap<String, String>,
) -> Result<ResolvedBuildVars> {
    let target_os = target_os_arg.map(validate_target_os).transpose()?;
    let Some(build) = cfg.build.as_ref() else {
        if let Some(chan) = channel_arg {
            anyhow::bail!(
                "--channel {chan:?} given but stick.toml has no [build] section declaring channels"
            );
        }
        if let Some(target) = target_os_arg {
            anyhow::bail!(
                "--target-os {target:?} given but stick.toml has no [build] section"
            );
        }
        if let Some(name) = overrides.keys().next() {
            anyhow::bail!("--var {name} is not declared in [build.vars]");
        }
        return Ok(ResolvedBuildVars {
            channel: "dev".to_string(),
            vars: HashMap::new(),
            volatile_vars: std::collections::HashSet::new(),
        });
    };
    cfg.validate_build()?;

    let channel = channel_arg
        .or(build.default_channel.as_deref())
        .unwrap_or("dev")
        .to_string();
    // An EXPLICIT channel is validated even when the project declares no
    // channels list: an empty list can vouch for nothing, and accepting the
    // flag anyway stamps a typo into release identity with every gate green.
    if let Some(explicit) = channel_arg {
        if !build.channels.iter().any(|c| c == explicit) {
            anyhow::bail!(
                "channel {explicit:?} is not declared in [build] channels {:?}",
                build.channels
            );
        }
    } else if !build.channels.is_empty() && !build.channels.iter().any(|c| c == &channel) {
        anyhow::bail!(
            "channel {channel:?} is not declared in [build] channels {:?}",
            build.channels
        );
    }
    for name in overrides.keys() {
        if !build.vars.contains_key(name) {
            anyhow::bail!("--var {name} is not declared in [build.vars]");
        }
    }

    let mut vars = HashMap::new();
    for (name, var) in &build.vars {
        let value = if let Some(over) = overrides.get(name) {
            over.clone()
        } else if var.channels.as_deref()
            .is_some_and(|chans| !chans.iter().any(|c| c == &channel))
        {
            match var.default.clone() {
                Some(default) => default,
                None => anyhow::bail!(
                    "[build.vars] {name} is restricted to channels {:?} and has no default \
                     for channel {channel:?}",
                    var.channels.as_deref().unwrap_or(&[])
                ),
            }
        } else if let Some(value) = var.value.as_ref() {
            value.clone()
        } else if let Some(builtin) = var.builtin.as_deref() {
            resolve_builtin(builtin, &channel, target_os)?
        } else if let Some(cmd) = var.cmd.as_ref() {
            match run_var_cmd(name, cmd, var.secret, project_dir, &channel) {
                Ok(value) => value,
                Err(_error)
                    if var
                        .optional_channels
                        .as_deref()
                        .is_some_and(|channels| channels.iter().any(|entry| entry == &channel)) =>
                {
                    log::warn!(
                        "[build.vars] {name}: source unavailable for channel {channel:?}; continuing with its declared default"
                    );
                    var.default.clone().expect("validate_build requires an optional default")
                }
                Err(error) => return Err(error),
            }
        } else {
            // validate_build above rejected sourceless vars.
            unreachable!("[build.vars] {name} has no source past validate_build")
        };
        vars.insert(name.clone(), ResolvedVar { value, secret: var.secret });
    }
    let volatile_vars = build
        .vars
        .iter()
        .filter(|(_, var)| var.volatile)
        .map(|(name, _)| name.clone())
        .collect();
    Ok(ResolvedBuildVars { channel, vars, volatile_vars })
}

/// The platform names the runtime distinguishes; `--target-os` accepts
/// exactly these.
const TARGET_PLATFORMS: &[&str] = &["linux", "osx", "windows", "ios", "web"];

fn validate_target_os(target: &str) -> Result<&str> {
    if TARGET_PLATFORMS.contains(&target) {
        Ok(target)
    } else {
        anyhow::bail!(
            "--target-os {target:?} is not a known platform (expected one of {})",
            TARGET_PLATFORMS.join(", ")
        )
    }
}

fn resolve_builtin(name: &str, channel: &str, target_os: Option<&str>) -> Result<String> {
    match name {
        "channel" => Ok(channel.to_string()),
        // An explicit target is the artifact truth (a Linux host packaging a
        // Windows release must bake "windows"); channel inference and the
        // packaging host are proxies used only when no target was declared.
        "platform" => Ok(if let Some(target) = target_os {
            target.to_string()
        } else if channel == "web" {
            "web".to_string()
        } else if matches!(channel, "ios" | "ios-dev") {
            "ios".to_string()
        } else {
            match std::env::consts::OS {
                "macos" => "osx".to_string(),
                other => other.to_string(),
            }
        }),
        other => anyhow::bail!("unknown [build.vars] builtin {other:?}"),
    }
}

/// A `secret = true` var's diagnostics are redacted on the failure path: the
/// command text can carry the credential (`load_key.sh --token abc`) and a
/// failing fetcher routinely echoes it. The variable NAME and the exit status
/// always survive -- they say which declaration to look at and how it died,
/// and neither can carry the value. A non-secret var reports everything.
fn run_var_cmd(
    name: &str,
    cmd: &str,
    secret: bool,
    project_dir: &Path,
    channel: &str,
) -> Result<String> {
    let shown = if secret { "<redacted: secret = true>".to_string() } else { format!("{cmd:?}") };
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(project_dir)
        .env("ROCK_CHANNEL", channel)
        .output()
        .map_err(|e| anyhow::anyhow!("[build.vars] {name}: failed to spawn {shown}: {e}"))?;
    if !output.status.success() {
        let detail = if secret {
            "stderr withheld (secret = true)".to_string()
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        };
        anyhow::bail!("[build.vars] {name}: {shown} failed ({}): {detail}", output.status);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let warning = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !warning.is_empty() {
        if secret {
            if value.is_empty() {
                log::warn!(
                    "[build.vars] {name}: source unavailable for channel {channel:?}; \
                     continuing without a value"
                );
            } else {
                log::warn!(
                    "[build.vars] {name}: source reported a warning for channel {channel:?}; \
                     details withheld (secret = true)"
                );
            }
        } else {
            log::warn!("[build.vars] {name}: {warning}");
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(toml: &str) -> StickConfig {
        let cfg = StickConfig::new_from_str(toml).expect("fixture parses");
        cfg.validate_build().expect("fixture validates");
        cfg
    }

    fn no_overrides() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn value_and_builtin_sources_resolve() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"steam\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nENDPOINT = \"https://example.test\"\nCHANNEL = { builtin = \"channel\" }\nCLOCK = { value = \"now\", volatile = true }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_build_vars(&cfg, dir.path(), Some("steam"), None, &no_overrides()).unwrap();
        assert_eq!(resolved.channel, "steam");
        assert_eq!(resolved.vars["ENDPOINT"].value, "https://example.test");
        assert_eq!(resolved.vars["CHANNEL"].value, "steam");
        assert!(resolved.volatile_vars.contains("CLOCK"));
    }

    #[test]
    fn platform_builtin_tracks_web_and_ios_channels() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"web\", \"ios-dev\"]\n",
            "[build.vars]\nPLATFORM = { builtin = \"platform\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let web = resolve_build_vars(&cfg, dir.path(), Some("web"), None, &no_overrides()).unwrap();
        assert_eq!(web.vars["PLATFORM"].value, "web");
        let ios = resolve_build_vars(&cfg, dir.path(), Some("ios-dev"), None, &no_overrides()).unwrap();
        assert_eq!(ios.vars["PLATFORM"].value, "ios");
        let host = resolve_build_vars(&cfg, dir.path(), Some("dev"), None, &no_overrides()).unwrap();
        let expected = match std::env::consts::OS {
            "macos" => "osx",
            other => other,
        };
        assert_eq!(host.vars["PLATFORM"].value, expected);
    }

    #[test]
    fn target_os_overrides_the_platform_builtin() {
        // Cross-builds package on one OS for another; the artifact target,
        // not the packaging host, is what the builtin must report.
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"steam\", \"web\"]\n",
            "[build.vars]\nPLATFORM = { builtin = \"platform\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let win =
            resolve_build_vars(&cfg, dir.path(), Some("steam"), Some("windows"), &no_overrides())
                .unwrap();
        assert_eq!(win.vars["PLATFORM"].value, "windows");
        // An explicit target also wins over channel inference: the target is
        // the artifact truth, the channel only a proxy for it.
        let web =
            resolve_build_vars(&cfg, dir.path(), Some("web"), Some("windows"), &no_overrides())
                .unwrap();
        assert_eq!(web.vars["PLATFORM"].value, "windows");
    }

    #[test]
    fn an_unknown_target_os_rejects() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\n",
            "[build.vars]\nPLATFORM = { builtin = \"platform\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), None, Some("win64"), &no_overrides())
            .expect_err("unknown target os must reject");
        assert!(format!("{err}").contains("win64"), "error names the bad target: {err}");
    }

    #[test]
    fn explicit_channel_with_no_declared_channels_rejects() {
        // A [build] section without a channels list cannot validate an
        // explicit --channel, so accepting one silently stamps a typo into
        // release identity. Fail closed instead.
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\n",
            "[build.vars]\nCHANNEL = { builtin = \"channel\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), Some("stema"), None, &no_overrides())
            .expect_err("explicit channel with no declared channels must reject");
        let msg = format!("{err}");
        assert!(msg.contains("stema"), "error names the channel: {msg}");

        // The implicit default stays usable: no --channel resolves as before.
        let resolved = resolve_build_vars(&cfg, dir.path(), None, None, &no_overrides()).unwrap();
        assert_eq!(resolved.vars["CHANNEL"].value, "dev");
    }

    #[test]
    fn cmd_runs_in_project_dir_with_rock_channel_and_trims_newline() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nMARKER = { cmd = \"cat marker.txt\" }\nCHAN = { cmd = \"printenv ROCK_CHANNEL\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "from-project-dir\n").unwrap();
        let resolved = resolve_build_vars(&cfg, dir.path(), None, None, &no_overrides()).unwrap();
        assert_eq!(resolved.vars["MARKER"].value, "from-project-dir");
        assert_eq!(resolved.vars["CHAN"].value, "dev");
    }

    #[test]
    fn override_wins_and_suppresses_the_source() {
        // cmd = "false" would fail resolution; the override must win without
        // running it, or precedence is a lie.
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nTOKEN = { cmd = \"false\", secret = true }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let overrides = HashMap::from([("TOKEN".to_string(), "override-value".to_string())]);
        let resolved = resolve_build_vars(&cfg, dir.path(), None, None, &overrides).unwrap();
        assert_eq!(resolved.vars["TOKEN"].value, "override-value");
        assert!(resolved.vars["TOKEN"].secret, "override keeps the declared secrecy");
    }

    // A secret var's FAILURE path is the leak: the command text can carry the
    // credential (`load_key.sh --token abc`) and a failing fetcher routinely
    // echoes it. Both sides are asserted -- the redaction, and that a
    // non-secret var still reports everything -- because a redaction that
    // fired on every var would satisfy the first arm alone.
    #[test]
    fn a_failing_secret_cmd_redacts_its_command_and_stderr() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nTOKEN = { cmd = \"echo s3cr3t-in-stderr >&2; echo s3cr3t-in-cmd >/dev/null; exit 7\", secret = true }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), None, None, &no_overrides())
            .expect_err("a failing cmd must fail the invocation");
        let msg = format!("{err}");
        assert!(msg.contains("TOKEN"), "the variable NAME survives redaction: {msg}");
        assert!(msg.contains('7'), "the exit status survives redaction: {msg}");
        assert!(msg.contains("secret"), "the message says why it is redacted: {msg}");
        assert!(
            !msg.contains("s3cr3t-in-stderr"),
            "stderr of a secret var's cmd leaked: {msg}"
        );
        assert!(
            !msg.contains("s3cr3t-in-cmd"),
            "the command text of a secret var leaked: {msg}"
        );
    }

    #[test]
    fn a_failing_non_secret_cmd_still_reports_command_and_stderr() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nSHA = { cmd = \"echo diagnostic-detail >&2; exit 7\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), None, None, &no_overrides())
            .expect_err("a failing cmd must fail the invocation");
        let msg = format!("{err}");
        assert!(msg.contains("SHA"), "error names the var: {msg}");
        assert!(msg.contains("diagnostic-detail"), "non-secret stderr is retained: {msg}");
        assert!(msg.contains("echo diagnostic-detail"), "non-secret cmd is retained: {msg}");
    }

    #[test]
    fn override_of_undeclared_var_rejects() {
        let cfg = fixture("[package]\nname = \"x\"\n[build]\nchannels = [\"dev\"]\n");
        let dir = tempfile::tempdir().unwrap();
        let overrides = HashMap::from([("NOPE".to_string(), "v".to_string())]);
        let err = resolve_build_vars(&cfg, dir.path(), None, None, &overrides)
            .expect_err("overriding an undeclared var must reject");
        assert!(format!("{err}").contains("NOPE"), "error names the var: {err}");
    }

    #[test]
    fn out_of_channel_var_takes_default_and_missing_default_rejects() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"steam\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nTOKEN = { cmd = \"false\", channels = [\"steam\"], default = \"\" }\n",
            "NEEDED = { cmd = \"false\", channels = [\"steam\"] }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), Some("dev"), None, &no_overrides())
            .expect_err("restricted var with no default must reject off-channel");
        assert!(format!("{err}").contains("NEEDED"), "error names the var: {err}");

        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"steam\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nTOKEN = { cmd = \"false\", channels = [\"steam\"], default = \"none\" }\n",
        ));
        let resolved = resolve_build_vars(&cfg, dir.path(), Some("dev"), None, &no_overrides()).unwrap();
        assert_eq!(resolved.vars["TOKEN"].value, "none", "off-channel uses default, cmd never runs");
    }

    #[test]
    fn optional_command_failure_uses_default_only_on_the_declared_channel() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"steam\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nTOKEN = { cmd = \"false\", optional_channels = [\"dev\"], default = \"\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let dev = resolve_build_vars(&cfg, dir.path(), Some("dev"), None, &no_overrides())
            .expect("optional dev command failure uses its default");
        assert_eq!(dev.vars["TOKEN"].value, "");

        let err = resolve_build_vars(&cfg, dir.path(), Some("steam"), None, &no_overrides())
            .expect_err("the same command must remain required on steam");
        assert!(format!("{err}").contains("TOKEN"));
    }

    #[test]
    fn failing_cmd_rejects_with_stderr() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nX = { cmd = \"echo boom >&2; exit 3\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), None, None, &no_overrides())
            .expect_err("failing cmd must fail resolution, never default silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("boom") && msg.contains("X"), "error carries var and stderr: {msg}");
    }

    #[test]
    fn undeclared_channel_rejects() {
        let cfg = fixture("[package]\nname = \"x\"\n[build]\nchannels = [\"dev\"]\n");
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), Some("stema"), None, &no_overrides())
            .expect_err("undeclared channel must reject");
        assert!(format!("{err}").contains("stema"), "error names the channel: {err}");
    }

    #[test]
    fn channel_arg_without_build_section_rejects() {
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), Some("steam"), None, &no_overrides())
            .expect_err("--channel with no [build] section must reject");
        assert!(format!("{err}").contains("[build]"), "error points at the missing section: {err}");
    }

    #[test]
    fn no_build_section_resolves_empty() {
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_build_vars(&cfg, dir.path(), None, None, &no_overrides()).unwrap();
        assert!(resolved.vars.is_empty());
        assert_eq!(resolved.channel, "dev");
    }
}
