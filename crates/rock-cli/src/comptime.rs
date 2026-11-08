//! Host-side resolution of comptime inputs:
//! turn a project's `[build.vars]` plus the invocation's `--channel`/`--var`
//! selection into the `ComptimeInputs` skull's `build_var`/`embed_string`
//! expansion reads off the Workspace. Commands run here, once per session build,
//! at the compile chokepoints -- never from the LSP, which performs no
//! emission and so never reaches this module.

use anyhow::Result;
use bone_core::prelude::Instant;
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
        .set(CliSelection {
            channel,
            implied_channel,
            target_os,
            overrides: parsed,
        })
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
        let err =
            set_invocation_selection(None, None, None, &[]).expect_err("second set must fail");
        assert!(err.to_string().contains("already initialized"));
    }
}

/// Stamp `ws` with the inputs resolved from `anchor`'s enclosing project.
/// This is the ONE resolution seam every EMITTING entry point routes through
/// -- per-site `ws.comptime_inputs = ...` assignments are how `rock test`,
/// `rock watch` and the profiling tools shipped broken (each new entry point
/// silently forgot the obligation). Type-check-only paths never need values
/// and the LSP never runs commands, so neither reaches this.
pub fn stamp_workspace(ws: &mut bone_semantic::inference::Workspace, anchor: &Path) -> Result<()> {
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
pub fn inputs_for_project_dir_as(
    dir: &Path,
    build_command: Option<&str>,
) -> Result<ComptimeInputs> {
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
            anyhow::bail!(
                "--var {name} given but no stick.toml found at or above {}",
                dir.display()
            );
        }
        return Ok(ComptimeInputs::default());
    };
    let project_dir = stick_path
        .parent()
        .expect("stick.toml has a parent dir")
        .to_path_buf();
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
        inputs.source_fingerprints.insert(
            ustr::Ustr::from(&name),
            bone_core::workspace::SourceFingerprint {
                secret: cfg
                    .build
                    .as_ref()
                    .and_then(|build| build.vars.get(&name))
                    .is_some_and(|var| var.secret),
                roots: declaration
                    .roots
                    .iter()
                    .map(|name| ustr::Ustr::from(name))
                    .collect(),
                files: declaration.files,
                vars: declaration
                    .vars
                    .iter()
                    .map(|name| ustr::Ustr::from(name))
                    .collect(),
            },
        );
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
            ComptimeVarValue {
                value: var.value,
                secret: var.secret,
            },
        );
    }
    Ok(inputs)
}

