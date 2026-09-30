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

#[test]
fn coroutine_helpers_resolve_scene_parameters_and_forward_the_scene() {
    let (out, run) = applied("helpers", r#"co fn inner(p: Player, n: int) -> Coroutine<int> {
    await frame(n)
    p.hp = n
}
co fn outer(s: WorldState, n: int) -> Coroutine<int> {
    await frame(n)
    s.player.get().unwrap().hp = n
}
co fn forward(p: Player) -> Coroutine<int> {
    await inner(p, 2)
    p.hp = 3
}
@sim
co fn helpers() -> Coroutine<int> {
    let player = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()
    let state = MAG_ACTIVE_WORLD.unwrap().get().unwrap()
    await forward(player)
    await outer(state, 4)
}
"#);
    assert!(out.contains("co fn inner(world: Scene<WorldState>, n: int)"), "{}\n{out}", shown(&run));
    assert!(out.contains("let p = world.player()\n    await frame(n)\n    p = world.player()"), "{}\n{out}", shown(&run));
    assert!(out.contains("let s = world.get().unwrap()\n    await frame(n)\n    s = world.get().unwrap()"), "{}\n{out}", shown(&run));
    assert!(out.contains("await inner(world, 2)"), "{}\n{out}", shown(&run));
    assert!(out.contains("await forward(world)"), "{}\n{out}", shown(&run));
    assert!(out.contains("await outer(world, 4)"), "{}\n{out}", shown(&run));
    assert!(run.stdout.contains("co-param 3 -> 0"), "{}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
    let root = package("helpers_twice", &[("sim", &out)]);
    let twice = fix(&root, &["sim"], true);
    assert_eq!(read(&root.join("sim.rock")), out, "{}", shown(&twice));
    assert_eq!(twice.code, Some(0), "{}", shown(&twice));
}

#[test]
fn externally_called_helper_is_refused() {
    let helper = "co fn shared(p: Player) -> Coroutine<int> {\n    await frame(1)\n    p.hp = 2\n}\n";
    let caller = "@sim\nco fn caller() -> Coroutine<int> {\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    await shared(p)\n}\n";
    let root = package("external", &[("helper", helper), ("caller", caller)]);
    let run = fix(&root, &["helper"], true);
    assert_eq!(read(&root.join("helper.rock")), helper, "{}", shown(&run));
    assert!(run.stdout.contains("called from another file"), "{}", shown(&run));
}

#[test]
fn helper_named_arguments_follow_the_changed_parameter_name() {
    let (out, run) = applied("helper_named", "co fn named(n: int, p: Player) -> Coroutine<int> {\n    await frame(n)\n    p.hp = n\n}\n@sim\nco fn caller() -> Coroutine<int> {\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    await named(p: p, n: 2)\n}\n");
    assert!(out.contains("await named(world: world, n: 2)"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn helper_cannot_forward_a_global_or_reassigned_player() {
    for (tag, init) in [("global", "MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()"), ("reassigned", "world.player()") ] {
        let caller = if tag == "global" {
            format!("fn caller() {{\n    let p = {init}\n    let task = helper(p)\n}}\n")
        } else {
            format!("@sim\nco fn caller(world: Scene<WorldState>) -> Coroutine<int> {{\n    let p = {init}\n    p = Player {{ hp: 9 }}\n    await helper(p)\n}}\n")
        };
        let helper = "co fn helper(p: Player) -> Coroutine<int> {\n    await frame(1)\n    p.hp = 2\n}\n";
        let (out, run) = applied(&format!("helper_{tag}"), &format!("{helper}{caller}"));
        assert!(out.contains(helper), "{}\n{out}", shown(&run));
        assert!(run.stdout.contains("refused helper"), "{}", shown(&run));
        assert_eq!(run.code, Some(3), "{}", shown(&run));
    }
}

#[test]
fn receiver_and_identity_returning_helpers_are_refused() {
    for (tag, helper) in [
        ("receiver", "co fn Player.receiver(p) -> Coroutine<int> {\n    await frame(1)\n    p.hp = 2\n}\n"),
        ("returns", "co fn returns(p: Player) -> Coroutine<int, Player> {\n    await frame(1)\n    return p\n}\n"),
    ] {
        let (out, run) = applied(&format!("helper_{tag}"), helper);
        assert_eq!(out, helper, "{}", shown(&run));
        assert!(run.stdout.contains(if tag == "receiver" { "receiver-style" } else { "returns an identity" }), "{}", shown(&run));
    }
}

#[test]
fn mob_helpers_forward_the_original_entity() {
    let (out, run) = applied("mob_helper", "co fn inner(m: Mob) -> Coroutine<int> {\n    await frame(1)\n    m.hp = 3\n}\nco fn outer(m: Mob) -> Coroutine<int> {\n    await inner(m)\n    m.hp = 4\n}\n@sim\nco fn caller() -> Coroutine<int> {\n    let target = spawn_mob(5)\n    let m = target.get() ?? return\n    await outer(m)\n}\n");
    assert!(out.contains("co fn inner(e: Entity<Mob>)"), "{}\n{out}", shown(&run));
    assert!(out.contains("let m = e.get().unwrap()\n    await frame(1)\n    m = e.get().unwrap()"), "{}\n{out}", shown(&run));
    assert!(out.contains("await inner(e)"), "{}\n{out}", shown(&run));
    assert!(out.contains("await outer(target)"), "{}\n{out}", shown(&run));
    assert!(run.stdout.contains("co-param 2 -> 0"), "{}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn mob_helper_without_a_local_entity_is_refused() {
    for (tag, init) in [("found", "find_boss()"), ("global", "BOSS.get().unwrap()")] {
        let helper = "co fn helper(m: Mob) -> Coroutine<int> {\n    await frame(1)\n    m.hp = 3\n}\n";
        let sim = format!("{helper}@sim\nco fn caller() -> Coroutine<int> {{\n    let m = {init}\n    await helper(m)\n}}\n");
        let (out, run) = applied(&format!("mob_helper_{tag}"), &sim);
        assert!(out.contains(helper), "{}\n{out}", shown(&run));
        assert!(run.stdout.contains("refused helper"), "{}", shown(&run));
        assert_eq!(run.code, Some(3), "{}", shown(&run));
    }
}

#[test]
fn mob_helper_avoids_handle_name_collisions_and_updates_named_calls() {
    let (out, run) = applied("mob_collision", "co fn helper(m: Mob) -> Coroutine<int> {\n    let e = 1\n    await frame(e)\n    m.hp = e\n}\n@sim\nco fn caller() -> Coroutine<int> {\n    let target = spawn_mob(5)\n    await helper(m: target.get().unwrap())\n}\n");
    assert!(out.contains("helper(e_2: Entity<Mob>)"), "{}\n{out}", shown(&run));
    assert!(out.contains("await helper(e_2: target)"), "{}\n{out}", shown(&run));
    assert!(out.contains("m = e_2.get().unwrap()"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn mob_helper_does_not_move_death_tested_handles_into_the_helper() {
    let helper = "co fn helper(m: Mob) -> Coroutine<int> {\n    await frame(1)\n    m.hp = 3\n}\n";
    let sim = format!("{helper}@sim\nco fn caller() -> Coroutine<int> {{\n    let target = spawn_mob(5)\n    await helper(target.get().unwrap())\n    let gone = target.get() == null\n}}\n");
    let (out, run) = applied("mob_helper_death", &sim);
    assert!(out.contains(helper), "{}\n{out}", shown(&run));
    assert!(run.stdout.contains("for death"), "{}", shown(&run));
}

#[test]
fn compact_loop_and_branch_rebind_inside_the_block() {
    let (out, run) = applied("compact", "@sim\nco fn compact() -> Coroutine<int> {\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    while p.hp < 3 { await frame(1) }\n    if p.hp == 3 { await frame(2) }\n    p.hp = 4\n}\n");
    assert!(out.contains("while p.hp < 3 { await frame(1)\n        p = world.player()\n    }"), "{}\n{out}", shown(&run));
    assert!(out.contains("if p.hp == 3 { await frame(2)\n        p = world.player()\n    }"), "{}\n{out}", shown(&run));
    assert!(run.stdout.contains("across-yield 3 -> 0"), "{}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn suspension_before_a_use_in_the_same_statement_is_refused() {
    let (out, run) = applied("condition", "co fn ready() -> Coroutine<int, bool> {\n    yield 1\n    return true\n}\n@sim\nco fn condition() -> Coroutine<int> {\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    if await ready() {\n        p.hp = 2\n    }\n}\n");
    assert!(run.stdout.contains("use within the same statement"), "{}\n{out}", shown(&run));
    assert!(!out.contains("p = world.player()\n}"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(3), "{}", shown(&run));
}

#[test]
fn unrelated_world_locals_are_renamed_by_binding_with_fresh_names() {
    let (out, run) = applied("world_collision", "value Coordinate { world: int }\n@sim\nco fn collides() -> Coroutine<int> {\n    let local_world = 0\n    let world = 7\n    let coords = Coordinate { world }\n    let read_world = () => world\n    world = world + 1\n    await frame(1)\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    p.hp = world + coords.world + read_world() + local_world\n    if true {\n        let world = 9\n        p.hp = world\n    }\n}\n");
    assert!(out.contains("collides(world: Scene<WorldState>)"), "{}\n{out}", shown(&run));
    assert!(out.contains("let local_world_2 = 7"), "{}\n{out}", shown(&run));
    assert!(out.contains("Coordinate { world: local_world_2 }"), "{}\n{out}", shown(&run));
    assert!(out.contains("() => local_world_2"), "{}\n{out}", shown(&run));
    assert!(out.contains("local_world_2 = local_world_2 + 1"), "{}\n{out}", shown(&run));
    assert!(out.contains("let local_world_3 = 9"), "{}\n{out}", shown(&run));
    assert!(out.contains("let p = world.player()"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn active_world_local_uses_a_fresh_state_name() {
    let (out, run) = applied("world_state_collision", "@sim\nco fn collides() -> Coroutine<int> {\n    let state = 0\n    let world_state = 0\n    let world = MAG_ACTIVE_WORLD.unwrap().get().unwrap()\n    await frame(1)\n    let hp = world.player.get().unwrap().hp\n    let mobs = world.mobs\n}\n");
    assert!(out.contains("let state_2 = world.get().unwrap()"), "{}\n{out}", shown(&run));
    assert!(out.contains("state_2 = world.get().unwrap()\n"), "{}\n{out}", shown(&run));
    assert!(out.contains("let hp = world.player().hp"), "{}\n{out}", shown(&run));
    assert!(out.contains("let mobs = state_2.mobs"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn new_check_errors_revert_entry_and_helper_edits_before_writing() {
    for (tag, sim, stage) in [
        ("entry", "@sim\nco fn caller() -> Coroutine<int> {\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    p.hp = 2\n}\n", "refused entry"),
        ("helper", "co fn helper(p: Player) -> Coroutine<int> {\n    await frame(1)\n    p.hp = 2\n}\n", "refused rebind"),
    ] {
        let root = package(&format!("revert_{tag}"), &[("sim", sim)]);
        write(&root.join("main.rock"), &WORLD.replace("pub fn player(world: Scene<WorldState>) -> Player {\n    return world.get().unwrap().player.get().unwrap()", "pub fn player(world: Scene<WorldState>) -> int {\n    return 0"));
        let run = fix(&root, &["sim"], true);
        assert_eq!(read(&root.join("sim.rock")), sim, "{}", shown(&run));
        assert!(run.stdout.contains(stage), "{}", shown(&run));
        assert_eq!(run.code, Some(3), "{}", shown(&run));
    }
}

#[test]
fn all_world_semantics_flags_skip_the_entire_file() {
    for (tag, marker) in [("run-lifecycle", "run_stop()"), ("restore", "snapshot_restore()"), ("menu-boot", "MAG_BOOT_MENU")] {
        let sim = format!("// {marker}\nco fn helper(p: Player) -> Coroutine<int> {{\n    await frame(1)\n    p.hp = 2\n}}\n@sim\nco fn caller() -> Coroutine<int> {{\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    await helper(p)\n}}\n");
        let (out, run) = applied(&format!("skip_{tag}"), &sim);
        assert_eq!(out, sim, "{}", shown(&run));
        assert!(run.stdout.contains(&format!("skipped ({tag})")), "{}", shown(&run));
    }
}

#[test]
fn helpers_forward_each_identity_parameter_with_its_own_handle() {
    let (out, run) = applied("mixed_helper", "co fn mixed(p: Player, s: WorldState, m: Mob) -> Coroutine<int> {\n    await frame(1)\n    p.hp = m.hp + s.player.get().unwrap().hp\n}\n@sim\nco fn caller() -> Coroutine<int> {\n    let p = MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n    let s = MAG_ACTIVE_WORLD.unwrap().get().unwrap()\n    let target = spawn_mob(5)\n    let m = target.get().unwrap()\n    await mixed(p, s, m)\n}\n");
    assert!(out.contains("mixed(world: Scene<WorldState>, world_2: Scene<WorldState>, e: Entity<Mob>)"), "{}\n{out}", shown(&run));
    assert!(out.contains("await mixed(world, world, target)"), "{}\n{out}", shown(&run));
    assert!(out.contains("s = world_2.get().unwrap()"), "{}\n{out}", shown(&run));
    assert!(out.contains("m = e.get().unwrap()"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn world_named_identity_parameter_keeps_its_borrow_name() {
    let (out, run) = applied("world_helper", "co fn helper(world: WorldState) -> Coroutine<int> {\n    await frame(1)\n    world.player.get().unwrap().hp = 2\n}\n@sim\nco fn caller() -> Coroutine<int> {\n    let s = MAG_ACTIVE_WORLD.unwrap().get().unwrap()\n    await helper(s)\n}\n");
    assert!(out.contains("helper(world_2: Scene<WorldState>)"), "{}\n{out}", shown(&run));
    assert!(out.contains("let world = world_2.get().unwrap()"), "{}\n{out}", shown(&run));
    assert!(out.contains("world = world_2.get().unwrap()\n"), "{}\n{out}", shown(&run));
    assert!(out.contains("await helper(world)"), "{}\n{out}", shown(&run));
    assert_eq!(run.code, Some(0), "{}", shown(&run));
}

#[test]
fn identity_returning_helpers_are_refused_even_without_identity_parameters() {
    for (tag, ty) in [("bare", "Player"), ("qualified", "game::Player")] {
        let helper = format!("co fn setup() -> Coroutine<int, {ty}> {{\n    await frame(1)\n    return MAG_ACTIVE_WORLD.unwrap().get().unwrap().player.get().unwrap()\n}}\n");
        let (out, run) = applied(&format!("return_identity_{tag}"), &helper);
        assert_eq!(out, helper, "{}", shown(&run));
        assert!(run.stdout.contains("co helper returns an identity"), "{}", shown(&run));
        assert_eq!(run.code, Some(3), "{}", shown(&run));
    }
}
