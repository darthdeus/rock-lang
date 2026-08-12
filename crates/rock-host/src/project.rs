use std::path::{Path, PathBuf};
use anyhow::Result;

/// Check if a directory looks like the Rock workspace root.
///
/// Recognizes two layouts:
/// - Dev workspace: has Cargo.toml + prebuilt/
/// - Release layout: has bin/rock + lib/ + stone/
fn is_rock_root(path: &Path) -> bool {
    // Dev workspace
    if path.join("Cargo.toml").exists() && path.join("prebuilt").is_dir() {
        return true;
    }
    // Release layout
    if path.join("lib").is_dir() && path.join("stone").is_dir() && path.join("bin").is_dir() {
        return true;
    }
    false
}

/// Find the Rock workspace root by looking for known layout markers.
/// This identifies the top-level Rock development workspace (which contains target/,
/// prebuilt/, tmp/, etc.) or a release installation (which contains bin/, lib/, stone/).
///
/// Walks up from the given path. Returns None if no workspace root is found.
pub fn find_workspace_root(path: &Path) -> Option<PathBuf> {
    let initial_path = fs_err::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf());

    let mut current = if initial_path.is_file() {
        initial_path.parent()?.to_path_buf()
    } else {
        initial_path
    };

    loop {
        if is_rock_root(&current) {
            return Some(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return None,
        }
    }
}

/// If the current executable is `<root>/bin/rock` inside a release layout,
/// return that root. The binary knows where its own install directory is,
/// which is more authoritative than walking up CWD — a user running a
/// release zip from inside another Rock workspace should still use the
/// release zip's stone/ and lib/, not the outer dev workspace.
fn release_root_from_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin_dir = exe.parent()?;
    let root = bin_dir.parent()?;
    if is_rock_root(root) && !root.join("Cargo.toml").exists() {
        Some(root.to_path_buf())
    } else {
        None
    }
}

/// Ensure the process is running from the Rock workspace root.
///
/// Resolution order:
///   1. Executable-based release layout (authoritative for release installs).
///   2. CWD is already a rock root.
///   3. Walk up from CWD to find one.
pub fn ensure_workspace_root() -> Result<()> {
    if let Some(root) = release_root_from_exe() {
        std::env::set_current_dir(&root)?;
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    if is_rock_root(&cwd) {
        return Ok(());
    }
    if let Some(root) = find_workspace_root(&cwd) {
        std::env::set_current_dir(&root)?;
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "Could not find Rock root directory.\n\
         Run this command from within the Rock workspace, or set the ROCK_DIR environment variable."
    ))
}

/// Find project directory by looking for stick.toml or other project markers
/// This walks up the directory tree from the given path until it finds a project root
pub fn find_project_root(path: &Path) -> Option<PathBuf> {
    // Convert to absolute path first to handle relative paths correctly
    let initial_path = fs_err::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf());

    // Start from the directory containing the path (or the path itself if it's a directory)
    let mut current_path = if initial_path.is_file() || initial_path.extension().is_some() {
        // If it's a file or has an extension, start from parent
        initial_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| initial_path.clone())
    } else {
        initial_path
    };

    // Project markers in order of preference
    // Only look for stick.toml - don't treat Cargo.toml or .git as Rock project roots
    let markers = ["stick.toml"];

    loop {
        // Check for each marker in order
        for marker in &markers {
            let marker_path = current_path.join(marker);
            if marker_path.exists() {
                return Some(current_path);
            }
        }

        // Move to parent directory
        match current_path.parent() {
            Some(parent) => current_path = parent.to_path_buf(),
            None => return None, // Reached filesystem root
        }
    }
}

/// Generate a deterministic identifier for a project based on its canonical path
/// This is used for creating unique socket names, cache directories, etc.
pub fn get_project_id(project_root: &Path) -> Result<String> {
    use std::hash::{Hash, Hasher};

    let canonical = project_root.canonicalize()?;
    let mut hasher = rustc_hash::FxHasher::default();
    canonical.to_string_lossy().as_bytes().hash(&mut hasher);
    // 32 bits of hash, 8 hex chars — parity with the prior SHA-256 truncation.
    // Non-cryptographic; only needs collision resistance over local project paths.
    Ok(format!("{:08x}", hasher.finish() as u32))
}

