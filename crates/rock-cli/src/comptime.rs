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
    /// `--target-os`: the artifact target for the `platform` builtin. A
    /// cross-build packages on one OS for another, so the packaging host is
    /// only a proxy used when this is absent.
    target_os: Option<String>,
    overrides: std::collections::HashMap<String, String>,
}

static SELECTION: OnceLock<CliSelection> = OnceLock::new();

/// Record the invocation's selection. `overrides` are raw `NAME=VALUE`
/// strings; a malformed one rejects here so the failure names the flag rather
/// than surfacing as an undeclared-var error later.
pub fn set_invocation_selection(
    channel: Option<String>,
    implied_channel: Option<String>,
    target_os: Option<String>,
    overrides: &[String],
) -> Result<()> {
    let mut parsed = std::collections::HashMap::new();
    for raw in overrides {
        let Some((name, value)) = raw.split_once('=') else {
            anyhow::bail!("--var expects NAME=VALUE, got {raw:?}");
        };
        parsed.insert(name.to_string(), value.to_string());
    }
    SELECTION
        .set(CliSelection { channel, implied_channel, target_os, overrides: parsed })
        .map_err(|_| anyhow::anyhow!("CLI invocation selection was already initialized"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::set_invocation_selection;

    #[test]
    fn repeated_invocation_selection_is_rejected() {
        assert!(set_invocation_selection(None, None, None, &[]).is_ok());
        let err = set_invocation_selection(None, None, None, &[]).expect_err("second set must fail");
        assert!(err.to_string().contains("already initialized"));
    }
}

/// Stamp `ws` with the inputs resolved from `anchor`'s enclosing project.
/// This is the ONE resolution seam every EMITTING entry point routes through
/// -- per-site `ws.comptime_inputs = ...` assignments are how `rock test`,
/// `rock watch` and the profiling tools shipped broken (each new entry point
/// silently forgot the obligation). Type-check-only paths never need values
/// and the LSP never runs commands, so neither reaches this.
pub fn stamp_workspace(
    ws: &mut bone_semantic::inference::Workspace,
    anchor: &Path,
) -> Result<()> {
    ws.comptime_inputs = inputs_for_entry_path(anchor)?;
    Ok(())
}

/// [`inputs_for_project_dir`] keyed by the invocation's entry path: a file
/// anchors at its parent (so a single file inside a `[build.vars]` project
/// behaves the same as the project build, on both backends), a directory at
/// itself. A path that does not exist on disk (synthetic in-memory module)
/// has no enclosing project and resolves empty.
pub fn inputs_for_entry_path(anchor: &Path) -> Result<ComptimeInputs> {
    let dir = if anchor.is_dir() {
        anchor
    } else if anchor.is_file() {
        match anchor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    } else {
        return Ok(ComptimeInputs::default());
    };
    inputs_for_project_dir(dir)
}

/// Resolve the comptime inputs for the project at/above `dir`: find its
/// stick.toml, validate `[build]`, run each var's source under the selected
/// channel, and hand back the map plus the embed root (the stick.toml dir).
/// No stick.toml means empty inputs -- unless the invocation selected a
/// channel, which nothing declares, and that rejects rather than silently
/// resolving nothing.
pub fn inputs_for_project_dir(dir: &Path) -> Result<ComptimeInputs> {
    let selection = SELECTION.get();
    let stick = rock_host::stick::StickConfig::find_from_dir(dir)?;
    let Some((stick_path, cfg)) = stick else {
        let channel = selection.and_then(|s| s.channel.as_deref());
        if let Some(chan) = channel {
            anyhow::bail!(
                "--channel {chan:?} given but no stick.toml found at or above {}",
                dir.display()
            );
        }
        if let Some(target) = selection.and_then(|s| s.target_os.as_deref()) {
            anyhow::bail!(
                "--target-os {target:?} given but no stick.toml found at or above {}",
                dir.display()
            );
        }
        let no_overrides = std::collections::HashMap::new();
        let overrides = selection.map(|s| &s.overrides).unwrap_or(&no_overrides);
        if let Some(name) = overrides.keys().next() {
            anyhow::bail!("--var {name} given but no stick.toml found at or above {}", dir.display());
        }
        return Ok(ComptimeInputs::default());
    };
    let project_dir = stick_path.parent().expect("stick.toml has a parent dir").to_path_buf();
    inputs_for_project_with_config(&project_dir, &cfg)
}

/// Resolve inputs from a config the caller has already parsed. Release
/// preparation uses this path so `[release]` selection and `[build.vars]`
/// resolution share one `StickConfig` parse.
pub fn inputs_for_project_with_config(
    project_dir: &Path,
    cfg: &rock_host::stick::StickConfig,
) -> Result<ComptimeInputs> {
    let selection = SELECTION.get();
    let mut channel = selection.and_then(|s| s.channel.as_deref());
    if channel.is_none() {
        if let Some(implied) = selection.and_then(|s| s.implied_channel.as_deref()) {
            let declared = cfg
                .build
                .as_ref()
                .is_some_and(|b| b.channels.iter().any(|c| c == implied));
            if declared {
                channel = Some(implied);
            }
        }
    }
    let no_overrides = std::collections::HashMap::new();
    let overrides = selection.map(|s| &s.overrides).unwrap_or(&no_overrides);

    let resolved = rock_host::stick::build_vars::resolve_build_vars(
        cfg,
        project_dir,
        channel,
        selection.and_then(|s| s.target_os.as_deref()),
        overrides,
    )?;

    let mut inputs = ComptimeInputs::default();
    inputs.channel = resolved.channel;
    inputs.volatile_vars = resolved
        .volatile_vars
        .into_iter()
        .map(|name| ustr::Ustr::from(&name))
        .collect();
    // Fallback only: an embed normally resolves against the calling file's own
    // package root, and reaches this when the module has no file on disk.
    inputs.embed_root = Some(project_dir.to_path_buf());
    for (name, var) in resolved.vars {
        inputs.vars.insert(
            ustr::Ustr::from(&name),
            ComptimeVarValue { value: var.value, secret: var.secret },
        );
    }
    Ok(inputs)
}
