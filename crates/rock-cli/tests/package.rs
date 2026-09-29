use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

#[cfg(unix)]
#[test]
fn sdk_executable_can_discover_sound_project_in_separate_source_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let sdk = tempfile::tempdir().expect("release SDK fixture");
    std::fs::create_dir_all(sdk.path().join("bin")).unwrap();
    std::fs::create_dir_all(sdk.path().join("lib")).unwrap();
    std::os::unix::fs::symlink(repo_root().join("stone"), sdk.path().join("stone")).unwrap();
    std::os::unix::fs::symlink(repo_root().join("stdlib"), sdk.path().join("stdlib")).unwrap();
    let sdk_rock = sdk.path().join("bin/rock");
    std::fs::copy(rock_bin(), &sdk_rock).expect("copy release-layout executable");
    let mut permissions = std::fs::metadata(&sdk_rock).unwrap().permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(&sdk_rock, permissions).unwrap();

    let source = tempfile::tempdir().expect("source worktree fixture");
    std::fs::write(source.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::create_dir(source.path().join("prebuilt")).unwrap();
    let project = source.path().join("mag");
    std::fs::create_dir_all(project.join("tests/sims")).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        format!(
            "[package]\nname = \"external-sound-sim\"\ntype = \"bin\"\n\n[dependencies]\nstone = {{ path = \"{}\" }}\n\n[build.vars]\nGIT_SHA = \"fixture\"\n\n[sound]\nproject = \"sound_project.json\"\n",
            sdk.path().join("stone").display()
        ),
    )
    .unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(
        project.join("sound_project.json"),
        r#"{
  "version": 1,
  "events": [{"id": "fixture.event", "steps": [{"layers": [{"sources": ["assets/sfx/fixture.ogg"]}]}]}]
}
"#,
    )
    .unwrap();
    std::fs::create_dir_all(project.join("assets/sfx")).unwrap();
    std::fs::write(project.join("assets/sfx/fixture.ogg"), b"fixture").unwrap();

    let output = Command::new(&sdk_rock)
        .current_dir(&project)
        .env_remove("ROCK_DIR")
        .env_remove("ROCK_CONTEXT_ROOT")
        .env_remove("ROCK_STDLIB_DIR")
        .args(["sim", "--list", "."])
        .output()
        .expect("run release-layout executable against external source worktree");
    assert!(
        output.status.success(),
        "external sound project failed through release executable\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn content_revision(catalog: &Path) -> String {
    let script = repo_root().join("tools/release/content_revision.py");
    let out = Command::new("python3")
        .arg(script)
        .arg(catalog)
        .arg("prop_kits.json")
        .output()
        .expect("run content revision helper");
    assert!(
        out.status.success(),
        "content revision failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// The shipping package of the real game excludes the development agent
/// bridge on every surface the package owns: emitted game code, the staged
/// manifest, and staged assets. The development inclusion control is the
/// channel-scoped fixture test below; this test pins the production artifact.
#[test]
fn shipping_package_of_mag_excludes_the_agent_bridge() {
    let out_dir = tempfile::tempdir().expect("package output");
    let out = Command::new(rock_bin())
        .current_dir(repo_root())
        .args([
            "package",
            "mag",
            "--channel",
            "steam",
            "--var",
            "TELEMETRY_TOKEN=",
            "--output",
            out_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock package");
    assert!(
        out.status.success(),
        "rock package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let build = out_dir.path();
    let markers = [
        "agent_bridge_poll_system",
        "editor_ipc_dispatch",
        "stone_ipc_listen",
        "mag-agent",
        "stone-agent",
    ];
    let lua = std::fs::read_to_string(build.join("main.lua")).expect("packaged main.lua");
    for marker in markers {
        assert!(!lua.contains(marker), "packaged game code contains agent marker {marker:?}");
    }
    let manifest = std::fs::read_to_string(build.join("package-manifest.json")).expect("manifest");
    for marker in ["mag-agent", "stone-agent"] {
        assert!(!manifest.contains(marker), "package manifest references {marker:?}");
    }
    let staged = walkdir::WalkDir::new(build.join("assets"))
        .into_iter()
        .filter_map(Result::ok)
        .count();
    assert!(staged > 0, "package staged no assets");
    for entry in walkdir::WalkDir::new(build.join("assets"))
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            assert!(!name.contains("stone_agent"), "staged asset carries the agent cdylib: {}", name);
            assert!(!name.contains("mag-agent"), "staged asset carries the bridge package: {}", name);
        }
    }
}

#[test]
fn package_manifest_is_the_secret_safe_metadata_boundary() {
    let out_dir = tempfile::tempdir().expect("package output");
    let secret = "package-known-secret-9f8d";
    let out = Command::new(rock_bin())
        .current_dir(repo_root())
        .args([
            "package",
            "mag",
            "--channel",
            "steam",
            "--var",
            &format!("TELEMETRY_TOKEN={secret}"),
            "--output",
            out_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock package");
    assert!(
        out.status.success(),
        "rock package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let build = out_dir.path();
    let manifest_path = build.join("package-manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path).expect("package manifest");
    let manifest: Value = serde_json::from_str(&manifest_text).expect("valid package manifest");
    assert!(!manifest_text.contains(secret), "manifest leaked the secret");
    let expected = format!(
        "sha256:{:x}",
        Sha256::digest(secret.as_bytes())
    );
    assert_eq!(
        manifest["build_vars"]["TELEMETRY_TOKEN"]["fingerprint"],
        expected
    );
    assert_eq!(manifest["package"]["channel"], "steam");
    let packaged_root = build.join("assets/mag");
    let catalog = packaged_root.join("levels/index.json");
    assert!(catalog.is_file(), "packaged level catalog is missing: {}", catalog.display());
    let catalog_value: Value = serde_json::from_slice(&std::fs::read(&catalog).unwrap())
        .expect("packaged level catalog JSON");
    let levels = catalog_value["levels"].as_array().expect("catalog level entries");
    assert!(!levels.is_empty(), "package stages a nonempty official catalog");
    let mut identities = Vec::new();
    for entry in levels {
        let namespace = entry["namespace"].as_str().expect("catalog entry namespace");
        let name = entry["name"].as_str().expect("catalog entry name");
        let relative = entry["path"].as_str().expect("catalog entry path");
        let packaged = packaged_root.join(relative);
        let source = repo_root().join("mag").join(relative);
        assert!(packaged.is_file(), "packaged official asset is missing: {}", packaged.display());
        assert_eq!(
            std::fs::read(&packaged).unwrap(),
            std::fs::read(&source).unwrap(),
            "packaged {name} bytes must match the catalog source"
        );
        identities.push(format!("{namespace}:{name}"));
    }
    assert!(identities.iter().any(|id| id == "official:crypt"),
        "package catalog includes official:crypt");
    assert!(identities.iter().any(|id| id == "official:crypt-depths"),
        "package catalog includes official:crypt-depths");
    assert!(packaged_root.join("magic_trials.json").is_file(),
        "packaged Magic Trials catalog is missing");
    let packaged_kits = packaged_root.join("prop_kits.json");
    assert!(packaged_kits.is_file(), "packaged prop kit catalog is missing");
    assert_eq!(
        std::fs::read(&packaged_kits).unwrap(),
        std::fs::read(repo_root().join("mag/prop_kits.json")).unwrap(),
        "packaged prop kit catalog bytes must match the source"
    );
    let packaged_revision = manifest["build_vars"]["CONTENT_REV"]["value"]
        .as_str()
        .expect("CONTENT_REV value");
    assert_eq!(
        packaged_revision,
        content_revision(&catalog),
        "CONTENT_REV must hash the packaged catalog and its ordered assets"
    );
    assert_eq!(
        content_revision(&catalog),
        packaged_revision,
        "catalog content revision ordering must be deterministic"
    );

    let fixture = tempfile::tempdir().expect("catalog revision fixture");
    let fixture_root = fixture.path();
    let fixture_catalog = fixture_root.join("levels/index.json");
    std::fs::create_dir_all(fixture_catalog.parent().unwrap()).unwrap();
    std::fs::copy(&catalog, &fixture_catalog).unwrap();
    let fixture_kits = fixture_root.join("prop_kits.json");
    std::fs::copy(&packaged_kits, &fixture_kits).unwrap();
    let mut depth_path = None;
    for entry in levels {
        let relative = entry["path"].as_str().unwrap();
        let destination = fixture_root.join(relative);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(packaged_root.join(relative), &destination).unwrap();
        if entry["name"] == "crypt-depths" {
            depth_path = Some(destination);
        }
    }
    let depth_path = depth_path.expect("Crypt Depths catalog entry");
    let revision_before_mutations = content_revision(&fixture_catalog);
    let depth_bytes = std::fs::read(&depth_path).unwrap();
    let mut changed_depth = depth_bytes.clone();
    changed_depth.extend_from_slice(b"\n");
    std::fs::write(&depth_path, changed_depth).unwrap();
    assert_ne!(
        content_revision(&fixture_catalog),
        revision_before_mutations,
        "CONTENT_REV must change when Crypt Depths changes"
    );
    std::fs::write(&depth_path, depth_bytes).unwrap();
    let catalog_bytes = std::fs::read(&fixture_catalog).unwrap();
    let mut changed_catalog = catalog_bytes.clone();
    changed_catalog.extend_from_slice(b"\n");
    std::fs::write(&fixture_catalog, changed_catalog).unwrap();
    assert_ne!(
        content_revision(&fixture_catalog),
        revision_before_mutations,
        "CONTENT_REV must change when index.json changes"
    );
    std::fs::write(&fixture_catalog, catalog_bytes).unwrap();
    let kits_bytes = std::fs::read(&fixture_kits).unwrap();
    let mut changed_kits = kits_bytes.clone();
    changed_kits.extend_from_slice(b"\n");
    std::fs::write(&fixture_kits, changed_kits).unwrap();
    assert_ne!(
        content_revision(&fixture_catalog),
        revision_before_mutations,
        "CONTENT_REV must change when prop_kits.json changes"
    );

    let level = packaged_root.join("levels/crypt.json");
    assert!(level.is_file(), "packaged Crypt asset is missing: {}", level.display());
    let level_digest = format!("{:x}", Sha256::digest(std::fs::read(level).unwrap()));
    assert_ne!(
        manifest["build_vars"]["CONTENT_REV"]["value"],
        &level_digest[..12],
        "CONTENT_REV must not regress to the old single-file Crypt hash"
    );
    assert!(manifest["artifacts"]["lua"]["sha256"].as_str().is_some());
    assert!(manifest["artifacts"]["bytecode"]["sha256"].as_str().is_some());
    let keys = manifest["artifacts"]["assets"]["keys"].as_array().unwrap();
    assert!(!keys.is_empty(), "manifest asset key population is empty");
    assert!(
        keys.iter().any(|key| key == "stone/assets/bitro.wav"),
        "engine asset key missing from package manifest"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).find(secret).is_none()
            && String::from_utf8_lossy(&out.stderr).find(secret).is_none(),
        "package diagnostics leaked the secret"
    );
    for entry in walkdir::WalkDir::new(&build).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name.contains("manifest") || name.contains("index") || name.contains("cache") {
            let bytes = std::fs::read(entry.path()).unwrap();
            assert!(!bytes.windows(secret.len()).any(|window| window == secret.as_bytes()),
                "package metadata {} leaked the secret", entry.path().display());
        }
    }
}

#[test]
fn failed_package_does_not_leave_stale_top_level_artifacts() {
    let out_dir = tempfile::tempdir().expect("package output");
    for (name, bytes) in [
        ("main.lua", b"stale lua".as_slice()),
        ("main.bc", b"stale bytecode".as_slice()),
        ("package-manifest.json", b"stale manifest".as_slice()),
    ] {
        std::fs::write(out_dir.path().join(name), bytes).unwrap();
    }
    let no_tools = tempfile::tempdir().expect("empty PATH");
    let out = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("PATH", no_tools.path())
        .args([
            "package",
            "mag",
            "--output",
            out_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn failing rock package");
    assert!(!out.status.success(), "package must fail when its declared commands are unavailable");
    for name in ["main.lua", "main.bc", "package-manifest.json"] {
        assert!(
            !out_dir.path().join(name).exists(),
            "failed package left stale {name}"
        );
    }
}

#[test]
fn package_excludes_unrelated_external_asset_overlays() {
    let overlay = tempfile::tempdir().expect("overlay tempdir");
    let source = overlay.path().join("assets/overlay-package-sentinel.txt");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "from-overlay").unwrap();
    let out_dir = tempfile::tempdir().expect("package output");
    let canonical_overlay = rock_host::art_vault::root()
        .expect("the game-art vault is required for this package control")
        .join("mag");
    assert!(canonical_overlay.is_dir(), "the MAG audio overlay is required for this package control");
    let overlays = std::env::join_paths([overlay.path(), canonical_overlay.as_path()]).unwrap();

    let out = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("ROCK_ASSET_OVERLAYS", overlays)
        .args([
            "package",
            "mag",
            "--output",
            out_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn rock package");
    assert!(
        out.status.success(),
        "rock package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out_dir.path().join("assets/mag/assets/overlay-package-sentinel.txt").exists(),
        "unrelated overlay asset must not stage"
    );
}

#[test]
fn package_stages_declared_loose_overlay_assets_and_excludes_neighbors() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("loose-overlay-asset");
    let overlay = tempfile::tempdir().expect("overlay fixture");
    let output = tempfile::tempdir().expect("package output");
    std::fs::create_dir_all(project.join("assets")).unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(
        project.join("stick.toml"),
        concat!(
            "[package]\nname = \"loose-overlay-asset\"\n",
            "[release]\nassets = [\"assets\"]\n",
            "overlay_assets = [\"assets/particles/declared.png\"]\n",
        ),
    )
    .unwrap();

    let overlay_assets = overlay.path().join("assets");
    std::fs::create_dir_all(overlay_assets.join("particles")).unwrap();
    std::fs::write(
        project.join("assets/visual_bank.json"),
        concat!(
            "{\"schema_version\":1,",
            "\"plan_fingerprint\":\"sha256:0000000000000000000000000000000000000000000000000000000000000000\",",
            "\"dependencies\":[]}\n",
        ),
    )
    .unwrap();
    std::fs::write(overlay_assets.join("particles/declared.png"), b"declared").unwrap();
    std::fs::write(overlay_assets.join("particles/unrelated.png"), b"unrelated").unwrap();
    std::fs::write(
        overlay_assets.join("visual_bank.json"),
        concat!(
            "{\"schema_version\":1,",
            "\"plan_fingerprint\":\"sha256:0000000000000000000000000000000000000000000000000000000000000000\",",
            "\"dependencies\":[]}\n",
        ),
    )
    .unwrap();

    let packaged = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("ROCK_ASSET_OVERLAYS", overlay.path())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn package");
    assert!(
        packaged.status.success(),
        "declared loose overlay asset package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );

    let game_key = project.strip_prefix(repo_root()).unwrap().to_string_lossy().replace('\\', "/");
    let packaged_assets = output.path().join("assets").join(game_key).join("assets");
    let declared = packaged_assets.join("particles/declared.png");
    assert!(declared.is_file(), "declared loose overlay asset must stage");
    assert_eq!(
        std::fs::read(declared).unwrap(),
        b"declared"
    );
    assert!(
        !packaged_assets.join("particles/unrelated.png").exists(),
        "undeclared overlay neighbor must not stage"
    );

    std::fs::write(
        project.join("stick.toml"),
        concat!(
            "[package]\nname = \"loose-overlay-asset\"\n",
            "[release]\nassets = [\"assets\"]\n",
            "overlay_assets = [\"assets/particles/missing.png\"]\n",
        ),
    )
    .unwrap();
    let rejected_output = tempfile::tempdir().expect("rejected package output");
    let rejected = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("ROCK_ASSET_OVERLAYS", overlay.path())
        .args([
            "package",
            project.to_str().unwrap(),
            "--output",
            rejected_output.path().to_str().unwrap(),
        ])
        .output()
        .expect("spawn missing-overlay package");
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "missing declared overlay asset was accepted");
    assert!(
        stderr.contains("declared overlay asset does not exist")
            && stderr.contains("assets/particles/missing.png"),
        "missing overlay diagnostic was unclear: {stderr}"
    );
    assert!(
        !rejected_output.path().join("package-manifest.json").exists(),
        "rejected package published a manifest"
    );
}

#[test]
fn package_stages_the_worktree_visual_bank_and_exact_shared_dependencies() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("worktree-visual-bank");
    let overlay = fixture_root.path().join("overlay");
    let output = fixture_root.path().join("package");
    let digest = "a".repeat(64);
    let dependency = format!("assets/.eat/{digest}/characters/mage/mage-idle.png");
    let bank = format!(
        "{{\"schema_version\":1,\"plan_fingerprint\":\"sha256:{}\",\"dependencies\":[\"{}\"]}}\n",
        "0".repeat(64),
        dependency,
    );
    std::fs::create_dir_all(project.join("assets")).unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"worktree-visual-bank\"\n\n[release]\nassets = [\"assets\"]\n",
    )
    .unwrap();
    std::fs::write(project.join("assets/visual_bank.json"), &bank).unwrap();
    let immutable = overlay.join(format!("assets/.eat/{digest}/characters/mage"));
    std::fs::create_dir_all(&immutable).unwrap();
    std::fs::write(immutable.join("mage-idle.png"), b"selected-by-bank").unwrap();
    std::fs::write(
        overlay.join("assets/visual_bank.json"),
        format!(
            "{{\"schema_version\":1,\"plan_fingerprint\":\"sha256:{}\",\"dependencies\":[\"assets/.eat/{}/missing.png\"]}}\n",
            "0".repeat(64),
            "b".repeat(64),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(overlay.join("assets/.eat/unrelated")).unwrap();
    std::fs::write(
        overlay.join("assets/.eat/unrelated/neighbor.png"),
        b"not-selected",
    )
    .unwrap();

    let overlays = std::env::join_paths([overlay.as_path()]).unwrap();
    let packaged = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("ROCK_ASSET_OVERLAYS", overlays)
        .args([
            "package",
            project.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("spawn package");
    assert!(
        packaged.status.success(),
        "worktree visual bank package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );

    let game_key = project
        .strip_prefix(repo_root())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let packaged_root = output.join("assets").join(&game_key);
    assert_eq!(
        std::fs::read(packaged_root.join("assets/visual_bank.json")).unwrap(),
        bank.as_bytes()
    );
    assert_eq!(
        std::fs::read(packaged_root.join(&dependency)).unwrap(),
        b"selected-by-bank"
    );
    assert!(!packaged_root.join("assets/.eat/unrelated/neighbor.png").exists());

    let manifest: Value = serde_json::from_slice(
        &std::fs::read(output.join("package-manifest.json")).unwrap(),
    )
    .unwrap();
    let visual = &manifest["artifacts"]["visual"];
    assert_eq!(visual["bank"], format!("{game_key}/assets/visual_bank.json"));
    assert_eq!(
        visual["dependencies"],
        json!([format!("{game_key}/{dependency}")])
    );
    let keys = manifest["artifacts"]["assets"]["keys"].as_array().unwrap();
    assert!(keys.iter().any(|key| key == &format!("{game_key}/{dependency}")));
    assert!(keys
        .iter()
        .any(|key| key == &format!("{game_key}/assets/visual_bank.json")));
    assert!(!keys
        .iter()
        .any(|key| key == &format!("{game_key}/assets/.eat/unrelated/neighbor.png")));
}

#[test]
fn package_rejects_an_overlay_bank_when_the_worktree_bank_is_missing() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("missing-worktree-visual-bank");
    let overlay = fixture_root.path().join("overlay");
    let output = fixture_root.path().join("package");
    std::fs::create_dir_all(project.join("assets")).unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"missing-worktree-visual-bank\"\n\n[release]\nassets = [\"assets\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(overlay.join("assets")).unwrap();
    std::fs::write(
        overlay.join("assets/visual_bank.json"),
        concat!(
            "{\"schema_version\":1,",
            "\"plan_fingerprint\":\"sha256:0000000000000000000000000000000000000000000000000000000000000000\",",
            "\"dependencies\":[]}\n",
        ),
    )
    .unwrap();

    let overlays = std::env::join_paths([overlay.as_path()]).unwrap();
    let packaged = Command::new(rock_bin())
        .current_dir(repo_root())
        .env("ROCK_ASSET_OVERLAYS", overlays)
        .args([
            "package",
            project.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("spawn package");
    let stderr = String::from_utf8_lossy(&packaged.stderr);
    assert!(!packaged.status.success(), "package accepted an overlay bank without a worktree bank");
    assert!(
        stderr.contains("worktree visual bank is missing")
            && stderr.contains("refusing the overlay bank"),
        "missing worktree bank diagnostic was unclear: {stderr}"
    );
    assert!(!output.join("package-manifest.json").exists());
}

fn write_sound_package_fixture(project: &Path, sound_project: &str, assets: &[(&str, &[u8])]) {
    std::fs::create_dir_all(project).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"sound-release-fixture\"\n\n[release]\nassets = [\"assets\"]\n\n[sound]\nproject = \"sound_project.json\"\n",
    )
    .unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {}\n").unwrap();
    std::fs::write(project.join("sound_project.json"), sound_project).unwrap();
    for (relative, bytes) in assets {
        let path = project.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

#[test]
fn package_rejects_missing_sound_dependency_before_staging() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("missing-dependency");
    let output = tempfile::tempdir().expect("package output");
    let sound_project = r#"{
  "version": 1,
  "events": [{"id": "spell.fixture.cast", "steps": [{"layers": [{"sources": ["assets/sfx/missing.ogg"]}]}]}]
}
"#;
    write_sound_package_fixture(&project, sound_project, &[]);

    let rejected = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn scratch package");
    let rejected_stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "missing dependency was accepted: {rejected_stderr}");
    assert!(
        rejected_stderr.contains("assets/sfx/missing.ogg")
            && rejected_stderr.contains("referenced asset does not exist"),
        "missing dependency diagnostic was not preserved: {rejected_stderr}"
    );
    assert!(!output.path().join("assets").exists(), "missing dependency staged assets before rejecting");
    assert!(!output.path().join("assets.rock-staging").exists(), "missing dependency left a staging marker");
}

#[test]
fn package_rejects_absolute_and_traversal_sound_keys_before_staging() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    for (name, key) in [("absolute", "/machine/a.wav"), ("traversal", "assets/../a.wav")] {
        let project = fixture_root.path().join(name);
        let output = tempfile::tempdir().expect("package output");
        let sound_project = format!(
            r#"{{
  "version": 1,
  "events": [{{"id": "spell.fixture.cast", "steps": [{{"layers": [{{"sources": ["{key}"]}}]}}]}}]
}}
"#
        );
        write_sound_package_fixture(&project, &sound_project, &[]);

        let rejected = Command::new(rock_bin())
            .current_dir(repo_root())
            .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
            .output()
            .expect("spawn invalid-key package");
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(!rejected.status.success(), "invalid key was accepted: {stderr}");
        assert!(stderr.contains(key), "diagnostic omitted {key}: {stderr}");
        assert!(!output.path().join("assets").exists(), "invalid key staged assets");
    }
}

#[test]
fn package_rejects_obsolete_sound_schema_before_staging() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("obsolete-shape");
    let output = tempfile::tempdir().expect("package output");
    let obsolete = r#"{
  "version": 1,
  "cues": [{"id": "spell.fixture.cast", "steps": [{"layers": [{"asset": "assets/sfx/old.wav", "legacy_path": "/machine/a.wav"}]}]}]
}
"#;
    write_sound_package_fixture(&project, obsolete, &[]);

    let rejected = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn obsolete-shape package");
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "obsolete input was accepted: {stderr}");
    assert!(stderr.contains("unknown field") && stderr.contains("cues"), "diagnostic was unclear: {stderr}");
    assert!(!output.path().join("assets").exists(), "obsolete input staged assets");
}

#[test]
fn package_stages_compiled_sound_bank_and_exact_manifest_dependencies() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("valid-sound");
    let output = tempfile::tempdir().expect("package output");
    let sound_project = r#"{
  "version": 1,
  "events": [{
    "id": "spell.fixture.cast",
    "steps": [{"layers": [
      {"sources": ["assets/sfx/a.ogg", "assets/sfx/b.ogg"]},
      {"sources": ["assets/sfx/a.ogg"], "volume": 0.5, "speed": 1.25, "offset": 0.25}
    ]}]
  }]
}
"#;
    write_sound_package_fixture(
        &project,
        sound_project,
        &[("assets/sfx/a.ogg", b"a"), ("assets/sfx/b.ogg", b"b")],
    );

    let accepted = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn valid package");
    assert!(
        accepted.status.success(),
        "valid package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&accepted.stdout),
        String::from_utf8_lossy(&accepted.stderr)
    );

    let game_key = project.strip_prefix(repo_root()).unwrap().to_string_lossy().replace('\\', "/");
    let packaged_root = output.path().join("assets").join(&game_key);
    assert_eq!(std::fs::read(packaged_root.join("assets/sfx/a.ogg")).unwrap(), b"a");
    assert_eq!(std::fs::read(packaged_root.join("assets/sfx/b.ogg")).unwrap(), b"b");
    assert!(packaged_root.join("assets/sound_bank.json").is_file());
    assert!(!packaged_root.join("sound_project.json").exists(), "authoring project leaked into runtime assets");

    let manifest: Value = serde_json::from_slice(
        &std::fs::read(output.path().join("package-manifest.json")).unwrap(),
    )
    .expect("package manifest JSON");
    let sound = &manifest["artifacts"]["sound"];
    assert_eq!(sound["project"], "sound_project.json");
    assert_eq!(sound["bank"], format!("{game_key}/assets/sound_bank.json"));
    assert_eq!(sound["schema"], 1);
    assert_eq!(
        sound["dependencies"],
        serde_json::json!([format!("{game_key}/assets/sfx/a.ogg"), format!("{game_key}/assets/sfx/b.ogg")])
    );
    let keys = manifest["artifacts"]["assets"]["keys"].as_array().unwrap();
    for dependency in sound["dependencies"].as_array().unwrap() {
        assert!(keys.iter().any(|key| key == dependency), "manifest omitted {dependency}");
        assert!(output.path().join("assets").join(dependency.as_str().unwrap()).is_file());
    }
}

#[test]
fn package_accepts_sound_dependencies_in_arbitrarily_named_asset_directories() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("arbitrary-directories");
    let output = tempfile::tempdir().expect("package output");
    let keys = [
        "assets/sfx/_inbox/new.ogg",
        "assets/sfx/_sources/raw.ogg",
        "assets/sfx/scratch/test.ogg",
        "assets/sfx/tmp/test.ogg",
        "assets/sfx/temp/test.ogg",
    ];
    let sources = keys
        .iter()
        .map(|key| format!(r#"{{"sources":["{key}"]}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let sound_project = format!(
        r#"{{
  "version": 1,
  "events": [{{"id": "fixture.event", "steps": [{{"layers": [{sources}]}}]}}]
}}
"#
    );
    let assets = keys.map(|key| (key, b"sound".as_slice()));
    write_sound_package_fixture(&project, &sound_project, &assets);

    let accepted = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn package");
    assert!(
        accepted.status.success(),
        "arbitrarily named asset directory was rejected: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    let manifest: Value = serde_json::from_slice(
        &std::fs::read(output.path().join("package-manifest.json")).unwrap(),
    )
    .expect("package manifest JSON");
    let dependencies = manifest["artifacts"]["sound"]["dependencies"].as_array().unwrap();
    assert_eq!(dependencies.len(), keys.len());
    for key in keys {
        assert!(dependencies.iter().any(|dependency| {
            dependency.as_str().is_some_and(|dependency| dependency.ends_with(key))
        }));
    }
}

