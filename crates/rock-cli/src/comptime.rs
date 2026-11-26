//! Resolve and stamp host-owned comptime inputs.

use anyhow::{Context, Result};
use bone_semantic::inference::{ComptimeInputs, ComptimeVarValue};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Default)]
struct CliSelection {
    channel: Option<String>,
    target_os: Option<String>,
    overrides: std::collections::HashMap<String, String>,
}

static SELECTION: OnceLock<CliSelection> = OnceLock::new();

pub(crate) fn set_invocation_selection(
    channel: Option<String>,
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
    rock_host::stick::set_invocation_channel(channel.clone());
    SELECTION
        .set(CliSelection {
            channel,
            target_os,
            overrides: parsed,
        })
        .map_err(|_| anyhow::anyhow!("CLI invocation selection was already initialized"))?;
    Ok(())
}

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
        assert!(set_invocation_selection(None, None, &[]).is_ok());
        let err = set_invocation_selection(None, None, &[]).expect_err("second set must fail");
        assert!(err.to_string().contains("already initialized"));
    }
}

pub(crate) fn stamp_workspace(
    ws: &mut bone_semantic::inference::Workspace,
    anchor: &Path,
) -> Result<()> {
    ws.comptime_inputs = resolve_checked_inputs(ws, inputs_for_entry_path(anchor)?, |_| None)?;
    Ok(())
}

