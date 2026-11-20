//! Random helper-body revisions through the production incremental compiler:
//! the baseline plus its last overlay must behave like a fresh build of the
//! final source.

use super::generator::revised;
use rock_cli::skull_backend::{IncrementalProject, SkullReload};
use std::path::PathBuf;
use std::time::Duration;

const HELPERS: usize = 4;
const EDITS: u64 = 4;

struct Build {
    _dir: tempfile::TempDir,
    main: PathBuf,
    incremental: PathBuf,
    source: PathBuf,
}

fn boot(text: &str) -> Result<(Build, IncrementalProject), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = |name| dir.path().join(name);
    let (main, incremental, source) =
        (path("main.lua"), path("incremental.lua"), path("main.rock"));
    std::fs::write(&source, text).map_err(|e| e.to_string())?;
    let root = dir.path().to_str().expect("utf-8 tempdir");
    let project = IncrementalProject::init(root, &main, &incremental)
        .map_err(|e| format!("initial build failed: {e:#}"))?;
    let build = Build {
        _dir: dir,
        main,
        incremental,
        source,
    };
    Ok((build, project))
}

/// `main` runs once, after the overlay has replaced the edited bodies, instead
/// of when `main.lua` loads.
fn run(build: &Build) -> Result<String, String> {
    let main = std::fs::read_to_string(&build.main).map_err(|e| e.to_string())?;
    let main = main.replacen("\n__rock_main_wrapper(main)\n", "\n", 1);
    let overlay = std::fs::read_to_string(&build.incremental).unwrap_or_default();
    let program = format!("function clear_all_systems() end\n{main}\n{overlay}\nmain()\n");
    let run =
        rock_host::lua_runner::run_lua_code_with_timeout(&program, Some(Duration::from_secs(30)))
            .map_err(|e| e.message)?;
    if run.success {
        Ok(run.stdout)
    } else {
        Err(run.combined())
    }
}

fn route(reload: &SkullReload) -> &'static str {
    match reload {
        SkullReload::Full { .. } => "full",
        SkullReload::Incremental { .. } => "overlay",
        SkullReload::FullRequired { .. } => "full-required",
        SkullReload::ShapeChanged { .. } => "shape-changed",
        SkullReload::NoChange => "no-change",
    }
}

/// The edited helper and the revisions after each edit.
pub(super) fn steps(seed: u64) -> Vec<(usize, [u64; HELPERS])> {
    let mut revisions = [0; HELPERS];
    let mut random = seed;
    (1..=EDITS)
        .map(|step| {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let helper = (random >> 33) as usize % HELPERS;
            revisions[helper] = step;
            (helper, revisions)
        })
        .collect()
}

pub(super) fn check(seed: u64) -> Result<(), String> {
    let (mut live, mut project) = boot(&revised(seed, &[]))?;
    let mut routes = Vec::new();
    let mut revisions = [0; HELPERS];
    for (helper, after) in steps(seed) {
        revisions = after;
        let text = revised(seed, &revisions);
        match project.apply_edit(&live.source, &text, &live.main, &live.incremental) {
            Ok(reload) => routes.push(format!("helper{helper}:{}", route(&reload))),
            // A refused edit is what a user resolves by restarting.
            Err(e) if format!("{e:#}").contains("requires a safe restart") => {
                routes.push(format!("helper{helper}:restart"));
                (live, project) = boot(&text)?;
            }
            Err(e) => {
                return Err(format!(
                    "edits {routes:?}, then helper{helper} failed: {e:#}"
                ));
            }
        }
    }
    let (fresh, _) = boot(&revised(seed, &revisions))?;
    let expected = run(&fresh).map_err(|e| format!("fresh build failed to run: {e}"))?;
    let actual = run(&live).map_err(|e| format!("overlay failed to run ({routes:?}): {e}"))?;
    if let Some(output) = std::env::var_os("ROCK_DIFF_OUTPUT") {
        let dir = std::path::Path::new(&output).join(format!("reload-{seed}"));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, build) in [("fresh", &fresh), ("live", &live)] {
            for (suffix, path) in [
                ("main.lua", &build.main),
                ("incremental.lua", &build.incremental),
            ] {
                let _ = std::fs::copy(path, dir.join(format!("{name}-{suffix}")));
            }
        }
        std::fs::write(dir.join("fresh.txt"), &expected).unwrap();
        std::fs::write(dir.join("live.txt"), &actual).unwrap();
        std::fs::write(dir.join("first.rock"), revised(seed, &[])).unwrap();
        std::fs::write(dir.join("last.rock"), revised(seed, &revisions)).unwrap();
    }
    if let Some((line, (want, got))) = expected
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        return Err(format!(
            "edits {routes:?}: trace line {}: fresh {want:?}, overlay {got:?}",
            line + 1
        ));
    }
    if expected.lines().count() != actual.lines().count() {
        return Err(format!("edits {routes:?}: trace lengths differ"));
    }
    Ok(())
}
