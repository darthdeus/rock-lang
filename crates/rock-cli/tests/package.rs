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
fn package_stages_external_asset_overlays() {
    let overlay = tempfile::tempdir().expect("overlay tempdir");
    let source = overlay.path().join("assets/overlay-package-sentinel.txt");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "from-overlay").unwrap();
    let out_dir = tempfile::tempdir().expect("package output");
    let overlays = std::env::join_paths([overlay.path()]).unwrap();

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
    assert_eq!(
        std::fs::read_to_string(
            out_dir.path().join("assets/mag/assets/overlay-package-sentinel.txt")
        )
        .unwrap(),
        "from-overlay"
    );
}