#[test]
fn sound_package_failure_is_transactional_for_generated_output() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("transactional-sound");
    let output = tempfile::tempdir().expect("package output");
    std::fs::write(output.path().join("caller-sentinel"), b"keep me").unwrap();
    let invalid = r#"{
  "version": 1,
  "events": [{"id": "spell.fixture.cast", "steps": [{"layers": [{"sources": ["assets/sfx/missing.wav"]}]}]}]
}
"#;
    write_sound_package_fixture(&project, invalid, &[]);

    let rejected = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn transactional package");
    assert!(!rejected.status.success(), "invalid sound package was accepted");
    assert_eq!(std::fs::read(output.path().join("caller-sentinel")).unwrap(), b"keep me");
    assert!(!output.path().join("assets").exists(), "failed sound package left generated assets");
    assert!(!output.path().join("package-manifest.json").exists(), "failed sound package left a manifest");
}

#[test]
fn packaged_sound_dependencies_survive_install_outside_source_tree() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("installed-sound");
    let output = tempfile::tempdir().expect("package output");
    let installed = tempfile::tempdir().expect("installed package");
    let sound_project = r#"{
  "version": 1,
  "events": [{"id": "spell.fixture.cast", "steps": [{"layers": [{"sources": ["assets/sfx/installed.ogg"]}]}]}]
}
"#;
    write_sound_package_fixture(&project, sound_project, &[("assets/sfx/installed.ogg", b"installed")]);
    let accepted = Command::new(rock_bin())
        .current_dir(repo_root())
        .args(["package", project.to_str().unwrap(), "--output", output.path().to_str().unwrap()])
        .output()
        .expect("spawn installed package");
    assert!(accepted.status.success(), "package failed: {}", String::from_utf8_lossy(&accepted.stderr));

    for entry in walkdir::WalkDir::new(output.path()).into_iter().filter_map(Result::ok) {
        let relative = entry.path().strip_prefix(output.path()).unwrap();
        let destination = installed.path().join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&destination).unwrap();
        } else {
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(installed.path().join("package-manifest.json")).unwrap(),
    )
    .expect("installed manifest JSON");
    for dependency in manifest["artifacts"]["sound"]["dependencies"].as_array().unwrap() {
        let relative = dependency.as_str().unwrap();
        assert!(installed.path().join("assets").join(relative).is_file(), "installed dependency missing: {relative}");
        assert!(!Path::new(relative).is_absolute());
    }
    assert!(installed.path().join("assets").join(
        manifest["artifacts"]["sound"]["bank"].as_str().unwrap()
    ).is_file());
}

