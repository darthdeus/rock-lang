use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn content_revision(catalog: &Path) -> String {
    let script = repo_root().join("tools/release/content_revision.py");
    let out = Command::new("python3")
        .arg(script)
        .arg(catalog)
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
    let canonical_overlay = repo_root().join("../game-art-vault/mag");
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