/// Returns true when the rock_dir looks like an extracted release zip
/// (bin/ + lib/ + stone/) rather than a dev workspace (Cargo.toml + prebuilt/).
///
/// In release layout the native libraries are already staged under lib/, so
/// commands like `rock run` must not shell out to cargo or copy from
/// target/debug/ — there is no target dir.
pub fn is_release_layout() -> bool {
    let root = PathBuf::from(crate::rock_dir());
    root.join("lib").is_dir() && root.join("bin").is_dir() && root.join("stone").is_dir()
        && !root.join("Cargo.toml").exists()
}

/// Get the path to the LuaJIT binary.
///
/// In release layout: `<rock_dir>/bin/luajit`
/// In dev layout: `<rock_dir>/prebuilt/gamelibs-<platform>/bin/luajit`
pub fn luajit_path() -> PathBuf {
    let root = PathBuf::from(crate::rock_dir());

    // Release layout: bin/luajit next to bin/rock
    let release_path = if cfg!(target_os = "windows") {
        root.join("bin").join("luajit.exe")
    } else {
        root.join("bin").join("luajit")
    };
    if release_path.exists() {
        return release_path;
    }

    // Dev layout: prebuilt/gamelibs-<platform>/bin/luajit
    if cfg!(target_os = "macos") {
        // Check for ARM first, then x64
        let arm = root.join("prebuilt/gamelibs-macos-arm64/bin/luajit");
        if arm.exists() { arm } else { root.join("prebuilt/gamelibs-macos-x64/bin/luajit") }
    } else if cfg!(target_os = "windows") {
        root.join("prebuilt/gamelibs-windows-x86_64/bin/luajit.exe")
    } else {
        root.join("prebuilt/gamelibs-linux-x86_64/bin/luajit")
    }
}

/// Get the library search path value for the current platform.
///
/// In release layout: `<rock_dir>/lib`
/// In dev layout: `tmp` (macOS), `tmp:prebuilt/gamelibs-<platform>/lib` (Linux),
///                `tmp;prebuilt\gamelibs-<platform>\bin` (Windows)
pub fn lib_path_value() -> String {
    let root = PathBuf::from(crate::rock_dir());

    // Release layout: lib/ directory exists at root
    let release_lib = root.join("lib");
    if release_lib.exists() {
        let lib_str = release_lib.to_string_lossy().into_owned();
        if cfg!(target_os = "windows") {
            // Windows also needs bin/ on PATH for DLLs next to executables
            let bin_str = root.join("bin").to_string_lossy().into_owned();
            return format!("tmp;{};{}", lib_str, bin_str);
        } else {
            return format!("tmp:{}", lib_str);
        }
    }

    // Dev layout: relative paths from CWD (which is the workspace root).
    // The prebuilt lib dir MUST be on the search path: stone's Rock global
    // initializer does ffi.load("libcimgui_complete.{dylib,so,dll}") before
    // stone_init has had a chance to dlopen anything via rpath.
    // Without prebuilt on the path, dyld/dl_open cannot find the lib and
    // the prelude silently falls back to a no-op imgui stub — symptom:
    // every imgui.Begin call succeeds but no window ever renders.
    if cfg!(target_os = "macos") {
        // ARM first, x64 fallback (matches luajit_path resolution above).
        let arm_lib = root.join("prebuilt/gamelibs-macos-arm64/lib");
        let chosen = if arm_lib.exists() {
            arm_lib
        } else {
            root.join("prebuilt/gamelibs-macos-x64/lib")
        };
        format!("tmp:{}", chosen.to_string_lossy())
    } else if cfg!(target_os = "windows") {
        "tmp;prebuilt\\gamelibs-windows-x86_64\\bin".to_string()
    } else {
        "tmp:prebuilt/gamelibs-linux-x86_64/lib".to_string()
    }
}

/// `lib_path_value()` with the caller's inherited search path appended.
///
/// Rock's own dirs (`tmp`, prebuilt gamelibs) come FIRST so the bundled
/// luajit/cimgui libs always win; the inherited value follows so a
/// user-provided `LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`/`PATH` still
/// resolves transitive system deps the prebuilt FHS libs need but the
/// gamelibs dir doesn't ship. On NixOS the prebuilt `libmpv.so` pulls
/// libjack/sndio/gnutls/libdrm/libxcb, which live in nix-store dirs the
/// user puts on the path (e.g. via `programs.nix-ld.libraries` ->
/// `$NIX_LD_LIBRARY_PATH`); clobbering instead of appending dropped them
/// and broke `libstone_video` load.
pub fn lib_path_with_inherited() -> String {
    let mut value = lib_path_value();
    let sep = if cfg!(target_os = "windows") { ";" } else { ":" };
    if let Some(inherited) = std::env::var_os(lib_path_var()) {
        if !inherited.is_empty() {
            value.push_str(sep);
            value.push_str(&inherited.to_string_lossy());
        }
    }
    value
}

