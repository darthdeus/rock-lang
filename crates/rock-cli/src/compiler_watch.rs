use crate::{Path, PathBuf, Result};

pub(crate) fn is_compiler_crate_dir(name: &str) -> bool {
    matches!(name, "rock" | "skull" | "bone")
        || name.starts_with("rock-")
        || name.starts_with("skull-")
        || name.starts_with("bone-")
}

pub(crate) fn compiler_watch_paths() -> Vec<PathBuf> {
    let root = PathBuf::from(rock_host::rock_dir());
    let mut paths = vec![root.join("Cargo.toml"), root.join("Cargo.lock")];
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        for entry in entries.flatten() {
            if is_compiler_crate_dir(&entry.file_name().to_string_lossy()) {
                let crate_dir = entry.path();
                let src = crate_dir.join("src");
                if src.is_dir() {
                    paths.push(src);
                }
                let manifest = crate_dir.join("Cargo.toml");
                if manifest.is_file() {
                    paths.push(manifest);
                }
                let build_script = crate_dir.join("build.rs");
                if build_script.is_file() {
                    paths.push(build_script);
                }
            }
        }
    }
    paths
}

pub(crate) fn build_compiler() -> Result<()> {
    rock_engine_host::engine_build::require_source_build()?;
    rock_host::cargo::run_cargo(&["build", "--bin", "rock"], "Failed to build compiler")
}

pub(crate) fn is_compiler_source(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref().to_string_lossy().replace('\\', "/");
    if path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock") {
        return true;
    }
    if path.ends_with("/build.rs") {
        let crate_name = path
            .trim_end_matches("/build.rs")
            .rsplit('/')
            .next()
            .unwrap_or("");
        return is_compiler_crate_dir(crate_name);
    }
    if let Some(idx) = path.find("/src/") {
        let crate_name = path[..idx].rsplit('/').next().unwrap_or("");
        return is_compiler_crate_dir(crate_name);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_compiler_source;
    #[test]
    fn compiler_source_classification() {
        // Compiler crate src files -> rebuild the rock binary.
        assert!(is_compiler_source("rock-host/src/lib.rs"));
        assert!(is_compiler_source("rock-cli/src/stone.rs"));
        assert!(is_compiler_source("skull/src/lib.rs"));
        assert!(is_compiler_source("skull-codegen-lua/src/lib.rs"));
        assert!(is_compiler_source("bone-lower/src/lib.rs"));
        assert!(is_compiler_source("bone/src/x.rs"));
        // Absolute paths resolve by the crate dir before /src/.
        assert!(is_compiler_source(
            "/workspace/compiler/skull-transform/src/inline.rs"
        ));
        // Cargo manifests/lockfile -> rebuild.
        assert!(is_compiler_source("Cargo.toml"));
        assert!(is_compiler_source("skull/Cargo.toml"));
        assert!(is_compiler_source("rock-cli/build.rs"));

        // The engine is NOT a compiler source (rebuilt separately as libstone).
        assert!(!is_compiler_source("stone-rs/src/audio.rs"));
        assert!(!is_compiler_source("game-native/src/lib.rs"));
        // Game / stdlib / engine Rock code are not compiler sources.
        assert!(!is_compiler_source("game/actors/enemies.rock"));
        assert!(!is_compiler_source("stdlib/unified.rock"));
        assert!(!is_compiler_source("stone/engine/lib.rock"));
        // A crate whose name merely contains a prefix mid-path isn't matched.
        assert!(!is_compiler_source("some/rocky/src/x.rs"));
    }
}
