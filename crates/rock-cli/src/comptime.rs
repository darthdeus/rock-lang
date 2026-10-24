//! Host-side resolution of comptime inputs (the compiler contract tier 1):
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
    // rock-host consumers of the channel (dependency resolution, source
    // loading) cannot read SELECTION; mirror the same values into rock-host.
    rock_host::stick::set_invocation_channel(channel.clone(), implied_channel.clone());
    SELECTION
        .set(CliSelection { channel, implied_channel, target_os, overrides: parsed })
        .map_err(|_| anyhow::anyhow!("CLI invocation selection was already initialized"))?;
    Ok(())
}

/// The invocation's explicit selection as `rock` arguments, for a child
/// `rock` that compiles on this invocation's behalf. The implied channel is
/// not forwarded: the child's own command implies its own.
pub fn invocation_selection_args() -> Vec<String> {
    let Some(selection) = SELECTION.get() else {
        return Vec::new();
    };
    let mut args = Vec::new();
    if let Some(channel) = &selection.channel {
        args.extend(["--channel".to_string(), channel.clone()]);
    }
    if let Some(target_os) = &selection.target_os {
        args.extend(["--target-os".to_string(), target_os.clone()]);
    }
    let mut overrides = selection.overrides.iter().collect::<Vec<_>>();
    overrides.sort();
    for (name, value) in overrides {
        args.extend(["--var".to_string(), format!("{name}={value}")]);
    }
    args
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
    ws.comptime_inputs = resolve_checked_inputs(ws, inputs_for_entry_path(anchor)?, |_| None)?;
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
    inputs_for_project_dir_as(dir, None)
}

/// [`inputs_for_project_dir`] as resolved by a `build_command` invocation
/// (`ROCK_BUILD_COMMAND`), whatever command this process is.
pub fn inputs_for_project_dir_as(dir: &Path, build_command: Option<&str>) -> Result<ComptimeInputs> {
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
    inputs_for_project_with_config(&project_dir, &cfg, build_command)
}

/// Resolve inputs from a config the caller has already parsed. Release
/// preparation uses this path so `[release]` selection and `[build.vars]`
/// resolution share one `StickConfig` parse.
pub fn inputs_for_project_with_config(
    project_dir: &Path,
    cfg: &rock_host::stick::StickConfig,
    build_command: Option<&str>,
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
        build_command,
    )?;

    let mut inputs = ComptimeInputs::default();
    for (name, declaration) in resolved.source_fingerprints {
        inputs.source_fingerprints.insert(ustr::Ustr::from(&name), bone_core::workspace::SourceFingerprint {
            secret: cfg.build.as_ref().and_then(|build| build.vars.get(&name)).is_some_and(|var| var.secret),
            roots: declaration.roots.iter().map(|name| ustr::Ustr::from(name)).collect(),
            files: declaration.files,
            vars: declaration.vars.iter().map(|name| ustr::Ustr::from(name)).collect(),
        });
    }
    inputs.channel = resolved.channel;
    inputs.modules_mode = match cfg.dialect() {
        rock_core::source_code::Dialect::V1 => "v1".to_string(),
        rock_core::source_code::Dialect::Legacy => "legacy".to_string(),
    };
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

