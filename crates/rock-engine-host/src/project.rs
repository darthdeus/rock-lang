//! Stone project recognition and live-session layout.
use anyhow::Result;
use rock_host::project::MAIN_LUA_FILENAME;
use std::path::{Path, PathBuf};

pub fn is_game_project(path: &Path) -> Result<bool> {
    if configured_stone_dir(path)?.is_some() {
        return Ok(true);
    }
    Ok(path.join("main_stone.rock").exists() || path.join("stone_ffi.rock").exists())
}

/// Root for `rock stone` session output. Everything else -- `run`, `frame`,
/// `perf`, the game tests -- writes `tmp/main.lua`, and the running game
/// reloads whatever lands at the path it was launched from, so a live session
/// keeps its build out of that shared slot
/// so concurrent one-shot builds cannot replace a running session.
pub const STONE_SESSION_DIR: &str = "tmp/stone";

/// One directory per (game, pid); `rock stone` prunes inactive sessions after seven days.
pub fn stone_session_main_lua_path(game: &str, pid: u32) -> PathBuf {
    let name = Path::new(game)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "game".to_string());
    PathBuf::from(STONE_SESSION_DIR)
        .join(format!("{name}-{pid}"))
        .join(MAIN_LUA_FILENAME)
}

pub fn configured_stone_dir(path: &Path) -> Result<Option<PathBuf>> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some((config_path, config)) = rock_host::stick::StickConfig::find_from_dir(&path)? else {
        return Ok(None);
    };
    let manifest_dir = config_path.parent().unwrap_or(&config_path);
    let channel = rock_host::stick::resolved_channel_for(&config);
    let graph =
        rock_host::stick::graph::PackageGraph::build_with_channel(manifest_dir, &config, &channel)?;
    Ok(graph
        .packages()
        .iter()
        .find(|package| package.manifest_name == "stone")
        .map(|package| package.root.clone()))
}

const PROJECT_PARENTS: &[&str] = &["games"];

/// Tries the path relative to cwd first, then `PROJECT_PARENTS` under `rock_dir()`.
/// Zero or several matches reject with the candidates listed; never guess.
pub fn resolve_project_dir(arg: &str) -> Result<PathBuf> {
    let local = resolve_project_dir_in(Path::new(""), PROJECT_PARENTS, arg);
    if local.is_err() {
        let root = rock_host::rock_dir();
        let root = Path::new(&root);
        if root != Path::new("")
            && root != Path::new(".")
            && let Ok(dir) = resolve_project_dir_in(root, PROJECT_PARENTS, arg)
        {
            return Ok(dir);
        }
    }
    local
}

/// A literal directory wins over expansion; only a bare name expands against the parents.
fn resolve_project_dir_in(base: &Path, parents: &[&str], arg: &str) -> Result<PathBuf> {
    let literal = base.join(arg);
    if literal.is_dir() {
        return Ok(literal);
    }

    let candidates: Vec<PathBuf> = parents
        .iter()
        .map(|p| base.join(p).join(arg))
        .filter(|c| c.is_dir())
        .collect();

    match candidates.len() {
        0 => Err(anyhow::anyhow!(
            "no project `{arg}`: not a directory, and no match for {}",
            parents
                .iter()
                .map(|p| format!("`{p}/{arg}`"))
                .collect::<Vec<_>>()
                .join(" or ")
        )),
        1 => Ok(candidates.into_iter().next().unwrap()),
        _ => Err(anyhow::anyhow!(
            "ambiguous project `{arg}`: matches {} -- pass an explicit path",
            candidates
                .iter()
                .map(|c| format!("`{}`", c.display()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn source_workspace_root(project: &Path) -> Option<PathBuf> {
    rock_host::project::find_source_root(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_err as fs;
    use tempfile::TempDir;
    #[test]
    fn game_recognition_uses_resolved_stone_identity() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (directory, manifest) in [
            ("engine", "[package]\nname = \"stone\"\ntype = \"lib\"\n"),
            (
                "wrapper",
                "[package]\nname = \"wrapper\"\ntype = \"lib\"\n[dependencies]\nrenamed = { path = \"../engine\" }\n",
            ),
            (
                "game",
                "[package]\nname = \"game\"\n[dependencies]\nwrapper = { path = \"../wrapper\" }\n",
            ),
            ("plain", "[package]\nname = \"plain\"\n"),
        ] {
            let path = root.join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("stick.toml"), manifest).unwrap();
        }
        for directory in ["engine", "wrapper", "game"] {
            let project = root.join(directory);
            assert_eq!(
                configured_stone_dir(&project).unwrap(),
                Some(root.join("engine"))
            );
            assert!(
                is_game_project(&project).unwrap(),
                "{directory} lost its resolved Stone runtime"
            );
        }
        assert!(!is_game_project(&root.join("plain")).unwrap());
    }

    #[test]
    fn resolve_project_dir_prefers_the_literal_path() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        for dir in ["sample-game", "games/sample-game", "games/puzzle"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (arg, want) in [
            ("sample-game", "sample-game"),
            ("puzzle", "games/puzzle"),
            (".", "."),
            ("games/puzzle", "games/puzzle"),
        ] {
            let got = resolve_project_dir_in(root, &["games"], arg).unwrap();
            assert_eq!(got, root.join(want), "{arg}");
        }
    }

    #[test]
    fn resolve_project_dir_ambiguous_rejects_with_candidates() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("games/dup")).unwrap();
        fs::create_dir_all(root.join("demos/dup")).unwrap();

        let err = resolve_project_dir_in(root, &["games", "demos"], "dup")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous project `dup`"), "unexpected: {err}");
        assert!(
            err.contains(&root.join("games").join("dup").display().to_string())
                && err.contains(&root.join("demos").join("dup").display().to_string()),
            "should list both candidates: {err}"
        );
    }
}