/// Get the environment variable name for the library search path.
pub fn lib_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// Get the LUA_PATH value for requiring Lua modules.
///
/// In release layout: `<rock_dir>/prebuilt/lua/?.lua;<rock_dir>/prebuilt/lua/?/init.lua;`
/// In dev layout: `<rock_dir>/?.lua;`
pub fn lua_path_value() -> String {
    let root = crate::rock_dir();
    // Legacy search path. cimgui reaches Lua through the embedded prelude's
    // package.preload entries, not this path; kept because it is generic
    // (any module dropped under prebuilt/lua/ resolves through it).
    let mut p =
        format!("{root}/prebuilt/lua/?.lua;{root}/prebuilt/lua/?/init.lua;{root}/?.lua;");

    // LuaJIT's own jit/ toolkit (jit.v, jit.dump, jit.vmdef, dis_*) so
    // `-jv` / `-jdump` resolve when running the bundled luajit. It
    // lives in `<prefix>/share/luajit-2.1/` next to `<prefix>/bin/
    // luajit` in every layout. vmdef.lua is generated per-build and
    // MUST match this exact binary, so derive it from luajit_path()
    // rather than guessing — cross-pairing builds gives a "core/
    // library version mismatch". Harmless on normal runs (jit.* is
    // only loaded on demand by a `-j` flag).
    if let Some(prefix) = luajit_path().parent().and_then(|d| d.parent()) {
        let share = prefix.join("share").join("luajit-2.1");
        if share.is_dir() {
            p.push_str(&format!("{}/?.lua;", share.to_string_lossy()));
        }
    }
    p
}

/// Filename of the assembled Lua output inside an output directory.
pub const MAIN_LUA_FILENAME: &str = "main.lua";

/// Default output directory for compiled games (and most things).
pub const DEFAULT_OUTPUT_DIR: &str = "tmp";

/// The canonical default location for the compiled program:
/// `tmp/main.lua`. Used as both the compile target and the file
/// luajit is exec'd on by `rock run` / `rock stone` / `rock frame`.
pub fn default_main_lua_path() -> PathBuf {
    PathBuf::from(DEFAULT_OUTPUT_DIR).join(MAIN_LUA_FILENAME)
}

/// Root for `rock stone` session output. Everything else -- `run`, `frame`,
/// `perf`, the game tests -- writes `tmp/main.lua`, and the running game
/// reloads whatever lands at the path it was launched from, so a live session
/// keeps its build out of that shared slot
/// (`docs/hot-reload-chunk-identity.md`).
pub const STONE_SESSION_DIR: &str = "tmp/stone";

/// This session's private `main.lua`, one directory per (game, pid). Nothing
/// sweeps these; `rock stone` warns once the root grows past its budget.
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