/// The development-capability gate: a channel-scoped dependency compiles into
/// development builds and is excluded from shipping builds entirely, so
/// generated shipping code carries none of its symbols. The game references
/// no dependency symbol (the MAG shape: production code never names the agent
/// bridge), so the shipping arm must compile CLEANLY while excluding it. A
/// compile failure on the shipping arm is the fail-closed direction and is
/// covered by the resolver tests, not here.
#[test]
fn shipping_channel_excludes_a_channel_scoped_dependency_from_generated_code() {
    let fixture_root = tempfile::tempdir_in(repo_root()).expect("package fixture");
    let project = fixture_root.path().join("channel-gated");
    let dep = fixture_root.path().join("dev-bridge-lib");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        project.join("stick.toml"),
        "[package]\nname = \"channel-gated\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n\
         [build]\nchannels = [\"dev\", \"steam\"]\ndefault_channel = \"dev\"\n\n\
         [dependencies]\ndev-bridge-lib = { path = \"../dev-bridge-lib\", channels = [\"dev\"] }\n",
    )
    .unwrap();
    std::fs::write(project.join("main.rock"), "fn main() {\n    print(\"game-body-only\")\n}\n").unwrap();
    std::fs::write(
        dep.join("stick.toml"),
        "[package]\nname = \"dev-bridge-lib\"\nversion = \"0.1.0\"\ntype = \"lib\"\n",
    )
    .unwrap();
    // The global initializer is the emission witness: an uncalled fn in the
    // dependency is dead-stripped, but a module-load effect is always emitted,
    // so its presence in the output proves the package compiled into the build.
    std::fs::write(
        dep.join("bridge.rock"),
        "global AGENT_BRIDGE_PRESENT: bool = false\n\nfn agent_bridge_marker() -> string {\n    return \"agent-bridge-marker\"\n}\n",
    )
    .unwrap();

    let output_dir = tempfile::tempdir().expect("compile output");
    let compile = |channel: &str, file: &str| {
        let output = output_dir.path().join(file);
        let result = Command::new(rock_bin())
            .current_dir(repo_root())
            .args([
                "compile",
                project.to_str().unwrap(),
                "--channel",
                channel,
                "--output",
                output.to_str().unwrap(),
            ])
            .output()
            .expect("spawn compile");
        (result, output)
    };

    let (shipping, shipping_path) = compile("steam", "steam.lua");
    assert!(
        shipping.status.success(),
        "shipping compile must succeed without the scoped dependency: {}",
        String::from_utf8_lossy(&shipping.stderr)
    );
    let shipping_lua = std::fs::read_to_string(&shipping_path).unwrap();
    assert!(
        !shipping_lua.contains("AGENT_BRIDGE_PRESENT"),
        "generated shipping code must exclude the channel-scoped dependency"
    );

    let (development, dev_path) = compile("dev", "dev.lua");
    assert!(
        development.status.success(),
        "development compile failed: {}",
        String::from_utf8_lossy(&development.stderr)
    );
    let dev_lua = std::fs::read_to_string(&dev_path).unwrap();
    assert!(
        dev_lua.contains("AGENT_BRIDGE_PRESENT"),
        "generated development code must include the channel-scoped dependency"
    );
}
