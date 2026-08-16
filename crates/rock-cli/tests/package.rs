use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

#[test]
fn package_manifest_is_the_secret_safe_metadata_boundary() {
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
        ])
        .output()
        .expect("spawn rock package");
    assert!(
        out.status.success(),
        "rock package failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let root = repo_root();
    let build = root.join("crates/fire/build");
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
    let level = build.join("assets/mag/saves/level.json");
    let level_digest = format!("{:x}", Sha256::digest(std::fs::read(level).unwrap()));
    assert_eq!(
        manifest["build_vars"]["CONTENT_REV"]["value"],
        &level_digest[..12],
        "CONTENT_REV must preserve the old staged-level fingerprint semantics"
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