/// Write `contents` to `path` atomically: write to a uniquely-named temp
/// file in the same directory, then `rename(2)` into place (atomic on
/// macOS + Linux). A concurrent reader -- a running game polling main.lua
/// for hot reload, or a parallel `rock frame` -- always sees either the
/// complete old or the complete new file, never a truncated mid-write
/// (which the hot-reload poller would `dofile` as an empty program,
/// clearing the system registries and blanking the screen).
///
/// The temp name is PID + a process-global counter, so concurrent
/// compiler instances (e.g. parallel tests) and concurrent writes within
/// one process never collide on the temp path. The single atomic-write
/// path shared by every generated-Lua writer (rock + skull backends).
pub fn write_atomic(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;

    let path = path.as_ref();
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs_err::create_dir_all(parent)?;

    // Temp file in the SAME directory so `persist` is a same-filesystem
    // (atomic) rename. NamedTempFile gives an OS-level unique name -- no
    // collisions between concurrent compiler / test instances -- and is
    // removed on any early return, so a failed write leaves no `.tmp`
    // litter. No fsync: main.lua is a regenerated build artifact, and
    // rename atomicity alone closes the concurrent-reader race.
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(contents.as_ref())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Build a `Command` ready to spawn luajit on `lua_file` with the
/// standard env (`LUA_PATH`, library search path) and forwarded
/// `app_args`. Callers configure stdio + add extra env vars before
/// spawning. Single place where the luajit + lib paths are resolved
/// for game execution -- shared by rock run, rock stone, rock perf,
/// rock frame, rock sim.
pub fn build_luajit_command(lua_file: &Path, app_args: &[String]) -> std::process::Command {
    let luajit_path = luajit_path();
    let mut cmd = std::process::Command::new(&luajit_path);
    // STONE_JV=1 (set directly or via the `--jv` CLI flag) turns on LuaJIT's
    // verbose trace log. `-j*` flags are only honored BEFORE the script name --
    // anything after lua_file is passed to the game as arg[] -- so it leads.
    // jit.v resolves from share/luajit-2.1/ via the LUA_PATH set below.
    if std::env::var("STONE_JV").as_deref() == Ok("1") {
        cmd.arg("-jv");
    }
    cmd.arg(lua_file)
        .env("LUA_PATH", lua_path_value())
        .env(lib_path_var(), lib_path_with_inherited());
    for arg in app_args {
        cmd.arg(arg);
    }
    cmd
}

/// Recursively find every `.rock` file under `source_dir` (absolute paths,
/// `~`-suffixed editor backups skipped). Surfaces per-entry walk errors to
/// stderr rather than silently dropping them — a permission error on stdlib/
/// would otherwise masquerade as "module not found" (silent-errors audit).
pub fn find_rock_files_at_path(source_dir: impl AsRef<Path>) -> Result<Vec<String>> {
    use anyhow::Context;
    use walkdir::WalkDir;

    // Canonicalize the base directory once — walkdir will produce absolute paths
    // from this base, so individual files don't need per-file canonicalize.
    let canonical_base = fs_err::canonicalize(source_dir.as_ref()).with_context(|| {
        format!(
            "Failed to resolve source directory '{}'",
            source_dir.as_ref().display()
        )
    })?;

    let mut files = Vec::new();

    // Sort each directory's entries by name so discovery order is independent of
    // the filesystem's `read_dir` order (which is stable per working-tree state
    // but shifts when files are rewritten -- e.g. a `git checkout` between builds).
    // Module order flows into `ws.modules`, which drives `type_id` numbering and
    // function-chunk emission order, so an unsorted walk makes the emitted `.lua`
    // non-reproducible across builds (unfuck-backlog #58).
    for entry in WalkDir::new(&canonical_base).sort_by_file_name() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "rock: skipping path under `{}` while scanning for .rock files: {e}",
                    canonical_base.display()
                );
                continue;
            }
        };
        let path = entry.path();

        if path.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "rock" && !path.to_string_lossy().ends_with("~") {
                    files.push(path.to_string_lossy().to_string());
                }
            }
        }
    }

    Ok(files)
}

/// Check if a path belongs to a game project (Stone/Lua game).
/// This determines if game setup (asset copying, config file, rock_game.json) is needed.
///
/// A project is a game if its stick.toml declares a `stone` dependency,
/// or it contains a `main_stone.rock` / `stone_ffi.rock` marker file
/// (legacy projects without stick.toml).
pub fn is_game_project(path: &Path) -> Result<bool> {
    // "Game project" controls three things that all assume Stone:
    //   - engine cdylib build (libstone, optionally libstone_video)
    //   - stone's cimgui global initializer in the emitted program
    //   - GAME_SOURCE_DIR injection for asset path resolution
    //
    // The signal is a `stone` dependency in stick.toml, OR a stone_ffi /
    // main_stone marker file for legacy projects without stick.toml.
    //
    // An earlier version treated *any* `target = "lua"` package as a
    // game -- but stone's cimgui initializer tries to `ffi.load("libcimgui_complete")`
    // at the top of every output, which fm and the other Lua-target libs
    // (json, fm-core) don't need and can't satisfy. The stub's stderr
    // splat was the user-visible damage.
    if let Some((_, stick_config)) = crate::stick::StickConfig::find_from_dir(path)? {
        if stick_config.dependencies.contains_key("stone") {
            return Ok(true);
        }
    }

    let main_stone = path.join("main_stone.rock");
    let stone_ffi = path.join("stone_ffi.rock");
    if main_stone.exists() || stone_ffi.exists() {
        return Ok(true);
    }

    Ok(false)
}

/// Parents a bare project name expands against (relative to the workspace
/// root), beyond the literal path. `games/` holds the match3-class games;
/// top-level projects (bitgun, mag, anvil) are picked up by the literal-path
/// check, so they need no entry here.
pub const PROJECT_PARENTS: &[&str] = &["games"];

