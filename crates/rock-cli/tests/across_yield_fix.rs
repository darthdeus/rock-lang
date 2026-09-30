//! `rock fix --across-yield` (docs/identity-storage-remaining-sites.md, the
//! recipe in docs/identity-storage-plan.md "Sim rewrite shards"), driven
//! through the real `rock` binary on a temp-dir package shaped like MAG's
//! world entry.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rock_across_yield_fix_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const WORLD: &str = r#"pub struct Player {
    pub hp: int,
}

pub struct Mob {
    pub hp: int,
}

pub struct WorldState {
    pub player: Entity<Player>,
    pub mobs: Arena<Mob>,
}

pub global MAG_ACTIVE_WORLD: Scene<WorldState>? = null
pub global BOSS: Entity<Mob> = Entity { _scene_id: 0, _arena_id: 0, _slot: 0, _generation: 0 }
pub global PREDS: [() -> bool] = []

pub fn player(world: Scene<WorldState>) -> Player {
    return world.get().unwrap().player.get().unwrap()
}

pub fn mag_world_state() -> WorldState {
    return MAG_ACTIVE_WORLD.unwrap().get().unwrap()
}

pub fn spawn_mob(hp: int) -> Entity<Mob> {
    return mag_world_state().mobs.insert(Mob { hp: hp })
}

pub fn find_boss() -> Mob {
    return BOSS.get().unwrap()
}

pub fn mag_set_active_world(world: Scene<WorldState>) {
    MAG_ACTIVE_WORLD = world
}

pub co fn frame(n: int) -> Coroutine<int> {
    yield n
}

pub fn wait_for(pred: () -> bool) -> int {
    PREDS.push(pred)
    return 0
}

fn main() {
}
"#;

/// A package `game` with the world stand-ins and one file per sim.
fn package(tag: &str, sims: &[(&str, &str)]) -> PathBuf {
    let dir = scratch(tag);
    write(
        &dir.join("game/stick.toml"),
        "[package]\nname = \"game\"\nversion = \"0.1.0\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
    );
    write(&dir.join("game/main.rock"), WORLD);
    for (name, body) in sims {
        write(&dir.join(format!("game/{name}.rock")), body);
    }
    dir.join("game")
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn fix(root: &Path, files: &[&str], apply: bool) -> Run {
    let mut args = vec!["fix".to_string(), "--across-yield".to_string()];
    args.extend(files.iter().map(|f| root.join(format!("{f}.rock")).display().to_string()));
    if apply {
        args.push("--apply".to_string());
    }
    let out = Command::new(env!("CARGO_BIN_EXE_rock")).current_dir(root).args(&args).output().expect("spawn rock");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    }
}

fn shown(run: &Run) -> String {
    format!("exit {:?}\nstdout:\n{}\nstderr:\n{}", run.code, run.stdout, run.stderr)
}

fn applied(tag: &str, sim: &str) -> (String, Run) {
    let root = package(tag, &[("sim", sim)]);
    let run = fix(&root, &["sim"], true);
    (read(&root.join("sim.rock")), run)
}

/// Entry parameter, the active-world chain, and a rebinding after the await
/// that the borrow crosses.
#[test]
fn simple_await_rebinds_the_player() {
    let (out, run) = applied(
        "simple",
        "@sim\nco fn simple() -> Coroutine<int> {\n    let player = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    player.hp = 3\n    await frame(1)\n    player.hp = 4\n}\n",
    );
    assert_eq!(
        out,
        "@sim\nco fn simple(world: Scene<WorldState>) -> Coroutine<int> {\n    let player = world.player()\n    player.hp = 3\n    await frame(1)\n    player = world.player()\n    player.hp = 4\n}\n",
        "{}",
        shown(&run)
    );
    assert!(run.stdout.contains("across-yield 1 -> 0"), "{}", shown(&run));
}

/// A borrow bound before a loop whose body awaits reaches the next iteration
/// through the back edge: the rebinding goes after the await inside the loop,
/// which also covers the use after the loop.
#[test]
fn loop_back_edge_rebinds_inside_the_loop() {
    let (out, run) = applied(
        "loop",
        "@sim\nco fn looped() -> Coroutine<int> {\n    let e = spawn_mob(5)\n    let m = e.get().unwrap()\n    let i = 0\n    while i < 3 {\n        m.hp = i\n        await frame(1)\n        i = i + 1\n    }\n    m.hp = 9\n}\n",
    );
    assert_eq!(
        out,
        "@sim\nco fn looped(world: Scene<WorldState>) -> Coroutine<int> {\n    let e = spawn_mob(5)\n    let m = e.get().unwrap()\n    let i = 0\n    while i < 3 {\n        m.hp = i\n        await frame(1)\n        m = e.get().unwrap()\n        i = i + 1\n    }\n    m.hp = 9\n}\n",
        "{}",
        shown(&run)
    );
}

/// A `wait_for` closure runs after the suspension, so it resolves inside.
#[test]
fn wait_for_closure_resolves_inside() {
    let (out, run) = applied(
        "closure",
        "@sim\nco fn closure() -> Coroutine<int> {\n    let e = spawn_mob(5)\n    let m = e.get() ?? return\n    m.hp = 1\n    yield wait_for(() => m.hp > 0)\n}\n",
    );
    assert_eq!(
        out,
        "@sim\nco fn closure(world: Scene<WorldState>) -> Coroutine<int> {\n    let e = spawn_mob(5)\n    let m = e.get() ?? return\n    m.hp = 1\n    yield wait_for(() => e.get().unwrap().hp > 0)\n}\n",
        "{}",
        shown(&run)
    );
}

