//! Resolution of `[build.vars]` into concrete values, once per CLI
//! invocation, before compilation. Precedence: declaration `default` (for a
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
    /// Hash into cache keys; never write the value into on-disk artifacts.
    pub secret: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedBuildVars {
    pub channel: String,
    pub vars: HashMap<String, ResolvedVar>,
}

pub fn resolve_build_vars(
    cfg: &StickConfig,
    project_dir: &Path,
    channel_arg: Option<&str>,
    overrides: &HashMap<String, String>,
) -> Result<ResolvedBuildVars> {
    let Some(build) = cfg.build.as_ref() else {
        if let Some(chan) = channel_arg {
            anyhow::bail!(
                "--channel {chan:?} given but stick.toml has no [build] section declaring channels"
            );
        }
        if let Some(name) = overrides.keys().next() {
            anyhow::bail!("--var {name} is not declared in [build.vars]");
        }
        return Ok(ResolvedBuildVars { channel: "dev".to_string(), vars: HashMap::new() });
    };
    cfg.validate_build()?;

    let channel = channel_arg
        .or(build.default_channel.as_deref())
        .unwrap_or("dev")
        .to_string();
    if !build.channels.is_empty() && !build.channels.iter().any(|c| c == &channel) {
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
        } else if var.builtin.as_deref() == Some("channel") {
            channel.clone()
        } else if let Some(cmd) = var.cmd.as_ref() {
            run_var_cmd(name, cmd, project_dir, &channel)?
        } else {
            // validate_build above rejected sourceless vars.
            unreachable!("[build.vars] {name} has no source past validate_build")
        };
        vars.insert(name.clone(), ResolvedVar { value, secret: var.secret });
    }
    Ok(ResolvedBuildVars { channel, vars })
}

fn run_var_cmd(name: &str, cmd: &str, project_dir: &Path, channel: &str) -> Result<String> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(project_dir)
        .env("ROCK_CHANNEL", channel)
        .output()
        .map_err(|e| anyhow::anyhow!("[build.vars] {name}: failed to spawn {cmd:?}: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "[build.vars] {name}: {cmd:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
            "[build.vars]\nENDPOINT = \"https://example.test\"\nCHANNEL = { builtin = \"channel\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_build_vars(&cfg, dir.path(), Some("steam"), &no_overrides()).unwrap();
        assert_eq!(resolved.channel, "steam");
        assert_eq!(resolved.vars["ENDPOINT"].value, "https://example.test");
        assert_eq!(resolved.vars["CHANNEL"].value, "steam");
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
        let resolved = resolve_build_vars(&cfg, dir.path(), None, &no_overrides()).unwrap();
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
        let resolved = resolve_build_vars(&cfg, dir.path(), None, &overrides).unwrap();
        assert_eq!(resolved.vars["TOKEN"].value, "override-value");
        assert!(resolved.vars["TOKEN"].secret, "override keeps the declared secrecy");
    }

    #[test]
    fn override_of_undeclared_var_rejects() {
        let cfg = fixture("[package]\nname = \"x\"\n[build]\nchannels = [\"dev\"]\n");
        let dir = tempfile::tempdir().unwrap();
        let overrides = HashMap::from([("NOPE".to_string(), "v".to_string())]);
        let err = resolve_build_vars(&cfg, dir.path(), None, &overrides)
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
        let err = resolve_build_vars(&cfg, dir.path(), Some("dev"), &no_overrides())
            .expect_err("restricted var with no default must reject off-channel");
        assert!(format!("{err}").contains("NEEDED"), "error names the var: {err}");

        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\", \"steam\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nTOKEN = { cmd = \"false\", channels = [\"steam\"], default = \"none\" }\n",
        ));
        let resolved = resolve_build_vars(&cfg, dir.path(), Some("dev"), &no_overrides()).unwrap();
        assert_eq!(resolved.vars["TOKEN"].value, "none", "off-channel uses default, cmd never runs");
    }

    #[test]
    fn failing_cmd_rejects_with_stderr() {
        let cfg = fixture(concat!(
            "[package]\nname = \"x\"\n",
            "[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n",
            "[build.vars]\nX = { cmd = \"echo boom >&2; exit 3\" }\n",
        ));
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), None, &no_overrides())
            .expect_err("failing cmd must fail resolution, never default silently");
        let msg = format!("{err:#}");
        assert!(msg.contains("boom") && msg.contains("X"), "error carries var and stderr: {msg}");
    }

    #[test]
    fn undeclared_channel_rejects() {
        let cfg = fixture("[package]\nname = \"x\"\n[build]\nchannels = [\"dev\"]\n");
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), Some("stema"), &no_overrides())
            .expect_err("undeclared channel must reject");
        assert!(format!("{err}").contains("stema"), "error names the channel: {err}");
    }

    #[test]
    fn channel_arg_without_build_section_rejects() {
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_build_vars(&cfg, dir.path(), Some("steam"), &no_overrides())
            .expect_err("--channel with no [build] section must reject");
        assert!(format!("{err}").contains("[build]"), "error points at the missing section: {err}");
    }

    #[test]
    fn no_build_section_resolves_empty() {
        let cfg = StickConfig::new_from_str("[package]\nname = \"x\"\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_build_vars(&cfg, dir.path(), None, &no_overrides()).unwrap();
        assert!(resolved.vars.is_empty());
        assert_eq!(resolved.channel, "dev");
    }
}