pub(crate) fn inputs_for_entry_path(anchor: &Path) -> Result<ComptimeInputs> {
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

pub(crate) enum ProjectInputs {
    Ready(ComptimeInputs),
    Resolving(std::thread::JoinHandle<Result<ComptimeInputs>>),
}

impl ProjectInputs {
    pub(crate) fn resolve(dir: &Path) -> Self {
        let dir = dir.to_path_buf();
        Self::Resolving(std::thread::spawn(move || {
            let inputs = inputs_for_project_dir(&dir)?;
            prefetch_compiler_identity(&inputs);
            Ok(inputs)
        }))
    }

    pub(crate) fn wait(self) -> Result<ComptimeInputs> {
        match self {
            Self::Ready(inputs) => Ok(inputs),
            Self::Resolving(thread) => thread
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
        }
    }
}

impl From<ComptimeInputs> for ProjectInputs {
    fn from(inputs: ComptimeInputs) -> Self {
        prefetch_compiler_identity(&inputs);
        Self::Ready(inputs)
    }
}

pub(crate) fn inputs_for_project_dir(dir: &Path) -> Result<ComptimeInputs> {
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
    inputs_for_project_with_config(&project_dir, &cfg)
}

pub(crate) fn inputs_for_project_with_config(
    project_dir: &Path,
    cfg: &rock_host::stick::StickConfig,
) -> Result<ComptimeInputs> {
    let selection = SELECTION.get();
    let channel = selection.and_then(|s| s.channel.as_deref());
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
    inputs.volatile_vars = resolved
        .volatile_vars
        .into_iter()
        .map(|name| ustr::Ustr::from(&name))
        .collect();
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
    let mut identities = rustc_hash::FxHashMap::default();
    let mut texts = rustc_hash::FxHashMap::default();
    let mut source_text = |file: &str| -> Option<String> {
        texts
            .entry(file.to_string())
            .or_insert_with(|| source_text(file))
            .clone()
    };
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
        let scope = ws
            .ns
            .package(0)
            .map(|package| bone_core::modules::FragmentScope {
                package: package.id,
                package_name: package.self_name,
                ..Default::default()
            });
        let resolved_roots = roots
            .iter()
            .map(|root| {
                if ws.type_generics.contains_key(root) {
                    return Ok(*root);
                }
                let Some(scope) = &scope else {
                    return Ok(*root);
                };
                let path = bone_core::modules::UsePathRef {
                    from_root: root.starts_with("::"),
                    segments: root
                        .trim_start_matches("::")
                        .split("::")
                        .map(ustr::Ustr::from)
                        .collect(),
                };
                bone_semantic::lower::resolve::resolve_path(ws, scope, &path)
                    .map(|resolved| resolved.key)
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(anyhow::Error::msg)?;
        let owners = bone_core::type_sources::declaration_sources(ws, &resolved_roots)
            .map_err(anyhow::Error::msg)?;
        hash.update((owners.len() as u64).to_le_bytes());
        for owner in owners {
            field(
                &mut hash,
                bone_core::modules::display_name(owner.name.as_str()),
            );
            let identity = identities.entry(owner.file).or_insert_with(|| {
                source_file_identity(owner.file.as_str(), inputs.embed_root.as_deref())
            });
            field(&mut hash, identity);
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

pub(crate) fn resolve_source_fingerprint_support_path(
    file: &str,
    embed_root: Option<&Path>,
) -> Result<PathBuf> {
    let declared_path = Path::new(file);
    let support_path = if let Some(package_path) = file.strip_prefix("packages/") {
        let (package, relative) = package_path
            .split_once('/')
            .context("package support files must name a package and relative path")?;
        rock_stdlib::package_dir(package)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("resolve source fingerprint package {package:?}"))?
            .join(relative)
    } else if declared_path.is_absolute() {
        declared_path.to_path_buf()
    } else {
        embed_root
            .with_context(|| {
                format!("source fingerprint support file {file:?} requires an embed root")
            })?
            .join(declared_path)
    };
    support_path.canonicalize().with_context(|| {
        format!(
            "resolve source fingerprint support file {}",
            support_path.display()
        )
    })
}

pub(crate) fn resolve_checked_inputs(
    ws: &bone_semantic::inference::Workspace,
    mut inputs: ComptimeInputs,
    source_text: impl Fn(&str) -> Option<String>,
) -> Result<ComptimeInputs> {
    inputs.source_fingerprint_files.clear();
    if inputs.source_fingerprints.is_empty() {
        return Ok(inputs);
    }
    let root = inputs.embed_root.clone();
    let mut watch_files = rustc_hash::FxHashMap::default();
    resolve_source_fingerprints(
        ws,
        &mut inputs,
        |file| {
            if let Some(name) =
                rock_stdlib::stdlib_file_name(file).or_else(|| file.strip_prefix("stdlib/"))
            {
                return source_text(&rock_stdlib::stdlib_file_id(name)).or_else(|| {
                    rock_stdlib::stdlib_files()
                        .iter()
                        .find(|f| f.name == name)
                        .map(|f| f.contents.to_string())
                });
            }
            // `packages/` selects the toolchain package before project paths,
            // so a same-spelled local file cannot shadow it or satisfy a miss.
            let package_path = file.starts_with("packages/");
            let path = resolve_source_fingerprint_support_path(file, root.as_deref()).ok();
            let text = if package_path {
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
    Ok(inputs)
}

pub(crate) fn prefetch_compiler_identity(inputs: &ComptimeInputs) {
    if inputs.source_fingerprints.is_empty() {
        return;
    }
    std::thread::spawn(|| {
        let _ = compiler_identity();
    });
}

pub(crate) fn compiler_identity() -> Result<String> {
    static IDENTITY: OnceLock<Result<String, String>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let image = rock_host::process_management::current_executable_image()
                .map_err(|e| e.to_string())?;
            let mut file = std::fs::File::open(&image).map_err(|e| e.to_string())?;
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

/// Hash the parts of a compiler image that can affect execution. An ELF
/// image (Linux) hashes only its `PT_LOAD` segments: everything the loader
/// maps, headers included in segment fields, with `p_memsz` covering BSS.
fn hash_compiler_image<R: std::io::Read + std::io::Seek>(image: &mut R) -> std::io::Result<String> {
    use std::io::SeekFrom;
    // Identity, not authentication: a 128-bit non-cryptographic hash reads
    // the image at memory speed, where SHA-256 cost ~200 ms per compile.
    let mut hash = xxhash_rust::xxh3::Xxh3::new();
    hash.update(b"rock-compiler-identity-v3\0");
    let mut buffer = [0u8; 65536];
    match elf_load_segments(image)? {
        Some(segments) => {
            for segment in segments {
                hash.update(&segment.flags.to_le_bytes());
                hash.update(&segment.vaddr.to_le_bytes());
                hash.update(&segment.filesz.to_le_bytes());
                hash.update(&segment.memsz.to_le_bytes());
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
    Ok(format!("{:032x}", hash.digest128()))
}

struct ElfLoadSegment {
    flags: u32,
    vaddr: u64,
    offset: u64,
    filesz: u64,
    memsz: u64,
}

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
    let (word, phoff_at, phentsize_at, min_phdr, flags_at, offsets) = match ident[4] {
        1 => (4, 28, 42, 32, 24, [4, 8, 16, 20]),
        2 => (8, 32, 54, 56, 4, [8, 16, 32, 40]),
        _ => return Ok(None),
    };
    if ident.len() < phentsize_at + 4 {
        return Ok(None);
    }
    let little = match ident[5] {
        1 => true,
        2 => false,
        _ => return Ok(None),
    };
    let u16_at = |bytes: &[u8], at| {
        let value = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        u64::from(if little { value } else { value.swap_bytes() })
    };
    let u32_at = |bytes: &[u8], at| {
        let value = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if little { value } else { value.swap_bytes() }
    };
    let word_at = |bytes: &[u8], at| {
        if word == 4 {
            return u64::from(u32_at(bytes, at));
        }
        let value = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        if little { value } else { value.swap_bytes() }
    };
    let phoff = word_at(ident, phoff_at);
    let phentsize = u16_at(ident, phentsize_at);
    let phnum = u16_at(ident, phentsize_at + 2);
    // PN_XNUM moves the count into section header zero. Use the whole image.
    if phnum == 0 || phnum == 0xffff || phentsize < min_phdr {
        return Ok(None);
    }
    if phoff
        .checked_add(phnum * phentsize)
        .is_none_or(|end| end > file_len)
    {
        return Ok(None);
    }
    let mut phdr = [0; 56];
    let phdr = &mut phdr[..min_phdr as usize];
    let mut segments = Vec::new();
    for index in 0..phnum {
        image.seek(SeekFrom::Start(phoff + index * phentsize))?;
        image.read_exact(phdr)?;
        if u32_at(phdr, 0) != 1 {
            continue; // PT_LOAD
        }
        let [offset, vaddr, filesz, memsz] = offsets.map(|at| word_at(phdr, at));
        let segment = ElfLoadSegment {
            flags: u32_at(phdr, flags_at),
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
    fn elf32_big_endian_uses_the_same_identity_fields_as_elf64() {
        let mut image = vec![0; 52];
        image[..6].copy_from_slice(b"\x7fELF\x01\x02");
        image[28..32].copy_from_slice(&52u32.to_be_bytes()); // e_phoff
        image[42..44].copy_from_slice(&32u16.to_be_bytes()); // e_phentsize
        image[44..46].copy_from_slice(&1u16.to_be_bytes()); // e_phnum
        for field in [1u32, 84, 0x400000, 0, 8, 16, 5, 0] {
            image.extend_from_slice(&field.to_be_bytes());
        }
        image.extend_from_slice(b"CODECODE");
        assert_eq!(identity(&image), identity(&elf_image()));
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
        .map(|(file, text)| {
            let mut module = skull_parser::parse_module(text, Ustr::from(file)).unwrap();
            module.package = Some(skull_parser::PackageTag {
                id: 0,
                self_name: "@/<fingerprint>".into(),
                identity_storage: skull_parser::IdentityStorage::Exempt,
                root: String::new(),
            });
            module
        })
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
            inputs.embed_root = Some(project_root.clone());
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

            std::fs::write(
                project_root.join("packages/package-probe/decoder.rock"),
                "fn decode() -> int { 100 }",
            )
            .unwrap();
            let decoyed = resolve_checked_inputs(&ws, inputs.clone(), read_checked_source).unwrap();
            assert_eq!(
                revision,
                decoyed.vars[&Ustr::from("REV")].value,
                "project-local decoys must not participate in a package fingerprint"
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

            std::fs::remove_file(&package_file).unwrap();
            let missing = resolve_checked_inputs(&ws, inputs.clone(), read_checked_source)
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