fn source_file_identity(file: &str, root: Option<&Path>) -> String {
    let path = Path::new(file);
    let Some(root) = root.filter(|_| path.is_absolute()) else { return file.to_string() };
    let root = root.canonicalize().unwrap_or_else(|_| {
        if root.is_absolute() { root.to_path_buf() } else {
            std::env::current_dir().unwrap_or_default().join(root)
        }
    });
    let source = path.components().collect::<Vec<_>>();
    let anchor = root.components().collect::<Vec<_>>();
    let shared = source.iter().zip(&anchor).take_while(|(a, b)| a == b).count();
    let mut parts = vec!["..".to_string(); anchor.len() - shared];
    parts.extend(source[shared..].iter().map(|part| part.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

fn resolve_source_fingerprints(
    ws: &bone_semantic::inference::Workspace,
    inputs: &mut ComptimeInputs,
    source_text: impl Fn(&str) -> Option<String>,
    compiler_identity: &str,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    fn field(hash: &mut Sha256, value: &str) {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    let mut resolved = Vec::new();
    for (name, declaration) in &inputs.source_fingerprints {
        let mut hash = Sha256::new();
        field(&mut hash, "rock-source-fingerprint-v1");
        field(&mut hash, compiler_identity);
        let mut roots = declaration.roots.clone();
        roots.sort();
        roots.dedup();
        if roots.is_empty() || inputs.volatile_vars.contains(name) {
            anyhow::bail!("source fingerprint {name} requires roots and stable metadata");
        }
        hash.update((roots.len() as u64).to_le_bytes());
        for root in &roots {
            field(&mut hash, root.as_str());
        }
        let owners = bone_core::type_sources::declaration_sources(ws, &roots)
            .map_err(anyhow::Error::msg)?;
        hash.update((owners.len() as u64).to_le_bytes());
        for owner in owners {
            // Canonical declaration identity is stable across worktree roots.
            // Hash all defining bytes, including defaults and helper bodies.
            field(&mut hash, owner.name.as_str());
            field(&mut hash, &source_file_identity(owner.file.as_str(), inputs.embed_root.as_deref()));
            let text = source_text(owner.file.as_str())
                .ok_or_else(|| anyhow::anyhow!("source fingerprint {name}: missing checked source {}", owner.file))?;
            field(&mut hash, &text);
        }
        let mut files = declaration.files.clone();
        files.sort();
        files.dedup();
        hash.update((files.len() as u64).to_le_bytes());
        for file in files {
            field(&mut hash, &file);
            let text = source_text(&file)
                .ok_or_else(|| anyhow::anyhow!("source fingerprint {name}: missing support source {file}"))?;
            field(&mut hash, &text);
        }
        let mut vars = declaration.vars.clone();
        vars.sort();
        vars.dedup();
        hash.update((vars.len() as u64).to_le_bytes());
        let mut secret = declaration.secret;
        for dependency in vars {
            let value = inputs.vars.get(&dependency)
                .ok_or_else(|| anyhow::anyhow!("source fingerprint {name}: missing input {dependency}"))?;
            if inputs.source_fingerprints.contains_key(&dependency) || inputs.volatile_vars.contains(&dependency) {
                anyhow::bail!("source fingerprint {name}: dependency {dependency} must be a stable resolved input");
            }
            secret |= value.secret;
            field(&mut hash, dependency.as_str());
            field(&mut hash, &value.value);
        }
        resolved.push((*name, ComptimeVarValue { value: format!("source-v1:{:x}", hash.finalize()), secret }));
    }
    for (name, value) in resolved {
        inputs.vars.insert(name, value);
    }
    Ok(())
}

/// Resolve declared fingerprints using the checked program's source snapshot.
/// The reader must return exactly the bytes used to check each file; support
/// files fall back to the project's filesystem. Stdlib always uses the same
/// compiler-selected snapshot as parsing, including embedded installations.
pub(crate) fn resolve_checked_inputs(
    ws: &bone_semantic::inference::Workspace,
    mut inputs: ComptimeInputs,
    source_text: impl Fn(&str) -> Option<String>,
) -> Result<ComptimeInputs> {
    if inputs.source_fingerprints.is_empty() {
        return Ok(inputs);
    }
    let started = std::time::Instant::now();
    let root = inputs.embed_root.clone();
    resolve_source_fingerprints(ws, &mut inputs, |file| {
        if let Some(name) = file.strip_prefix("<stdlib>/").or_else(|| file.strip_prefix("stdlib/")) {
            return rock_stdlib::stdlib_files().iter().find(|f| f.name == name).map(|f| f.contents.to_string());
        }
        source_text(file).or_else(|| {
            let path = Path::new(file);
            let path = if path.is_absolute() { path.to_path_buf() } else { root.as_ref()?.join(path) };
            let path = path.canonicalize().ok()?;
            source_text(path.to_str()?).or_else(|| std::fs::read_to_string(path).ok())
        })
    }, &compiler_identity()?)?;
    crate::compile_measurements::timings("comptime", &[("source-fingerprint", started.elapsed())]);
    Ok(inputs)
}

/// Content identity of the running compiler image, shared by source inputs
/// and hosts' pre-compile cache keys. Never substitutes a checkout revision.
pub fn compiler_identity() -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    static IDENTITY: OnceLock<Result<String, String>> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let image = rock_host::process_management::current_executable_image().map_err(|e| e.to_string())?;
        let mut file = std::fs::File::open(image).map_err(|e| e.to_string())?;
        let before = file.metadata().map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        if before.modified().map_err(|e| e.to_string())? > compiler_process_started()? {
            return Err("compiler image changed after process start".into());
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 { break; }
            hash.update(&buffer[..count]);
        }
        let after = file.metadata().map_err(|e| e.to_string())?;
        if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            return Err("compiler image changed while hashing".into());
        }
        Ok(format!("{:x}", hash.finalize()))
    }).clone().map_err(anyhow::Error::msg)
}

#[cfg(target_os = "macos")]
fn compiler_process_started() -> std::result::Result<std::time::SystemTime, String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: info is a writable proc_bsdinfo of exactly size bytes.
    let written = unsafe {
        libc::proc_pidinfo(std::process::id() as libc::c_int, libc::PROC_PIDTBSDINFO,
            0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
    };
    if written != size { return Err(std::io::Error::last_os_error().to_string()); }
    Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(info.pbi_start_tvsec)
        + std::time::Duration::from_micros(info.pbi_start_tvusec))
}

#[cfg(test)]
mod source_fingerprint_tests {
    use super::*;
    use bone_core::workspace::SourceFingerprint;
    use bone_semantic::inference::Workspace;
    use ustr::Ustr;

