//! Location of the game-art vault: the per-machine store for source kits,
//! texture originals and baked runtime objects. Manifests and Stick configs
//! name it with the `$GAME_ART_VAULT` token so each machine can keep the vault
//! wherever its disks are.

use anyhow::Result;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(crate) const ENV: &str = "GAME_ART_VAULT";
pub const TOKEN: &str = "$GAME_ART_VAULT";
/// The vault root named by the environment, validated before use.
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

/// Expand a leading `$GAME_ART_VAULT` token. `Ok(None)` when `raw` does not
/// start with the token; `root` is only called when it does.
pub(crate) fn expand_token(
    raw: &str,
    root: impl FnOnce() -> Result<PathBuf>,
) -> Result<Option<PathBuf>> {
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

/// `path` written as `$GAME_ART_VAULT/...` when it lies under `root`. Callers
/// compare canonical paths so a symlinked vault still matches.
pub fn tokenize(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(if relative.as_os_str().is_empty() {
        TOKEN.to_string()
    } else {
        format!("{TOKEN}/{}", relative.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names_the_vault() {
        let vault = tempfile::tempdir().unwrap();
        let root = root_from(Some(vault.path().as_os_str())).unwrap();
        assert_eq!(root, vault.path());
    }

    #[test]
    fn unset_or_empty_env_is_an_error() {
        assert!(root_from(None).is_err());
        assert!(root_from(Some(OsStr::new(""))).is_err());
    }

    #[test]
    fn missing_vault_is_an_error_naming_the_setting() {
        let err = root_from(None).unwrap_err().to_string();
        assert!(err.contains("GAME_ART_VAULT"), "{err}");
        let err = root_from(Some(OsStr::new("/nonexistent/vault")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nonexistent/vault"), "{err}");
    }

    #[test]
    fn relative_env_is_rejected() {
        let err = root_from(Some(OsStr::new("relative-vault")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");
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
    }

    #[test]
    fn token_expansion_propagates_a_missing_vault() {
        let err =
            expand_token("$GAME_ART_VAULT/demo", || anyhow::bail!("vault not found")).unwrap_err();
        assert!(err.to_string().contains("vault not found"));
    }

    #[test]
    fn paths_under_the_vault_are_tokenized() {
        let root = Path::new("/vault");
        assert_eq!(
            tokenize(Path::new("/vault/blender-assets/kit/a.glb"), root).as_deref(),
            Some("$GAME_ART_VAULT/blender-assets/kit/a.glb")
        );
        assert_eq!(
            tokenize(Path::new("/vault"), root).as_deref(),
            Some("$GAME_ART_VAULT")
        );
        assert_eq!(tokenize(Path::new("/vault2/a.glb"), root), None);
        assert_eq!(tokenize(Path::new("/tmp/a.glb"), root), None);
    }
}