fn source_file_identity(file: &str, root: Option<&Path>) -> String {
    let path = Path::new(file);
    let Some(root) = root.filter(|_| path.is_absolute()) else {
        return file.to_string();
    };
    let root = root.canonicalize().unwrap_or_else(|_| {
        if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(root)
        }
    });
    let source = path.components().collect::<Vec<_>>();
    let anchor = root.components().collect::<Vec<_>>();
    let shared = source
        .iter()
        .zip(&anchor)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts = vec!["..".to_string(); anchor.len() - shared];
    parts.extend(
        source[shared..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

fn resolve_source_fingerprints(
    ws: &bone_semantic::inference::Workspace,
    inputs: &mut ComptimeInputs,
    mut source_text: impl FnMut(&str) -> Option<String>,
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
        let owners =
            bone_core::type_sources::declaration_sources(ws, &roots).map_err(anyhow::Error::msg)?;
        hash.update((owners.len() as u64).to_le_bytes());
        for owner in owners {
            // Canonical declaration identity is stable across worktree roots.
            // Hash all defining bytes, including defaults and helper bodies.
            field(&mut hash, owner.name.as_str());
            field(
                &mut hash,
                &source_file_identity(owner.file.as_str(), inputs.embed_root.as_deref()),
            );
            let text = source_text(owner.file.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "source fingerprint {name}: missing checked source {}",
                    owner.file
                )
            })?;
            field(&mut hash, &text);
        }
        let mut files = declaration.files.clone();
        files.sort();
        files.dedup();
        hash.update((files.len() as u64).to_le_bytes());
        for file in files {
            field(&mut hash, &file);
            let text = source_text(&file).ok_or_else(|| {
                anyhow::anyhow!("source fingerprint {name}: missing support source {file}")
            })?;
            field(&mut hash, &text);
        }
        let mut vars = declaration.vars.clone();
        vars.sort();
        vars.dedup();
        hash.update((vars.len() as u64).to_le_bytes());
        let mut secret = declaration.secret;
        for dependency in vars {
            let value = inputs.vars.get(&dependency).ok_or_else(|| {
                anyhow::anyhow!("source fingerprint {name}: missing input {dependency}")
            })?;
            if inputs.source_fingerprints.contains_key(&dependency)
                || inputs.volatile_vars.contains(&dependency)
            {
                anyhow::bail!(
                    "source fingerprint {name}: dependency {dependency} must be a stable resolved input"
                );
            }
            secret |= value.secret;
            field(&mut hash, dependency.as_str());
            field(&mut hash, &value.value);
        }
        resolved.push((
            *name,
            ComptimeVarValue {
                value: format!("source-v1:{:x}", hash.finalize()),
                secret,
            },
        ));
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
    inputs.source_fingerprint_files.clear();
    if inputs.source_fingerprints.is_empty() {
        return Ok(inputs);
    }
    let started = Instant::now();
    let root = inputs.embed_root.clone();
    let mut watch_files = rustc_hash::FxHashMap::default();
    resolve_source_fingerprints(
        ws,
        &mut inputs,
        |file| {
            if let Some(name) =
                rock_stdlib::stdlib_file_name(file).or_else(|| file.strip_prefix("stdlib/"))
            {
                return rock_stdlib::stdlib_files()
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| f.contents.to_string());
            }
            // `packages/` is an explicit toolchain-package selector. Resolve it
            // before project-relative inputs so a same-spelled local file
            // cannot shadow the selected package or satisfy a missing file.
            let package_path = file.strip_prefix("packages/");
            let path = if let Some(package_path) = package_path {
                let (package, relative) = package_path.split_once('/')?;
                Some(rock_stdlib::package_dir(package).ok()?.join(relative))
            } else {
                let path = Path::new(file);
                if path.is_absolute() {
                    Some(path.to_path_buf())
                } else {
                    root.as_ref().map(|root| root.join(path))
                }
            };
            let path = path.and_then(|path| path.canonicalize().ok());
            let text = if package_path.is_some() {
                let path = path.as_ref()?;
                source_text(path.to_str()?).or_else(|| std::fs::read_to_string(path).ok())?
            } else {
                source_text(file).or_else(|| {
                    let path = path.as_ref()?;
                    source_text(path.to_str()?).or_else(|| std::fs::read_to_string(path).ok())
                })?
            };
            if let Some(path) = path {
                watch_files.insert(
                    ustr::Ustr::from(path.to_str()?),
                    skull_mir::mir::comptime_value_hash(&text),
                );
            }
            Some(text)
        },
        &compiler_identity()?,
    )?;
    inputs.source_fingerprint_files = watch_files;
    crate::compile_measurements::timings("comptime", &[("source-fingerprint", started.elapsed())]);
    Ok(inputs)
}

/// Content identity of the running compiler image, shared by source inputs
/// and hosts' pre-compile cache keys. Never substitutes a checkout revision.
pub fn compiler_identity() -> Result<String> {
    static IDENTITY: OnceLock<Result<String, String>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let image = rock_host::process_management::current_executable_image()
                .map_err(|e| e.to_string())?;
            let mut file = std::fs::File::open(image).map_err(|e| e.to_string())?;
            let before = file.metadata().map_err(|e| e.to_string())?;
            #[cfg(target_os = "macos")]
            verify_macos_compiler_image(&before)?;
            let identity = hash_compiler_image(&mut file).map_err(|e| e.to_string())?;
            let after = file.metadata().map_err(|e| e.to_string())?;
            if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
                return Err("compiler image changed while hashing".into());
            }
            Ok(identity)
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Content identity of a compiler image at `image`, using the same
/// definition as [`compiler_identity`]. Hosts keying a compiler binary they
/// spawn (rather than the running one) must use this, not a plain file hash,
/// so both routes name the same compiler identically.
pub fn compiler_image_identity(image: &Path) -> Result<String> {
    let mut file = std::fs::File::open(image)?;
    let before = file.metadata()?;
    let identity = hash_compiler_image(&mut file)?;
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        anyhow::bail!("compiler image changed while hashing");
    }
    Ok(identity)
}

