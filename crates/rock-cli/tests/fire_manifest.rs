use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
fn fire_identity_is_consumed_from_package_metadata() {
    let root = repo_root();
    let build = std::fs::read_to_string(root.join("crates/fire/build.rs")).unwrap();
    let main = std::fs::read_to_string(root.join("crates/fire/src/main.rs")).unwrap();

    assert!(build.contains("package-manifest.json"), "Fire build must consume the package manifest");
    for forbidden in [
        "git describe",
        "SystemTime",
        "ROCK_CHANNEL",
        "ROCK_TELEMETRY_ENDPOINT",
        "ROCK_TELEMETRY_TOKEN",
        "emit_content_revision",
        "FIRE_CONTENT_REV",
    ] {
        assert!(!build.contains(forbidden), "Fire build.rs still owns {forbidden}");
    }
    for forbidden in [
        "__ROCK_BUILD_SHA",
        "__ROCK_BUILD_UNIX",
        "__ROCK_CHANNEL",
        "__ROCK_CONTENT_REV",
        "__ROCK_PLATFORM",
        "__ROCK_TELEMETRY_ENDPOINT",
        "__ROCK_TELEMETRY_TOKEN",
        "__ROCK_CHANNEL_STAMP",
    ] {
        assert!(!main.contains(forbidden), "Fire runtime still writes obsolete {forbidden}");
    }
}