/// A borrow that is not a handle resolution is left as written and reported.
#[test]
fn unclassifiable_binding_is_left_and_reported() {
    let sim = "@sim\nco fn found() -> Coroutine<int> {\n    let m = find_boss()\n    await frame(1)\n    m.hp = 2\n}\n";
    let (out, run) = applied("unclassified", sim);
    assert_eq!(out, sim.replace("found()", "found(world: Scene<WorldState>)"), "{}", shown(&run));
    assert!(run.stdout.contains("left `m`") && run.stdout.contains("find_boss()"), "{}", shown(&run));
    assert_eq!(run.code, Some(3), "{}", shown(&run));
}

/// A resolution rooted in a global would re-read "the current object" after
/// the await (A2): it is left and reported, never rebound.
#[test]
fn global_rooted_binding_is_not_re_read() {
    let sim = "@sim\nco fn boss() -> Coroutine<int> {\n    let m = BOSS.get().unwrap()\n    await frame(1)\n    m.hp = 2\n}\n";
    let (out, run) = applied("boss", sim);
    assert_eq!(out, sim.replace("boss()", "boss(world: Scene<WorldState>)"), "{}", shown(&run));
    assert!(run.stdout.contains("left `m`") && run.stdout.contains("rooted in the global `BOSS`"), "{}", shown(&run));
}

/// Files with world semantics are skipped whole.
#[test]
fn world_switching_file_is_skipped() {
    let sim = "@sim\nco fn switch() -> Coroutine<int> {\n    let player = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    mag_set_active_world(MAG_ACTIVE_WORLD.unwrap())\n    await frame(1)\n    player.hp = 4\n}\n";
    let (out, run) = applied("switch", sim);
    assert_eq!(out, sim, "{}", shown(&run));
    assert!(run.stdout.contains("skipped (world-switch)"), "{}", shown(&run));
}

/// Without `--apply` nothing is written.
#[test]
fn check_mode_writes_nothing() {
    let sim = "@sim\nco fn simple() -> Coroutine<int> {\n    let player = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    await frame(1)\n    player.hp = 4\n}\n";
    let root = package("check", &[("sim", sim)]);
    let run = fix(&root, &["sim"], false);
    assert_eq!(read(&root.join("sim.rock")), sim, "{}", shown(&run));
    assert!(run.stdout.contains("across-yield 1 -> 0"), "{}", shown(&run));
    assert_eq!(run.code, Some(3), "{}", shown(&run));
}

/// `ROCK_IDENTITY_CENSUS_SITES` lists each site with the binding it names,
/// which is what the codemod's pilot and shard lists are chosen from.
#[test]
fn census_lists_sites_with_their_binding() {
    let root = package(
        "census",
        &[("sim", "@sim\nco fn simple() -> Coroutine<int> {\n    let player = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    await frame(1)\n    player.hp = 4\n}\n")],
    );
    let out = Command::new(env!("CARGO_BIN_EXE_rock"))
        .current_dir(root.parent().unwrap())
        .env("ROCK_IDENTITY_CENSUS_SITES", "across-yield")
        .args(["identity-storage-census", "."])
        .output()
        .expect("spawn rock");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let sites = stdout.split("sites (file, line, kind, identity, binding):\n").nth(1).unwrap_or_else(|| panic!("{stdout}"));
    assert_eq!(sites, "game/sim.rock\t5\tacross-yield\tgame::Player\tplayer\n");
}

/// With several suspensions between the binding and its use, the rebinding
/// goes after the last one on each path, including an await nested in a
/// branch of a loop body.
#[test]
fn rebinding_follows_the_last_suspension_on_each_path() {
    let (out, run) = applied(
        "last",
        "@sim\nco fn last() -> Coroutine<int> {\n    let player = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    await frame(1)\n    for round in 0..2 {\n        player.hp = round\n        if round == 0 {\n            await frame(2)\n            await frame(3)\n        }\n    }\n}\n",
    );
    assert_eq!(
        out,
        "@sim\nco fn last(world: Scene<WorldState>) -> Coroutine<int> {\n    let player = world.player()\n    await frame(1)\n    player = world.player()\n    for round in 0..2 {\n        player.hp = round\n        if round == 0 {\n            await frame(2)\n            await frame(3)\n            player = world.player()\n        }\n    }\n}\n",
        "{}",
        shown(&run)
    );
}

/// A sim that tests the handle for death expects the mob may be gone after
/// the await; rebinding with `unwrap()` would fail there, so the borrow is
/// left and reported.
#[test]
fn handle_tested_for_death_is_left() {
    let sim = "@sim\nco fn dies() -> Coroutine<int> {\n    let e = spawn_mob(5)\n    let m = e.get().unwrap()\n    await frame(1)\n    let gone = e.get() == null || m.hp <= 0\n}\n";
    let (out, run) = applied("death", sim);
    assert_eq!(out, sim.replace("dies()", "dies(world: Scene<WorldState>)"), "{}", shown(&run));
    assert!(run.stdout.contains("left `m`") && run.stdout.contains("for death"), "{}", shown(&run));
}