/// Hash the parts of a compiler image that can affect execution. An ELF
/// image (Linux) hashes only its `PT_LOAD` segments: everything the loader
/// maps, headers included in segment fields, with `p_memsz` covering BSS.
/// Bytes outside load segments -- DWARF debug sections, `.symtab`, section
/// headers -- cannot change what the compiler does, and on a Linux release
/// binary they are ~94% of the file (hundreds of ms of hashing per compile).
/// Debug info derives from the loaded code, so any compiler code change
/// still changes this identity. Non-ELF images (e.g. Mach-O, where debug
/// data already lives outside the executable) hash the whole file, as
/// before. Any ELF inconsistency also falls back to the whole file rather
/// than guessing.
fn hash_compiler_image<R: std::io::Read + std::io::Seek>(image: &mut R) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::SeekFrom;
    let mut hash = Sha256::new();
    hash.update(b"rock-compiler-identity-v2\0");
    let mut buffer = [0u8; 65536];
    match elf_load_segments(image)? {
        Some(segments) => {
            for segment in segments {
                hash.update(segment.flags.to_le_bytes());
                hash.update(segment.vaddr.to_le_bytes());
                hash.update(segment.filesz.to_le_bytes());
                hash.update(segment.memsz.to_le_bytes());
                image.seek(SeekFrom::Start(segment.offset))?;
                let mut remaining = segment.filesz;
                while remaining > 0 {
                    let want = remaining.min(buffer.len() as u64) as usize;
                    let count = image.read(&mut buffer[..want])?;
                    if count == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "compiler image ends inside a load segment",
                        ));
                    }
                    hash.update(&buffer[..count]);
                    remaining -= count as u64;
                }
            }
        }
        None => {
            image.seek(SeekFrom::Start(0))?;
            loop {
                let count = image.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

struct ElfLoadSegment {
    flags: u32,
    vaddr: u64,
    offset: u64,
    filesz: u64,
    memsz: u64,
}

/// `PT_LOAD` segments of an ELF image in file order, or `None` for a
/// non-ELF or inconsistent image (the caller then hashes the whole file).
fn elf_load_segments<R: std::io::Read + std::io::Seek>(
    image: &mut R,
) -> std::io::Result<Option<Vec<ElfLoadSegment>>> {
    use std::io::SeekFrom;
    let file_len = image.seek(SeekFrom::End(0))?;
    image.seek(SeekFrom::Start(0))?;
    let mut ident_buf = [0u8; 64];
    let read = image.read(&mut ident_buf)?;
    let ident = &ident_buf[..read];
    if read < 16 || ident[..4] != *b"\x7fELF" {
        return Ok(None);
    }
    let is64 = match ident[4] {
        1 => false,
        2 => true,
        _ => return Ok(None),
    };
    let little = match ident[5] {
        1 => true,
        2 => false,
        _ => return Ok(None),
    };
    let u16_at = |b: &[u8], at: usize| -> Option<u64> {
        let v = b.get(at..at + 2)?;
        Some(if little {
            u16::from_le_bytes([v[0], v[1]])
        } else {
            u16::from_be_bytes([v[0], v[1]])
        } as u64)
    };
    let u32_at = |b: &[u8], at: usize| -> Option<u64> {
        let v = b.get(at..at + 4)?;
        Some(if little {
            u32::from_le_bytes([v[0], v[1], v[2], v[3]])
        } else {
            u32::from_be_bytes([v[0], v[1], v[2], v[3]])
        } as u64)
    };
    let u64_at = |b: &[u8], at: usize| -> Option<u64> {
        if is64 {
            let v = b.get(at..at + 8)?;
            Some(if little {
                u64::from_le_bytes(v.try_into().ok()?)
            } else {
                u64::from_be_bytes(v.try_into().ok()?)
            })
        } else {
            u32_at(b, at)
        }
    };
    let (phoff, phentsize, phnum) = if is64 {
        match (u64_at(ident, 32), u16_at(ident, 54), u16_at(ident, 56)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Ok(None),
        }
    } else {
        match (u32_at(ident, 28), u16_at(ident, 42), u16_at(ident, 44)) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Ok(None),
        }
    };
    // PN_XNUM (0xffff) moves the count into section header 0; not expected
    // for a compiler binary, and the whole-file fallback stays correct.
    let min_phdr = if is64 { 56 } else { 32 };
    if phnum == 0 || phnum == 0xffff || phentsize < min_phdr {
        return Ok(None);
    }
    let Some(ph_end) = phnum
        .checked_mul(phentsize)
        .and_then(|span| phoff.checked_add(span))
    else {
        return Ok(None);
    };
    if ph_end > file_len {
        return Ok(None);
    }
    let mut phdr = vec![0u8; min_phdr as usize];
    let mut segments = Vec::new();
    for index in 0..phnum {
        image.seek(SeekFrom::Start(phoff + index * phentsize))?;
        image.read_exact(&mut phdr)?;
        if u32_at(&phdr, 0) != Some(1) {
            continue; // not PT_LOAD
        }
        let fields = if is64 {
            (
                u32_at(&phdr, 4),
                u64_at(&phdr, 8),
                u64_at(&phdr, 16),
                u64_at(&phdr, 32),
                u64_at(&phdr, 40),
            )
        } else {
            (
                u32_at(&phdr, 24),
                u64_at(&phdr, 4),
                u64_at(&phdr, 8),
                u64_at(&phdr, 16),
                u64_at(&phdr, 20),
            )
        };
        let (Some(flags), Some(offset), Some(vaddr), Some(filesz), Some(memsz)) = fields else {
            return Ok(None);
        };
        let segment = ElfLoadSegment {
            flags: flags as u32,
            vaddr,
            offset,
            filesz,
            memsz,
        };
        if segment
            .offset
            .checked_add(segment.filesz)
            .is_none_or(|end| end > file_len)
        {
            return Ok(None);
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        return Ok(None);
    }
    Ok(Some(segments))
}

#[cfg(test)]
mod compiler_image_tests {
    use super::hash_compiler_image;
    use std::io::Cursor;

    fn identity(bytes: &[u8]) -> String {
        hash_compiler_image(&mut Cursor::new(bytes.to_vec())).unwrap()
    }

    #[cfg(target_os = "macos")]
    fn run_macos_identity_child(mode: &str) {
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", "cp \"$1\" \"$2\" && exec \"$2\" --exact comptime::compiler_image_tests::macos_identity_child --nocapture", "identity-probe"])
            .arg(std::env::current_exe().unwrap())
            .arg(temp.path().join("compiler"))
            .env("ROCK_TEST_MACOS_IDENTITY", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_exec_accepts_image_created_after_process_start() {
        run_macos_identity_child("fresh");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_replaced_running_image() {
        run_macos_identity_child("replace");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_identity_child() {
        let Ok(mode) = std::env::var("ROCK_TEST_MACOS_IDENTITY") else {
            return;
        };
        if mode == "replace" {
            let image = std::env::current_exe().unwrap();
            let replacement = image.with_extension("replacement");
            std::fs::copy(&image, &replacement).unwrap();
            std::fs::rename(&replacement, &image).unwrap();
            assert!(
                super::compiler_identity()
                    .unwrap_err()
                    .to_string()
                    .contains("compiler image replaced after exec")
            );
        } else {
            assert!(!super::compiler_identity().unwrap().is_empty());
        }
    }

    /// Minimal ELF64-LE image: one PT_LOAD over "CODECODE" at offset 128,
    /// then trailing bytes outside every load segment (the debug stand-in).
    fn elf_image() -> Vec<u8> {
        let mut bytes = vec![0u8; 128];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // 64-bit
        bytes[5] = 1; // little-endian
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let ph = 64;
        bytes[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        bytes[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // flags R+X
        bytes[ph + 8..ph + 16].copy_from_slice(&128u64.to_le_bytes()); // offset
        bytes[ph + 16..ph + 24].copy_from_slice(&0x400000u64.to_le_bytes()); // vaddr
        bytes[ph + 32..ph + 40].copy_from_slice(&8u64.to_le_bytes()); // filesz
        bytes[ph + 40..ph + 48].copy_from_slice(&16u64.to_le_bytes()); // memsz
        bytes.extend_from_slice(b"CODECODE");
        bytes.extend_from_slice(b"DEBUGDEBUG");
        bytes
    }

    #[test]
    fn elf_identity_ignores_bytes_outside_load_segments() {
        let base = elf_image();
        let mut debug_changed = base.clone();
        *debug_changed.last_mut().unwrap() = b'X';
        assert_eq!(identity(&base), identity(&debug_changed));
        let mut longer_debug = base.clone();
        longer_debug.extend_from_slice(b"MOREDEBUG");
        assert_eq!(identity(&base), identity(&longer_debug));
    }

    #[test]
    fn elf_identity_tracks_loaded_bytes_and_segment_shape() {
        let base = elf_image();
        let mut code_changed = base.clone();
        code_changed[128] = b'X';
        assert_ne!(identity(&base), identity(&code_changed));
        let mut memsz_changed = base.clone();
        memsz_changed[64 + 40..64 + 48].copy_from_slice(&24u64.to_le_bytes());
        assert_ne!(identity(&base), identity(&memsz_changed));
    }

    #[test]
    fn non_elf_and_broken_elf_fall_back_to_whole_file() {
        assert_ne!(identity(b"plain bytes A"), identity(b"plain bytes B"));
        assert_eq!(identity(b"plain bytes"), identity(b"plain bytes"));
        // Truncated ELF header, and a phdr table past EOF: no panic, and a
        // trailing change still moves the whole-file fallback identity.
        let truncated = &elf_image()[..20];
        let _ = identity(truncated);
        let mut past_eof = elf_image();
        past_eof[32..40].copy_from_slice(&4096u64.to_le_bytes());
        let mut past_eof_changed = past_eof.clone();
        *past_eof_changed.last_mut().unwrap() = b'X';
        assert_ne!(identity(&past_eof), identity(&past_eof_changed));
    }
}

#[cfg(target_os = "macos")]
fn verify_macos_compiler_image(file: &std::fs::Metadata) -> std::result::Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    // sys/proc_info.h: libc exposes vnode_info_path but not these wrappers.
    #[repr(C)]
    struct RegionInfo {
        protection: u32,
        max_protection: u32,
        inheritance: u32,
        flags: u32,
        offset: u64,
        behavior: u32,
        user_wired_count: u32,
        user_tag: u32,
        pages_resident: u32,
        pages_shared_now_private: u32,
        pages_swapped_out: u32,
        pages_dirtied: u32,
        ref_count: u32,
        shadow_depth: u32,
        share_mode: u32,
        private_pages_resident: u32,
        shared_pages_resident: u32,
        obj_id: u32,
        depth: u32,
        address: u64,
        size: u64,
    }
    #[repr(C)]
    struct RegionWithPath {
        region: RegionInfo,
        vnode: libc::vnode_info_path,
    }
    const PROC_PIDREGIONPATHINFO: libc::c_int = 8;
    // libc deprecates its dyld bindings in favor of mach2, which is not a dependency.
    // SAFETY: dyld's image zero is the executable and lives for this process.
    #[allow(deprecated)]
    let header = unsafe { libc::_dyld_get_image_header(0) };
    if header.is_null() {
        return Err("running compiler image has no Mach-O header".into());
    }
    // SAFETY: both fields are C data structures whose zero values are valid.
    let mut info: RegionWithPath = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<RegionWithPath>() as libc::c_int;
    // SAFETY: info is writable for size bytes; header selects its mapped region.
    let written = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            PROC_PIDREGIONPATHINFO,
            header as u64,
            (&mut info as *mut RegionWithPath).cast(),
            size,
        )
    };
    if written != size {
        return Err(format!(
            "cannot inspect running compiler image: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mapped = info.vnode.vip_vi.vi_stat;
    // exec preserves process birth time. Compare the opened file with the
    // executable's mapped vnode instead, including after an atomic replacement.
    if u64::from(mapped.vst_dev) != file.dev() || mapped.vst_ino != file.ino() {
        return Err("compiler image replaced after exec".into());
    }
    Ok(())
}

#[cfg(test)]
mod source_fingerprint_tests {
    use super::*;
    use bone_core::workspace::SourceFingerprint;
    use bone_semantic::inference::Workspace;
    use ustr::Ustr;

    fn fixture() -> (Workspace, ComptimeInputs) {
        let mut modules = [
            ("root.rock", "struct Root { child: Child }"),
            ("child.rock", "value Child { count: int = 4 }"),
            ("other.rock", "struct Other { count: int }"),
        ]
        .iter()
        .map(|(file, text)| skull_parser::parse_module(text, Ustr::from(file)).unwrap())
        .collect::<Vec<_>>();
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap();
        let mut inputs = ComptimeInputs::default();
        inputs.source_fingerprints.insert(
            Ustr::from("REV"),
            SourceFingerprint {
                secret: false,
                roots: vec![Ustr::from("Root")],
                files: vec!["decoder.rock".into()],
                vars: vec![Ustr::from("ABI")],
            },
        );
        inputs.vars.insert(
            Ustr::from("ABI"),
            ComptimeVarValue {
                value: "runtime-1".into(),
                secret: false,
            },
        );
        (ws, inputs)
    }

    fn revision(
        ws: &Workspace,
        inputs: &ComptimeInputs,
        changed: &str,
        replacement: &str,
        compiler: &str,
    ) -> String {
        let mut inputs = inputs.clone();
        resolve_source_fingerprints(
            ws,
            &mut inputs,
            |file| {
                Some(
                    if file == changed {
                        replacement
                    } else {
                        match file {
                            "root.rock" => "struct Root { child: Child }",
                            "child.rock" => "value Child { count: int = 4 }",
                            "decoder.rock" => "fn decode() { 1 }",
                            "other.rock" => "struct Other { count: int }",
                            _ => return None,
                        }
                    }
                    .to_string(),
                )
            },
            compiler,
        )
        .unwrap();
        inputs.vars[&Ustr::from("REV")].value.clone()
    }

    #[test]
    fn source_fingerprint_tracks_whole_owners_support_runtime_and_compiler() {
        let (ws, inputs) = fixture();
        let baseline = revision(&ws, &inputs, "", "", "compiler-1");
        for (file, text) in [
            ("child.rock", "value Child { count: int = 5 }"),
            (
                "root.rock",
                "struct Root { child: Child }\nfn helper() { 2 }",
            ),
            ("decoder.rock", "fn decode() { 2 }"),
        ] {
            assert_ne!(
                baseline,
                revision(&ws, &inputs, file, text, "compiler-1"),
                "{file}"
            );
        }
        assert_eq!(
            baseline,
            revision(&ws, &inputs, "other.rock", "unrelated body", "compiler-1")
        );
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
        std::fs::write(
            dir.path().join("main.rock"),
            "value Root { count: int = 4 }\nfn main() { print(build_var(\"REV\")) }",
        )
        .unwrap();
        let inputs = inputs_for_project_dir(dir.path()).unwrap();
        assert!(!inputs.vars.contains_key(&Ustr::from("REV")));
        let output = dir.path().join("program.lua");
        let recorded = crate::skull_backend::skull_compile_project_with_recorded_inputs(
            dir.path().to_str().unwrap(),
            Some(output.to_str().unwrap().into()),
            inputs,
        )
        .unwrap();
        let revision = &recorded.vars[&Ustr::from("REV")].value;
        assert!(revision.starts_with("source-v1:"));
        assert!(std::fs::read_to_string(output).unwrap().contains(revision));
    }

    #[test]
    fn checked_source_fingerprint_uses_package_override() {
        const CHILD: &str = "ROCK_CLI_TEST_PACKAGE_FINGERPRINT_CHILD";
        const PACKAGE_ROOT: &str = "ROCK_CLI_TEST_PACKAGE_FINGERPRINT_ROOT";
        const PROJECT_ROOT: &str = "ROCK_CLI_TEST_PACKAGE_FINGERPRINT_PROJECT";

        if std::env::var_os(CHILD).is_some() {
            let package_root = std::path::PathBuf::from(std::env::var_os(PACKAGE_ROOT).unwrap());
            let project_root = std::path::PathBuf::from(std::env::var_os(PROJECT_ROOT).unwrap());
            let (ws, mut inputs) = fixture();
            inputs.embed_root = Some(project_root);
            inputs
                .source_fingerprints
                .get_mut(&Ustr::from("REV"))
                .unwrap()
                .files = vec!["packages/package-probe/decoder.rock".into()];
            let decode = "fn decode() -> int { 7 }";
            let read_checked_source = |file: &str| match file {
                "root.rock" => Some("struct Root { child: Child }".into()),
                "child.rock" => Some("value Child { count: int = 4 }".into()),
                _ => None,
            };

            let resolved =
                resolve_checked_inputs(&ws, inputs.clone(), read_checked_source).unwrap();
            let package_file = package_root
                .join("package-probe/decoder.rock")
                .canonicalize()
                .unwrap();
            let package_file_key = Ustr::from(package_file.to_str().unwrap());
            let revision = resolved.vars[&Ustr::from("REV")].value.clone();
            assert_eq!(
                resolved.source_fingerprint_files.get(&package_file_key),
                Some(&skull_mir::mir::comptime_value_hash(decode)),
                "fingerprint tracking must watch the resolved package file"
            );

            let changed_decode = "fn decode() -> int { 8 }";
            std::fs::write(&package_file, changed_decode).unwrap();
            let changed = resolve_checked_inputs(&ws, inputs.clone(), read_checked_source).unwrap();
            assert_ne!(revision, changed.vars[&Ustr::from("REV")].value);
            assert_eq!(
                changed.source_fingerprint_files.get(&package_file_key),
                Some(&skull_mir::mir::comptime_value_hash(changed_decode)),
                "watch hash must follow package-source changes"
            );

            std::fs::remove_file(package_file).unwrap();
            let missing = resolve_checked_inputs(&ws, inputs, read_checked_source)
                .expect_err("a project-root file must not satisfy a missing package file");
            assert!(
                missing
                    .to_string()
                    .contains("packages/package-probe/decoder.rock"),
                "{missing:#}"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let project_package = project_root.join("packages/package-probe");
        std::fs::create_dir_all(&project_package).unwrap();
        std::fs::write(
            project_package.join("decoder.rock"),
            "fn decode() -> int { 99 }",
        )
        .unwrap();

        let package_root = dir.path().join("package-overrides");
        let package = package_root.join("package-probe");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("stick.toml"),
            "[package]\nname = \"package-probe\"\n",
        )
        .unwrap();
        std::fs::write(package.join("decoder.rock"), "fn decode() -> int { 7 }").unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "comptime::source_fingerprint_tests::checked_source_fingerprint_uses_package_override",
            ])
            .env(CHILD, "1")
            .env("ROCK_PACKAGES_DIR", &package_root)
            .env(PACKAGE_ROOT, &package_root)
            .env(PROJECT_ROOT, &project_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "package fingerprint child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn source_fingerprint_preserves_file_identity_across_worktree_roots() {
        let fingerprint = |checkout: &str, filename: &str| {
            let root = std::path::PathBuf::from(checkout).join("app");
            let file = root.join(filename);
            let mut modules = vec![
                skull_parser::parse_module(
                    "struct Root { count: int }",
                    Ustr::from(file.to_str().unwrap()),
                )
                .unwrap(),
            ];
            let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).unwrap();
            let mut inputs = ComptimeInputs {
                embed_root: Some(root),
                ..Default::default()
            };
            inputs.source_fingerprints.insert(
                Ustr::from("REV"),
                SourceFingerprint {
                    secret: false,
                    roots: vec![Ustr::from("Root")],
                    files: vec![],
                    vars: vec![],
                },
            );
            resolve_source_fingerprints(
                &ws,
                &mut inputs,
                |_| Some("struct Root { count: int }".into()),
                "compiler",
            )
            .unwrap();
            inputs.vars[&Ustr::from("REV")].value.clone()
        };
        let first = fingerprint("/checkout-one", "types.rock");
        assert_eq!(first, fingerprint("/checkout-two", "types.rock"));
        assert_ne!(first, fingerprint("/checkout-one", "nested/types.rock"));
    }

    #[test]
    fn source_fingerprint_rejects_missing_source_and_input() {
        let (ws, mut inputs) = fixture();
        assert!(
            resolve_source_fingerprints(&ws, &mut inputs.clone(), |_| None, "compiler").is_err()
        );
        inputs.vars.clear();
        assert!(
            resolve_source_fingerprints(&ws, &mut inputs, |_| Some("bytes".into()), "compiler")
                .is_err()
        );
    }
}
