use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rock"))
}

fn run_rock(args: &[&str]) -> std::process::Output {
    Command::new(rock_bin())
        .current_dir(repo_root())
        .args(args)
        .output()
        .expect("spawn rock")
}

#[test]
fn mag_stamp_surfaces_are_declared_build_vars() {
    let root = repo_root();
    let telemetry = std::fs::read_to_string(root.join("mag/telemetry.rock")).unwrap();
    let build_info = std::fs::read_to_string(root.join("mag/build_info.rock")).unwrap();
    let stick = std::fs::read_to_string(root.join("mag/stick.toml")).unwrap();

    for name in [
        "GIT_SHA",
        "BUILD_UNIX",
        "BUILD_DATE",
        "CHANNEL",
        "CONTENT_REV",
        "PLATFORM",
        "TELEMETRY_ENDPOINT",
        "TELEMETRY_TOKEN",
    ] {
        assert!(stick.contains(&format!("{name}")), "mag/stick.toml lacks {name}");
    }
    for name in [
        "GIT_SHA",
        "CHANNEL",
        "CONTENT_REV",
        "PLATFORM",
        "TELEMETRY_ENDPOINT",
        "TELEMETRY_TOKEN",
    ] {
        assert!(telemetry.contains(&format!("build_var(\"{name}\")")), "telemetry lacks build_var({name})");
    }
    for name in ["GIT_SHA", "BUILD_DATE"] {
        assert!(build_info.contains(&format!("build_var(\"{name}\")")), "build_info lacks build_var({name})");
    }
    for name in [
        "mag_build_sha",
        "mag_build_channel",
        "mag_content_rev",
        "mag_telemetry_platform",
        "mag_telemetry_endpoint",
        "mag_telemetry_token",
    ] {
        assert!(!telemetry.contains(&format!("inline(lua) fn {name}")), "{name} still has an inline(lua) accessor");
        assert!(!telemetry.contains(&format!("inline(js) fn {name}")), "{name} still has an inline(js) accessor");
    }
    assert!(!build_info.contains("inline(lua)"), "build_info still has an inline(lua) stamp accessor");
    assert!(!build_info.contains("inline(js)"), "build_info still has an inline(js) stamp accessor");
    assert!(!telemetry.contains("__ROCK_"), "telemetry still reads a compatibility global");
    assert!(!build_info.contains("__ROCK_"), "build_info still reads a compatibility global");
}

#[test]
fn mag_lua_and_web_builds_carry_declared_identity() {
    let root = repo_root();
    let dir = tempfile::tempdir().unwrap();
    let lua_path = dir.path().join("mag.lua");
    let out = run_rock(&[
        "compile",
        "mag",
        "--channel",
        "steam",
        "--var",
        "TELEMETRY_TOKEN=known-test-token",
        "--output",
        lua_path.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "native mag compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let lua = std::fs::read_to_string(&lua_path).unwrap();
    let sha = String::from_utf8(
        Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert!(lua.contains(&sha), "Lua did not carry GIT_SHA={sha}");
    assert!(lua.contains("steam"), "Lua did not carry the selected channel");
    assert!(lua.contains("known-test-token"), "Lua did not carry the declared token");
    assert!(!lua.contains("__ROCK_BUILD_SHA"), "Lua still reads the old build global");
    assert!(!lua.contains("os.getenv('MAG_TELEMETRY_ENDPOINT')"));

    for (channel, platform) in [("direct", "linux"), ("ios-dev", "ios")] {
        let path = dir.path().join(format!("mag-{channel}.lua"));
        let out = run_rock(&[
            "compile",
            "mag",
            "--channel",
            channel,
            "--output",
            path.to_str().unwrap(),
        ]);
        assert!(
            out.status.success(),
            "{channel} mag compile failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let native = std::fs::read_to_string(path).unwrap();
        assert!(native.contains(channel), "Lua did not carry {channel}");
        assert!(native.contains(platform), "Lua did not carry {platform} platform");
    }

    let web_dir = dir.path().join("web");
    let out = run_rock(&["web", "mag", "--out", web_dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "web mag build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let js = std::fs::read_to_string(web_dir.join("game.js")).unwrap();
    assert!(js.contains(&sha), "web JS did not carry GIT_SHA={sha}");
    assert!(js.contains("web"), "web JS did not carry the web channel/platform");
    assert!(!js.contains("__ROCK_BUILD_SHA"), "web JS still carries the old build global");
    assert!(!js.contains("__ROCK_CHANNEL"), "web JS still carries the old channel global");
}