/// Resolve a project argument (`rock sim`/`--list`) to a directory, relative to
/// the current directory -- callers run `ensure_workspace_root` first, so cwd is
/// the workspace root. An existing path wins as-is; otherwise the bare name is
/// expanded against `PROJECT_PARENTS`. Zero or more-than-one matches reject with
/// the candidates listed -- never guess.
pub fn resolve_project_dir(arg: &str) -> Result<PathBuf> {
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
    fn test_find_project_root_with_stick_toml() {
        let temp = TempDir::new().unwrap();
        // Canonicalize the temp path to resolve symlinks (e.g., /var -> /private/var on macOS)
        let project_dir = temp.path().canonicalize().unwrap();

        // Create stick.toml
        fs::write(project_dir.join("stick.toml"), "[package]\nname = \"test\"").unwrap();

        // Create a subdirectory with a file
        let sub_dir = project_dir.join("src");
        fs::create_dir(&sub_dir).unwrap();
        let file_path = sub_dir.join("main.rock");
        fs::write(&file_path, "fn main() {}").unwrap();

        // Should find project root from file in subdirectory
        assert_eq!(find_project_root(&file_path), Some(project_dir.to_path_buf()));

        // Should find project root from subdirectory
        assert_eq!(find_project_root(&sub_dir), Some(project_dir.to_path_buf()));

        // Should find project root from project directory itself
        assert_eq!(find_project_root(&project_dir), Some(project_dir.to_path_buf()));
    }

    #[test]
    fn test_find_project_root_with_git() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().canonicalize().unwrap();

        // Create .git directory (but no stick.toml)
        fs::create_dir(project_dir.join(".git")).unwrap();

        let file_path = project_dir.join("main.rock");
        fs::write(&file_path, "fn main() {}").unwrap();

        // .git alone is NOT a project marker (only stick.toml is), so should return None
        assert_eq!(find_project_root(&file_path), None);
    }

    #[test]
    fn test_find_project_root_none() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("main.rock");
        fs::write(&file_path, "fn main() {}").unwrap();
        
        // No project markers, should return None
        assert_eq!(find_project_root(&file_path), None);
    }

    #[test]
    fn test_project_id_deterministic() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path();

        let id1 = get_project_id(project_dir).unwrap();
        let id2 = get_project_id(project_dir).unwrap();

        // Should generate the same ID for the same path
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 8);
    }

    #[test]
    fn resolve_project_dir_literal_path_wins() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        // a top-level project dir (bitgun/mag/anvil-shaped) AND a games/ dir of
        // the same name -- the literal path must win over expansion.
        fs::create_dir_all(root.join("bitgun")).unwrap();
        fs::create_dir_all(root.join("games/bitgun")).unwrap();

        let got = resolve_project_dir_in(root, &["games"], "bitgun").unwrap();
        assert_eq!(got, root.join("bitgun"));
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
        assert!(err.contains("games/nope"), "should name the searched parent: {err}");
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
        assert!(err.contains("games/dup") && err.contains("demos/dup"),
            "should list both candidates: {err}");
    }

    #[test]
    fn stone_jv_env_inserts_jv_before_script() {
        use std::ffi::OsStr;
        let lua = std::path::Path::new("tmp/main.lua");

        // SAFETY: nextest runs each test in its own process, so mutating this
        // process-global env var can't race a sibling test. Restored below.
        let prev = std::env::var_os("STONE_JV");

        unsafe { std::env::set_var("STONE_JV", "1") };
        let cmd = build_luajit_command(lua, &[]);
        let args: Vec<_> = cmd.get_args().map(OsStr::to_os_string).collect();
        let jv = args.iter().position(|a| a == "-jv");
        let script = args.iter().position(|a| a == OsStr::new("tmp/main.lua"));
        assert!(jv.is_some(), "STONE_JV=1 should insert -jv; args={args:?}");
        assert!(script.is_some(), "script arg present; args={args:?}");
        // luajit only honors -j* flags BEFORE the script name.
        assert!(jv.unwrap() < script.unwrap(), "-jv must precede the script; args={args:?}");

        unsafe { std::env::set_var("STONE_JV", "0") };
        let cmd0 = build_luajit_command(lua, &[]);
        assert!(!cmd0.get_args().any(|a| a == "-jv"), "STONE_JV=0 must not insert -jv");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("STONE_JV", v),
                None => std::env::remove_var("STONE_JV"),
            }
        }
    }
}
