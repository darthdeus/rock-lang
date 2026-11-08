//! Read-only adoption preflight. Compilation and runtime remain separate gates.

use anyhow::Result;
use std::path::Path;

fn native_library_name() -> String {
    format!(
        "{}stone.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    )
}

pub fn check(project: &str, sdk: &Path) -> Result<()> {
    let config = rock_host::stick::StickConfig::from_path(&Path::new(project).join("stick.toml"))?;
    config.validate_build()?;
    rock_engine_host::engine_build::game_engine_libs(project)?;
    let library = sdk.join("lib").join(native_library_name());
    if !library.is_file() {
        anyhow::bail!("SDK native library is missing: {}", library.display());
    }
    // Sound preparation uses the source workspace even when the compiler lives
    // in an extracted SDK. Resolve it here without compiling or writing a bank.
    (crate::registry::RuntimeServices::registered().sound_project_path)(project)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_manifest_compatibility_checks_builtins_and_native_needs() {
        let root = tempfile::tempdir().unwrap();
        let game = root.path().join("game");
        let sdk = root.path().join("sdk");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir_all(sdk.join("lib")).unwrap();
        let manifest = game.join("stick.toml");
        let source = "[package]\nname = \"game\"\n[build.vars]\nGIT_SHA = { builtin = \"source_revision\" }\n";
        std::fs::write(&manifest, source).unwrap();
        let error = check(game.to_str().unwrap(), &sdk).unwrap_err().to_string();
        assert!(error.contains(&native_library_name()), "{error}");
        std::fs::write(sdk.join("lib").join(native_library_name()), b"fixture").unwrap();
        check(game.to_str().unwrap(), &sdk).unwrap();
        std::fs::write(
            &manifest,
            source.replace("source_revision", "unknown_builtin"),
        )
        .unwrap();
        assert!(
            check(game.to_str().unwrap(), &sdk)
                .unwrap_err()
                .to_string()
                .contains("unknown_builtin")
        );
        std::fs::write(
            &manifest,
            format!(
                "{source}\n[native]\nmanifest = \"missing/Cargo.toml\"\nlibrary = \"missing\"\n"
            ),
        )
        .unwrap();
        assert!(
            check(game.to_str().unwrap(), &sdk)
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
    }
}
