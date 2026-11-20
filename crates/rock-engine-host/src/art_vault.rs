//! Location of the game-art vault: the per-machine store for source kits,
//! texture originals and baked runtime objects. Manifests and Stick configs
//! name it with the `$GAME_ART_VAULT` token so each machine can keep the vault
//! wherever its disks are.

use anyhow::Result;
use std::ffi::OsStr;
use std::path::PathBuf;

pub const ENV: &str = "GAME_ART_VAULT";
pub const TOKEN: &str = "$GAME_ART_VAULT";
pub fn root() -> Result<PathBuf> {
    let env = std::env::var_os(ENV);
    root_from(env.as_deref())
}

pub(crate) fn root_from(env: Option<&OsStr>) -> Result<PathBuf> {
    let value = env
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("cannot locate the art vault: {ENV} is unset"))?;
    let root = PathBuf::from(value);
    if !root.is_absolute() {
        anyhow::bail!("{ENV} must be an absolute path, got {}", root.display());
    }
    if !root.is_dir() {
        anyhow::bail!(
            "art vault not found at {}; set {ENV} to the vault directory",
            root.display()
        );
    }
    Ok(root)
}

pub fn expand_token(raw: &str, root: impl FnOnce() -> Result<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(rest) = raw.strip_prefix(TOKEN) else {
        return Ok(None);
    };
    if rest.is_empty() {
        return root().map(Some);
    }
    match rest.strip_prefix('/') {
        Some(tail) => Ok(Some(root()?.join(tail))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vault_root_is_an_existing_absolute_env_path() {
        let vault = tempfile::tempdir().unwrap();
        assert_eq!(
            root_from(Some(vault.path().as_os_str())).unwrap(),
            vault.path()
        );
        for (env, want) in [
            (None, "GAME_ART_VAULT"),
            (Some(""), "GAME_ART_VAULT"),
            (Some("/nonexistent/vault"), "/nonexistent/vault"),
            (Some("relative-vault"), "absolute"),
        ] {
            let err = root_from(env.map(OsStr::new)).unwrap_err().to_string();
            assert!(err.contains(want), "{env:?}: {err}");
        }
    }

    #[test]
    fn token_expands_only_as_a_whole_leading_component() {
        let root = || Ok(PathBuf::from("/vault"));
        assert_eq!(
            expand_token("$GAME_ART_VAULT", root).unwrap(),
            Some(PathBuf::from("/vault"))
        );
        assert_eq!(
            expand_token("$GAME_ART_VAULT/blender-assets/kit/a.glb", root).unwrap(),
            Some(PathBuf::from("/vault/blender-assets/kit/a.glb"))
        );
        let untouched = || -> Result<PathBuf> { panic!("root must not be resolved") };
        assert_eq!(expand_token("$GAME_ART_VAULTS/a", untouched).unwrap(), None);
        assert_eq!(expand_token("~/projects/a", untouched).unwrap(), None);
        let err =
            expand_token("$GAME_ART_VAULT/demo", || anyhow::bail!("vault not found")).unwrap_err();
        assert!(err.to_string().contains("vault not found"));
    }
}
