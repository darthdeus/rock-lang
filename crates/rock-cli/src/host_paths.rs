use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const WORK_ROOTS: &str = include_str!("host-work-roots.json");

fn work_root(host: &str, home: &Path) -> Result<PathBuf> {
    let roots: std::collections::HashMap<String, PathBuf> = serde_json::from_str(WORK_ROOTS)
        .context("invalid host work-root policy")?;
    Ok(roots.get(host).cloned().unwrap_or_else(|| home.to_path_buf()))
}

pub(crate) fn local_work_root(home: &Path) -> Result<PathBuf> {
    let mut host = std::env::var("ROCK_JOBS_SELF").unwrap_or_default();
    if cfg!(target_os = "macos") && host.is_empty() {
        let output = std::process::Command::new("scutil").args(["--get", "LocalHostName"])
            .output().context("read macOS LocalHostName for work storage")?;
        anyhow::ensure!(output.status.success(), "scutil LocalHostName: {}", String::from_utf8_lossy(&output.stderr));
        if String::from_utf8_lossy(&output.stdout).trim().eq_ignore_ascii_case("jakubs-host-c") {
            host = "studio".into();
        }
    }
    let root = work_root(&host, home)?;
    // Refuse a missing volume rather than filling the small system disk below its mountpoint.
    if root != home {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let volume = std::fs::metadata(&root).with_context(|| format!("work volume {} unavailable", root.display()))?;
            let parent = std::fs::metadata(root.parent().context("work volume has no parent")?)?;
            anyhow::ensure!(volume.dev() != parent.dev(), "work volume {} is not mounted", root.display());
        }
    }
    Ok(root)
}

#[cfg(test)]
pub(crate) fn assert_cache_default(test: &str, cache: &str, root: impl Fn() -> Result<Option<PathBuf>>) {
    if let Ok(expected) = std::env::var("ROCK_STORAGE_TEST_EXPECTED") {
        assert_eq!(root().unwrap().unwrap(), PathBuf::from(expected));
        return;
    }
    let home = tempfile::tempdir().unwrap();
    for host in ["host-a", "studio"] {
        // The Studio case needs its actual mounted volume; other hosts still test HOME and overrides.
        if host == "studio" && !cfg!(target_os = "macos") { continue; }
        let base = if host == "studio" { Path::new("/Volumes/Data") } else { home.path() };
        let expected = base.join("tmp").join(cache);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("HOME", home.path()).env("ROCK_JOBS_SELF", host)
            .env("ROCK_STORAGE_TEST_EXPECTED", expected)
            .env_remove("ROCK_FRAME_COMPILE_CACHE").env_remove("ROCK_FRAME_COMPILE_CACHE_DIR")
            .env_remove("ROCK_SIM_COMPILE_CACHE").env_remove("ROCK_SIM_COMPILE_CACHE_DIR")
            .env_remove("SKULL_ALLOCATION_REPORT").env_remove("SKULL_ALLOCATION_ACTIVATION")
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_storage_policy() {
        let home = Path::new("/author");
        assert_eq!(work_root("studio", home).unwrap(), Path::new("/Volumes/Data"));
        for host in ["host-a", "host-b", "m1", "unknown"] {
            assert_eq!(work_root(host, home).unwrap(), home);
        }
    }
}
