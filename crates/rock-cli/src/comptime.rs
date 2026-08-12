//! Host-side resolution of comptime inputs (docs/comptime-plan.md tier 1):
//! turn a project's `[build.vars]` plus the invocation's `--channel`/`--var`
//! selection into the `ComptimeInputs` skull's `build_var`/`embed_string`
//! expansion reads off the Workspace. Commands run here, once per session build,
//! at the compile chokepoints -- never from the LSP, which performs no
//! emission and so never reaches this module.

use anyhow::Result;
use bone_semantic::inference::{ComptimeInputs, ComptimeVarValue};
use std::path::Path;
use std::sync::OnceLock;

/// The invocation's `--channel`/`--var` selection. Set once by `run_cli`
/// before dispatch; read-only afterwards. Per-invocation CLI state, not a
/// mutable mechanism -- resolution itself stays per-project at the
/// chokepoints.
#[derive(Debug, Default)]
struct CliSelection {
    channel: Option<String>,
    /// The command's soft channel (`rock web` -> "web"): used only when the
    /// project DECLARES it in `[build] channels`, ignored otherwise -- an
    /// explicit `--channel` must reject when undeclared, but a command
    /// implication must not break projects that never opted into channels.
    implied_channel: Option<String>,
    overrides: std::collections::HashMap<String, String>,
}

static SELECTION: OnceLock<CliSelection> = OnceLock::new();

/// Record the invocation's selection. `overrides` are raw `NAME=VALUE`
/// strings; a malformed one rejects here so the failure names the flag rather
/// than surfacing as an undeclared-var error later.
pub fn set_invocation_selection(
    channel: Option<String>,
    implied_channel: Option<String>,
    overrides: &[String],
) -> Result<()> {
    let mut parsed = std::collections::HashMap::new();
    for raw in overrides {
        let Some((name, value)) = raw.split_once('=') else {
            anyhow::bail!("--var expects NAME=VALUE, got {raw:?}");
        };
        parsed.insert(name.to_string(), value.to_string());
    }
    let _ = SELECTION.set(CliSelection { channel, implied_channel, overrides: parsed });
    Ok(())
}

/// Resolve the comptime inputs for the project at/above `dir`: find its
/// stick.toml, validate `[build]`, run each var's source under the selected
/// channel, and hand back the map plus the embed root (the stick.toml dir).
/// No stick.toml means empty inputs -- unless the invocation selected a
/// channel, which nothing declares, and that rejects rather than silently
/// resolving nothing.
pub fn inputs_for_project_dir(dir: &Path) -> Result<ComptimeInputs> {
    let selection = SELECTION.get();
    let mut channel = selection.and_then(|s| s.channel.as_deref());
    let no_overrides = std::collections::HashMap::new();
    let overrides = selection.map(|s| &s.overrides).unwrap_or(&no_overrides);

    let stick = rock_host::stick::StickConfig::find_from_dir(dir)?;
    if channel.is_none() {
        if let (Some(implied), Some((_, cfg))) =
            (selection.and_then(|s| s.implied_channel.as_deref()), stick.as_ref())
        {
            let declared = cfg
                .build
                .as_ref()
                .is_some_and(|b| b.channels.iter().any(|c| c == implied));
            if declared {
                channel = Some(implied);
            }
        }
    }
    let Some((stick_path, cfg)) = stick else {
        if let Some(chan) = channel {
            anyhow::bail!(
                "--channel {chan:?} given but no stick.toml found at or above {}",
                dir.display()
            );
        }
        if let Some(name) = overrides.keys().next() {
            anyhow::bail!("--var {name} given but no stick.toml found at or above {}", dir.display());
        }
        return Ok(ComptimeInputs::default());
    };
    let project_dir = stick_path.parent().expect("stick.toml has a parent dir").to_path_buf();

    let resolved = rock_host::stick::build_vars::resolve_build_vars(
        &cfg,
        &project_dir,
        channel,
        overrides,
    )?;

    let mut inputs = ComptimeInputs::default();
    inputs.channel = resolved.channel;
    // Fallback only: an embed normally resolves against the calling file's own
    // package root, and reaches this when the module has no file on disk.
    inputs.embed_root = Some(project_dir);
    for (name, var) in resolved.vars {
        inputs.vars.insert(
            ustr::Ustr::from(&name),
            ComptimeVarValue { value: var.value, secret: var.secret },
        );
    }
    Ok(inputs)
}
