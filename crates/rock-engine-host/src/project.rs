//! Stone project recognition and live-session layout.
use anyhow::Result;
use rock_host::project::MAIN_LUA_FILENAME;
use std::path::{Path, PathBuf};

/// Use the same resolved Stone identity for game setup and engine resources.
/// Marker files retain support for manifest-free projects.
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

/// This session's private `main.lua`, one directory per (game, pid).
/// `rock stone` prunes inactive sessions after seven days.
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

pub fn configured_stone_dir(path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some((config_path, _config)) = rock_host::stick::StickConfig::find_from_dir(&path)? else {
        return Ok(None);
    };
    let manifest_dir = config_path.parent().unwrap_or(&config_path);
    let graph = rock_host::stick::graph::PackageGraph::build(manifest_dir)?;
    let root = graph.root();
    // Stone testing itself: the root IS the stone package, so resolve its own
    // directory instead of reporting no Stone root (which fails global init).
    if graph.package(root).manifest_name == "stone" {
        return Ok(Some(graph.package(root).root.clone()));
    }
    Ok(graph
        .packages()
        .iter()
        .find(|package| package.id != root && package.manifest_name == "stone")
        .map(|package| package.root.clone()))
}

/// Parents a bare project name expands against (relative to the workspace
/// root), beyond the literal path. `games/` holds the match3-class games;
/// top-level projects (sample-game, demo) are picked up by the literal-path
/// check, so they need no entry here.
pub const PROJECT_PARENTS: &[&str] = &["games"];

/// Resolve a project argument (`rock sim`/`--list`) to a directory. Checks
/// relative to current directory first (allowing execution from game subdirectories);
/// if not found in CWD, expands against `PROJECT_PARENTS` relative to `rock_dir()`.
/// Zero or more-than-one matches reject with the candidates listed -- never guess.
pub fn resolve_project_dir(arg: &str) -> Result<PathBuf> {
    if let Ok(dir) = resolve_project_dir_in(Path::new(""), PROJECT_PARENTS, arg) {
        return Ok(dir);
    }
    let rock_root = PathBuf::from(rock_host::rock_dir());
    if rock_root != Path::new("") && rock_root != Path::new(".") {
        if let Ok(dir) = resolve_project_dir_in(&rock_root, PROJECT_PARENTS, arg) {
            return Ok(dir);
        }
    }
    resolve_project_dir_in(Path::new(""), PROJECT_PARENTS, arg)
}

/// Core of `resolve_project_dir`, parameterized on the search base and parent
/// list so it can be exercised without touching the process cwd. An empty
/// `base` yields clean cwd-relative paths (`Path::new("").join("x")` == `"x"`).
pub fn resolve_project_dir_in(base: &Path, parents: &[&str], arg: &str) -> Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use fs_err as fs;
    use tempfile::TempDir;
    #[test]
    fn game_recognition_uses_resolved_stone_identity() {
        let tmp = TempDir::new().unwrap();
        // Discovery returns canonical paths; macOS temp dirs sit behind a /var symlink.
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
        for directory in ["wrapper", "game"] {
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
    fn resolve_project_dir_literal_path_wins() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        // a top-level project dir (sample-game/demo-shaped) AND a games/ dir of
        // the same name -- the literal path must win over expansion.
        fs::create_dir_all(root.join("sample-game")).unwrap();
        fs::create_dir_all(root.join("games/sample-game")).unwrap();

        let got = resolve_project_dir_in(root, &["games"], "sample-game").unwrap();
        assert_eq!(got, root.join("sample-game"));
    }

    #[test]
    fn resolve_project_dir_bare_name_expands_to_games() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("games/match3")).unwrap();

        let got = resolve_project_dir_in(root, &["games"], "match3").unwrap();
        assert_eq!(got, root.join("games/match3"));
    }

    #[test]
    fn resolve_project_dir_dot_stays_current() {
        // The no-arg default (`.`) is a literal directory that always wins,
        // so it means "current directory" and is never bare-name-expanded --
        // even when a `games/.` would technically exist.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("games")).unwrap();

        let got = resolve_project_dir_in(root, &["games"], ".").unwrap();
        assert_eq!(got, root.join("."));
    }

    #[test]
    fn resolve_project_dir_explicit_subpath_passthrough() {
        // A literal path a caller typed (with a separator) passes through
        // unchanged -- the resolver only expands BARE names.
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("games/match3")).unwrap();

        let got = resolve_project_dir_in(root, &["games"], "games/match3").unwrap();
        assert_eq!(got, root.join("games/match3"));
    }

    #[test]
    fn resolve_project_dir_missing_rejects() {
        let temp = TempDir::new().unwrap();
        let err = resolve_project_dir_in(temp.path(), &["games"], "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no project `nope`"), "unexpected: {err}");
        assert!(
            err.contains("games/nope"),
            "should name the searched parent: {err}"
        );
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

// Source-owned tools and sim labels follow the game tree, not an installed SDK.
pub fn source_workspace_root(project: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("ROCK_CONTEXT_ROOT").map(std::path::PathBuf::from) {
        if root.is_dir()
            && root.join("sdk").is_dir()
            && root.join(".context/context.json").is_file()
        {
            return Some(root);
        }
    }
    project
        .ancestors()
        .find(|root| root.join("Cargo.toml").exists() && root.join("crates").is_dir())
        .map(std::path::Path::to_path_buf)
}