    fn fixture() -> (Workspace, ComptimeInputs) {
        let mut modules = [
            ("root.rock", "struct Root { child: Child }") ,
            ("child.rock", "value Child { count: int = 4 }"),
            ("other.rock", "struct Other { count: int }"),
        ].iter().map(|(file, text)| skull_parser::parse_module(text, Ustr::from(file)).unwrap()).collect::<Vec<_>>();
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap();
        let mut inputs = ComptimeInputs::default();
        inputs.source_fingerprints.insert(Ustr::from("REV"), SourceFingerprint {
            secret: false,
            roots: vec![Ustr::from("Root")],
            files: vec!["decoder.rock".into()],
            vars: vec![Ustr::from("ABI")],
        });
        inputs.vars.insert(Ustr::from("ABI"), ComptimeVarValue { value: "runtime-1".into(), secret: false });
        (ws, inputs)
    }

    fn revision(ws: &Workspace, inputs: &ComptimeInputs, changed: &str, replacement: &str, compiler: &str) -> String {
        let mut inputs = inputs.clone();
        resolve_source_fingerprints(ws, &mut inputs, |file| Some(if file == changed { replacement } else {
            match file {
                "root.rock" => "struct Root { child: Child }",
                "child.rock" => "value Child { count: int = 4 }",
                "decoder.rock" => "fn decode() { 1 }",
                "other.rock" => "struct Other { count: int }",
                _ => return None,
            }
        }.to_string()), compiler).unwrap();
        inputs.vars[&Ustr::from("REV")].value.clone()
    }

    #[test]
    fn source_fingerprint_tracks_whole_owners_support_runtime_and_compiler() {
        let (ws, inputs) = fixture();
        let baseline = revision(&ws, &inputs, "", "", "compiler-1");
        for (file, text) in [
            ("child.rock", "value Child { count: int = 5 }"),
            ("root.rock", "struct Root { child: Child }\nfn helper() { 2 }"),
            ("decoder.rock", "fn decode() { 2 }"),
        ] {
            assert_ne!(baseline, revision(&ws, &inputs, file, text, "compiler-1"), "{file}");
        }
        assert_eq!(baseline, revision(&ws, &inputs, "other.rock", "unrelated body", "compiler-1"));
        assert_ne!(baseline, revision(&ws, &inputs, "", "", "compiler-2"));
        let mut inputs = inputs;
        inputs.vars.get_mut(&Ustr::from("ABI")).unwrap().value = "runtime-2".into();
        assert_ne!(baseline, revision(&ws, &inputs, "", "", "compiler-1"));
    }

    #[test]
    fn checked_source_fingerprint_is_recorded_by_project_compile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stick.toml"),
            "[package]\nname = \"recorded\"\n[build.vars]\nREV = { source_fingerprint = { roots = [\"Root\"] } }\n").unwrap();
        std::fs::write(dir.path().join("main.rock"),
            "value Root { count: int = 4 }\nfn main() { print(build_var(\"REV\")) }").unwrap();
        let inputs = inputs_for_project_dir(dir.path()).unwrap();
        assert!(!inputs.vars.contains_key(&Ustr::from("REV")));
        let output = dir.path().join("program.lua");
        let recorded = crate::skull_backend::skull_compile_project_with_recorded_inputs(
            dir.path().to_str().unwrap(), Some(output.to_str().unwrap().into()), inputs,
        ).unwrap();
        let revision = &recorded.vars[&Ustr::from("REV")].value;
        assert!(revision.starts_with("source-v1:"));
        assert!(std::fs::read_to_string(output).unwrap().contains(revision));
    }

    #[test]
    fn source_fingerprint_preserves_file_identity_across_worktree_roots() {
        let fingerprint = |checkout: &str, filename: &str| {
            let root = std::path::PathBuf::from(checkout).join("app");
            let file = root.join(filename);
            let mut modules = vec![skull_parser::parse_module("struct Root { count: int }", Ustr::from(file.to_str().unwrap())).unwrap()];
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap();
            let mut inputs = ComptimeInputs::default();
            inputs.embed_root = Some(root);
            inputs.source_fingerprints.insert(Ustr::from("REV"), SourceFingerprint {
                secret: false, roots: vec![Ustr::from("Root")], files: vec![], vars: vec![],
            });
            resolve_source_fingerprints(&ws, &mut inputs, |_| Some("struct Root { count: int }".into()), "compiler").unwrap();
            inputs.vars[&Ustr::from("REV")].value.clone()
        };
        let first = fingerprint("/checkout-one", "types.rock");
        assert_eq!(first, fingerprint("/checkout-two", "types.rock"));
        assert_ne!(first, fingerprint("/checkout-one", "nested/types.rock"));
    }

    #[test]
    fn source_fingerprint_rejects_missing_source_and_input() {
        let (ws, mut inputs) = fixture();
        assert!(resolve_source_fingerprints(&ws, &mut inputs.clone(), |_| None, "compiler").is_err());
        inputs.vars.clear();
        assert!(resolve_source_fingerprints(&ws, &mut inputs, |_| Some("bytes".into()), "compiler").is_err());
    }
}
