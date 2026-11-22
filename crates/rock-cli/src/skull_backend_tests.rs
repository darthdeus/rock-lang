use crate::compile_cmd;
#[cfg(unix)]
use crate::test_protocol::LuaTestSequence;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn source_fingerprint_support_files_follow_successful_live_compiles() {
    let mut fx = ProjectFixture::boot_files(&[
        (
            "stick.toml",
            "[package]\nname = \"fingerprint-watch\"\n[build.vars]\nREV = { source_fingerprint = { roots = [\"Root\"], files = [\"support.txt\"] } }\n",
        ),
        (
            "main.rock",
            "value Root { count: int }\nfn main() { print(build_var(\"REV\")) }\n",
        ),
        ("support.txt", "first decoder"),
    ]);
    let support = fx.path("support.txt").canonicalize().unwrap();
    let recorded_hash = |project: &super::IncrementalProject| {
        project
            .comptime_watch_files_with_hashes()
            .into_iter()
            .find(|(path, _)| path == &support)
            .expect("declared support file must reach the watcher")
            .1
    };
    let revision = |project: &super::IncrementalProject| {
        project.comptime_inputs().vars[&ustr::Ustr::from("REV")]
            .value
            .clone()
    };
    let first_hash = recorded_hash(&fx.project);
    let first_revision = revision(&fx.project);
    std::fs::write(&support, "second decoder").unwrap();
    assert_eq!(
        recorded_hash(&fx.project),
        first_hash,
        "watch seeding must describe compiled bytes, not reread current disk"
    );
    assert!(matches!(
        fx.recompile(std::slice::from_ref(&support)).unwrap(),
        super::SkullReload::Full { .. }
    ));
    let second_hash = recorded_hash(&fx.project);
    let second_revision = revision(&fx.project);
    assert_ne!(first_hash, second_hash);
    assert_ne!(first_revision, second_revision);
    std::fs::remove_file(&support).unwrap();
    assert!(fx.recompile(std::slice::from_ref(&support)).is_err());
    assert_eq!(recorded_hash(&fx.project), second_hash);
    assert_eq!(revision(&fx.project), second_revision);
    fx.edit_files(&[(
        "stick.toml",
        "[package]\nname = \"fingerprint-watch\"\n[build.vars]\nREV = \"fixed\"\n",
    )])
    .unwrap();
    assert!(
        fx.project
            .comptime_watch_files_with_hashes()
            .iter()
            .all(|(path, _)| path != &support)
    );
    assert_eq!(revision(&fx.project), "fixed");
}

#[test]
fn allocation_report_writer_is_explicit_and_writes_the_compiler_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("allocation-report.txt");
    let report = skull::allocation_report::AllocationReport::default();

    super::write_allocation_report(Some(&path), &report).expect("write report");

    let text = std::fs::read_to_string(path).expect("read report");
    assert!(text.contains("emitted allocation-capable Lua report: 0 bodies, 0 direct sites"));
    assert!(text.contains("emitted site manifest: 0 sites"));
}

#[path = "overlay_test_support.rs"]
mod overlay_test_support;
use overlay_test_support::*;

struct ProjectFixture {
    dir: tempfile::TempDir,
    project: super::IncrementalProject,
    main: std::path::PathBuf,
    incremental: std::path::PathBuf,
}

impl ProjectFixture {
    fn boot(source_text: &str) -> Self {
        Self::boot_files(&[("main.rock", source_text)])
    }

    fn boot_files(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).expect("initial fixture file");
        }
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let project =
            super::IncrementalProject::init(dir.path().to_str().unwrap(), &main, &incremental)
                .expect("initial build");
        Self {
            dir,
            project,
            main,
            incremental,
        }
    }

    fn path(&self, file: &str) -> std::path::PathBuf {
        self.dir.path().join(file)
    }

    fn edit(&mut self, text: &str) -> anyhow::Result<super::SkullReload> {
        self.edit_files(&[("main.rock", text)])
    }

    fn edit_files(&mut self, files: &[(&str, &str)]) -> anyhow::Result<super::SkullReload> {
        let paths = files
            .iter()
            .map(|(name, text)| {
                let path = self.path(name);
                std::fs::write(&path, text).expect("edited fixture file");
                path
            })
            .collect::<Vec<_>>();
        self.recompile(&paths)
    }

    fn recompile(&mut self, paths: &[std::path::PathBuf]) -> anyhow::Result<super::SkullReload> {
        self.project.recompile(paths, &self.main, &self.incremental)
    }

    fn force_full(&mut self) -> anyhow::Result<()> {
        self.project.force_full(&self.main, &self.incremental)
    }

    fn main_text(&self) -> String {
        std::fs::read_to_string(&self.main).expect("generated main.lua")
    }

    fn overlay_text(&self) -> String {
        std::fs::read_to_string(&self.incremental).expect("generated incremental.lua")
    }
}

fn session(src: &str) -> bone_incremental::session::Session {
    let mut source = rock_core::source_file_from_path_and_contents(
        "main.rock".to_string(),
        "main.rock".to_string(),
        src.to_string(),
    );
    source.package = Some(skull_parser::PackageTag {
        id: 1,
        self_name: "@/<backend-test>".to_string(),
        identity_storage: skull_parser::IdentityStorage::Exempt,
        root: String::new(),
    });
    crate::bone_backend::bone_session(&rock_core::SourceFiles::from_single_file(source))
        .expect("session")
}

#[cfg(test)]
mod overlay_runtime_tests {

    use super::session;
    use super::{ExecOutcome, exec_lua, normalize_stdout};

    use super::overlay_test_support::{SYS_STUB, run_parts, stub_for};

    const SCORE4: &str = "fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n";

    macro_rules! pump_source {
        () => {
            "@nomangle\nfn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n"
        };
    }
    const START_PUMP: &str = concat!("@nomangle\nfn start() { GEN = gen() }\n", pump_source!());

    struct Live {
        session: bone_incremental::session::Session,
        mid: bone_semantic::prelude::ModuleId,
        baseline: String,
        cache: skull::mir_emit::SkullEmitCache,
    }

    impl Live {
        fn boot(v0: &str) -> Self {
            let session = session(v0);
            let mid = skull::user_module_id(session.workspace()).unwrap();
            let (full, cache) =
                skull::mir_emit::emit_full(session.workspace(), mid).expect("emit_full v0");
            Self {
                session,
                mid,
                baseline: full.lua,
                cache,
            }
        }

        fn body_edit(&mut self, src: &str) -> bone_incremental::session::UpdateResult {
            let update = self.session.update_file("main.rock", src).expect("update");
            assert!(
                matches!(update.tier, bone_incremental::session::UpdateTier::BodyOnly),
                "premise: this edit is body-only, got {:?}",
                update.tier
            );
            update
        }

        fn emit(&mut self, update: &bone_incremental::session::UpdateResult) -> String {
            skull::mir_emit::emit_overlay(
                self.session.workspace(),
                &mut self.cache,
                &update.reinferred,
            )
            .expect("emit_overlay")
            .lua
        }

        fn overlay(&mut self, src: &str) -> String {
            let update = self.body_edit(src);
            self.emit(&update)
        }

        fn full_reload(&mut self, src: &str) -> String {
            let update = self.session.update_file("main.rock", src).expect("update");
            assert!(
                matches!(
                    update.tier,
                    bone_incremental::session::UpdateTier::Interface
                        | bone_incremental::session::UpdateTier::FullUser
                ),
                "premise: this edit forces a full rebuild, got {:?}",
                update.tier
            );
            self.full()
        }

        fn full(&self) -> String {
            skull::mir_emit::emit_full(self.session.workspace(), self.mid)
                .expect("emit_full")
                .0
                .lua
        }
    }

    fn assert_stdout(outcome: ExecOutcome, expected: &str, ctx: &str) {
        match outcome {
            ExecOutcome::Output(output) => {
                assert_eq!(normalize_stdout(&output).trim(), expected, "{ctx}")
            }
            ExecOutcome::Errored(error) => panic!("{ctx}: run failed: {error}"),
        }
    }

    fn assert_run(program: &str, steps: &[&str], expected: &str, ctx: &str) {
        let mut parts = vec![program, stub_for(program)];
        parts.extend_from_slice(steps);
        assert_stdout(run_parts(&parts), expected, ctx);
    }

    fn assert_overlay_matches_full(v0: &str, v1: &str, driver: &str, before: &str, after: &str) {
        let mut live = Live::boot(v0);
        assert_run(&live.baseline, &[driver], before, "baseline");
        let overlay = live.overlay(v1);
        assert_run(&live.full(), &[driver], after, "fresh build");
        assert_run(&live.baseline, &[&overlay, driver], after, "overlay");
    }

    fn assert_resume_after_overlay(
        v0: &str,
        v1: &str,
        resume: &[&str],
        expected: &str,
        case: &str,
    ) {
        let mut live = Live::boot(v0);
        let overlay = live.overlay(v1);
        let steps = [&["start()", "pump()", &overlay][..], resume].concat();
        assert_run(&live.baseline, &steps, expected, case);
    }

    fn sim_fixture(tick_val: i32) -> String {
        format!(
            "struct UpdateSystem {{ func: fn() -> (), name: string, stage: int }}\n\
             global UPDATE_SYSTEMS: [UpdateSystem] = []\n\
             deco init update(stage: int = 100) {{\n\
                UPDATE_SYSTEMS.push(UpdateSystem {{ func: transmute(target.func), name: transmute(target.name), stage: stage }})\n\
             }}\n\
             global TICK_OUT: int = 0\n\
             @update\n\
             fn tick() {{ TICK_OUT = {tick_val} }}\n\
             @nomangle\n\
             fn run_frame() {{ for s in UPDATE_SYSTEMS {{ s.func() }} }}\n"
        )
    }

    #[test]
    fn failed_overlay_after_late_top_level_work_keeps_direct_calls_and_code_refs_old() {
        let v0 = format!(
            "const LATE: int = 0\nfn explode() -> int {{ error(\"late overlay failure\") return 0 }}\n{}",
            format!(
                "{}\n@nomangle fn direct_frame() {{ tick() }}\n",
                sim_fixture(1)
            )
            .replace(
                "TICK_OUT = 1",
                "TICK_OUT = 1 + LATE\nif TICK_OUT < 0 { direct_frame() }"
            )
        );
        let v1 = format!(
            "const LATE: int = explode()\nfn explode() -> int {{ error(\"late overlay failure\") return 0 }}\n{}",
            format!(
                "{}\n@nomangle fn direct_frame() {{ tick() }}\n",
                sim_fixture(2)
            )
            .replace(
                "TICK_OUT = 2",
                "TICK_OUT = 2 + LATE\nif TICK_OUT < 0 { direct_frame() }"
            )
        );
        let mut live = Live::boot(&v0);

        let overlay = live.overlay(&v1);

        let script = format!(
            "{}\n{}\nlocal retained_direct = direct_frame\nlocal ok, err = xpcall(function()\n{}end, debug.traceback)\nassert(not ok and tostring(err):find(\"late overlay failure\", 1, true), tostring(err))\nif not ok then __rock_code_slot_abort() end\nprint(ok)\nprint(_G.__rock_code_slot_pending == nil)\nrun_frame()\nprint(TICK_OUT)\nretained_direct()\nprint(TICK_OUT)\nassert(direct_frame == retained_direct, \"direct caller identity changed\")",
            live.baseline, SYS_STUB, overlay
        );

        assert_stdout(
            exec_lua(&script),
            "false\ntrue\n1\n1",
            "a failed overlay must keep the retained v0 CodeRef and direct caller",
        );
    }

    #[test]
    fn failed_overlay_without_code_refs_restores_globals_and_can_retry() {
        let v0 = r#"
nobox value Pair { x: int, y: int }
global EFFECTS: int = 0
@nomangle fn effects() -> int { EFFECTS }
fn identity<T>(value: T) -> T { value }
fn number() -> int { 10 }
fn boolean() -> bool { false }
fn explode() -> int { EFFECTS = EFFECTS + 1 error("late overlay failure") return 0 }
const EARLY: int = number()
const FLAG: bool = boolean()
const PAIR: Pair = Pair { x: number(), y: number() + 1 }
const LATE: int = number()
@nomangle fn direct() -> int { EARLY + LATE }
fn main() { print(direct()) print(FLAG) print("{PAIR.x} {PAIR.y}") }
"#;
        let v1 = v0
            .replace(
                "const EARLY: int = number()",
                "const EARLY: int = number() + 1",
            )
            .replace(
                "const FLAG: bool = boolean()",
                "const FLAG: bool = !boolean()",
            )
            .replace("x: number(),", "x: number() + 2,")
            .replace("const LATE: int = number()", "const LATE: int = explode()")
            .replace("{ EARLY + LATE }", "{ identity(EARLY + LATE) }");
        let mut live = Live::boot(v0);
        let failed = live.overlay(&v1);
        let repaired = live.overlay(&v1.replace("= explode()", "= number() + 2"));
        let failed_attempt = checked_late_initializer_failure(&failed);
        let driver = format!(
            r#"
local retained = direct
local early, flag, late = EARLY, FLAG, LATE
{failed_attempt}
assert(EARLY == early and FLAG == flag and LATE == late, "constants changed after failure")
assert(effects() == 1, "initializer side effect was unexpectedly rolled back")
assert(direct == retained and retained() == 20, "direct caller changed after failure")
main()
{repaired}
assert(direct() == 23 and retained() == 23, "retry did not publish current callees")
main()
"#
        );
        assert_run(
            &live.baseline,
            &[&driver],
            "20\nfalse\n10 11\n23\ntrue\n12 11",
            "failed overlay rollback and repaired retry without CodeRefs",
        );
    }

    const STATIC_RETRY_FIXTURE: &str = "@nomangle fn probe() -> int { static cell = 1\n cell = cell + 1\n cell }\n\
                  const LATE: int = 0\n\
                  fn explode() -> int { error(\"late overlay failure\") return 0 }\n\
                  fn main() { print(probe()) print(LATE) }\n";

    fn checked_late_initializer_failure(overlay: &str) -> String {
        format!(
            r#"local ok, err = pcall(function()
{overlay}
end)
assert(not ok and tostring(err):find("late overlay failure", 1, true), tostring(err))"#
        )
    }

    #[test]
    fn failed_overlay_static_initializer_retry_preserves_successive_live_state() {
        let v0 = STATIC_RETRY_FIXTURE;
        let changed = v0.replace("static cell = 1", "static cell = 100");
        let failing = changed.replace("const LATE: int = 0", "const LATE: int = explode()");
        let mut live = Live::boot(v0);
        let failed = live.overlay(&failing);
        let repaired = live.overlay(&changed);
        let unrelated =
            live.overlay(&changed.replace("cell = cell + 1", "let unused = 0\n cell = cell + 1"));
        let reverted = live.overlay(v0);
        let failed_attempt = checked_late_initializer_failure(&failed);
        let driver = format!(
            r#"
print(probe())
print(probe())
{failed_attempt}
print(probe())
{repaired}
print(probe())
print(probe())
{repaired}
print(probe())
{unrelated}
print(probe())
{reverted}
print(probe())
"#
        );
        assert_run(
            &live.baseline,
            &[&driver],
            "2\n3\n4\n101\n102\n103\n104\n2",
            "a failed initializer edit must retry its reset and preserve later accumulated state",
        );
    }

    #[test]
    fn failed_overlay_static_removal_retry_drops_cells_and_can_reintroduce_them() {
        let v0 = STATIC_RETRY_FIXTURE;
        let removed = v0.replace("static cell = 1\n cell = cell + 1\n cell", "9");
        let failing = removed.replace("const LATE: int = 0", "const LATE: int = explode()");
        let mut live = Live::boot(v0);
        let failed = live.overlay(&failing);
        let repaired = live.overlay(&removed);
        let reintroduced = live.overlay(v0);
        let failed_attempt = checked_late_initializer_failure(&failed);
        let driver = format!(
            r#"
assert(probe() == 2, "the baseline static must be warm")
{failed_attempt}
assert(probe() == 3, "failed removal changed the warmed static")
{repaired}
assert(probe() == 9, "retry failed to drop the removed static")
{reintroduced}
assert(probe() == 2 and probe() == 3, "reintroduced static did not initialize once")
{reintroduced}
assert(probe() == 4, "reloading the reintroduced static reset live state")
print("STATIC_REMOVAL_RETRY")
"#
        );
        assert_run(
            &live.baseline,
            &[&driver],
            "STATIC_REMOVAL_RETRY",
            "static removal must survive failed emission and reintroduction",
        );
    }

    #[test]
    fn wire_hook_failure_is_post_publication_and_not_reported_as_rollback() {
        let v0 = r#"
@lifecycle(phase="wire", owner="process")
deco init wire {}
const DEP: int = 1
global RUNS: int = 0
@wire(onchange=[DEP]) fn gated_wire() { RUNS = RUNS + 1 }
fn main() {}
"#;
        let v1 = v0.replace(
            "RUNS = RUNS + 1",
            "error(\"wire hook failed after publication\")",
        );
        let mut live = Live::boot(v0);
        let overlay = live.overlay(&v1);
        let driver = r#"
local function run_wire()
  local phase = _G.__ROCK_LIFECYCLE_PLAN.wire
  for i = 1, phase[0] do
    local entry = phase[i]
    if __rock_lifecycle_take_wire(entry.key) then entry.func() end
  end
end
local runs_key = __rock_global_entry("RUNS").slots[1]
run_wire()
assert(__rock_globals[runs_key] == 1, "initial wire did not run exactly once: " .. tostring(__rock_globals[runs_key]))
local old_plan = _G.__ROCK_LIFECYCLE_PLAN
REPLACE_OVERLAY
local candidate_plan = _G.__ROCK_LIFECYCLE_PLAN
assert(candidate_plan ~= old_plan, "candidate LifecyclePlan was not published")
local ok, err = pcall(run_wire)
assert(not ok and tostring(err):find("wire hook failed after publication", 1, true), "wire failure was not observed: " .. tostring(err))
assert(_G.__rock_code_slot_pending == nil, "the hook failure must not leave a staging transaction")
assert(_G.__ROCK_LIFECYCLE_PLAN == candidate_plan, "hook failure must not claim publication rollback")
assert(__rock_globals[runs_key] == 1, "wire failure changed the retained effect")
print("WIRE_HOOK_FAILURE_POST_PUBLICATION")
"#
        .replace("REPLACE_OVERLAY", &overlay);
        let script = format!("{}\n{}\n{}", live.baseline, SYS_STUB, driver);
        match exec_lua(&script) {
            ExecOutcome::Output(output) => {
                assert!(normalize_stdout(&output).contains("WIRE_HOOK_FAILURE_POST_PUBLICATION"))
            }
            ExecOutcome::Errored(error) => panic!("wire hook failure driver failed: {error}"),
        }
    }

    #[test]
    fn coroutine_body_edit_overlay_runs_new_body() {
        let v0 = "co fn counter() -> Coroutine<int> {\n  yield 1\n  yield 2\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";
        let v1 = v0.replace("yield 1\n  yield 2", "yield 10\n  yield 20");
        assert_overlay_matches_full(v0, &v1, "main()", "1 2", "10 20");
    }

    const EXPECT_LAYOUT_ERROR: &str = "local ok, err = pcall(pump); assert(not ok and tostring(err):find('frame layout changed', 1, true)); pump()";

    #[test]
    fn live_coroutine_yield_renumber_overlay_stamp_kills() {
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n{extra}  yield Ev::Tick(1)\n  print(\"fire\")\n  yield Ev::Tick(2)\n  print(\"cleanup\")\n}}\n\
                 {SCORE4}\
                 {START_PUMP}"
            )
        };
        let (v0, v1) = (prog(""), prog("  yield Ev::Done\n"));

        assert_resume_after_overlay(
            &v0,
            &v1,
            &[EXPECT_LAYOUT_ERROR, "pump()", "pump()"],
            "1\n-1\n-1\n-1",
            "inserting a yield invalidates the retained resume point",
        );
    }

    fn assert_co_fn_rejected(src: &str, needle: &str) {
        let session = session(src);
        let mid = skull::user_module_id(session.workspace()).unwrap();
        match skull::mir_emit::emit_full(session.workspace(), mid) {
            Err(e) => assert!(
                format!("{e:?}").contains(needle),
                "rejection must mention {needle}, got: {e:?}"
            ),
            Ok((full, _)) => assert!(
                !full.lua.contains("function gen_0__step("),
                "a rejected co fn must not emit:
{}",
                full.lua
            ),
        }
    }

    #[test]
    fn non_lambda_coroutine_hook_arg_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   fn cleanup() { print(\"x\") }\n\
                   co fn gen() -> Coroutine<Ev> {\n  on_killed(cleanup)\n  yield Ev::Tick(1)\n}\n\
                   fn main() { let c = gen() }\n";
        assert_co_fn_rejected(src, "lambda");
    }

    #[test]
    fn yield_inside_defer_body_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   co fn gen() -> Coroutine<Ev> {\n  defer { yield Ev::Tick(1) }\n  yield Ev::Tick(2)\n}\n\
                   fn main() { let c = gen() }\n";
        assert_co_fn_rejected(src, "defer");
    }

    // Mob uses compiler-private slots, so the probe reads the global's storage
    // and resolves field names through the reloaded type's slot map.
    const LIVE_MOB_PROBE: &str = r#"
local m = __rock_globals[__rock_global_entry("M").slots[1]]
local slots = getmetatable(m).__rock_slots
print("hp=" .. tostring(m[slots.hp]) .. " armor=" .. tostring(m[slots.armor]))
"#;

    #[test]
    fn struct_field_add_full_reload_nil_poisons_live_instances() {
        let v0 = "struct Mob { hp: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10 } }\n\
                  @nomangle\n\
                  fn describe() {\n  let m = M\n  if m != null { print(\"{m.hp}\") }\n}\n\
                  fn main() { }\n";
        let v1 = "struct Mob { hp: int, armor: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10, armor: 5 } }\n\
                  @nomangle\n\
                  fn describe() {\n  let m = M\n  if m != null { print(\"{m.hp + m.armor}\") }\n}\n\
                  fn main() { }\n";

        let mut live = Live::boot(v0);

        let full1 = live.full_reload(v1);

        assert_run(
            &live.baseline,
            &[
                "spawn()",
                "describe()",
                "_G.HOT_RELOAD = true",
                &full1,
                LIVE_MOB_PROBE,
            ],
            "10\nhp=10 armor=nil",
            "a live struct instance must survive the full re-dofile WITHOUT the added field",
        );

        let composed = run_parts(&[
            &live.baseline,
            stub_for(&live.baseline),
            "spawn()",
            "describe()",
            "_G.HOT_RELOAD = true",
            &full1,
            "describe()",
        ]);
        let diag = composed
            .expect_errored("expected the recompiled reader to die on the nil-valued added field");
        assert!(
            diag.contains("arithmetic on") && diag.contains("nil"),
            "expected nil-arithmetic on the field the live instance never had, got:\n{diag}"
        );
    }
}
#[cfg(test)]
mod shape_drift_tests {

    use super::super::SkullReload;
    use super::ProjectFixture;

    #[test]
    fn struct_layout_edits_request_restart_and_defer_the_write() {
        let v0 = "struct Mob { hp: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10 } }\n\
                  fn main() { }\n";
        let v1 = "struct Mob { hp: int, armor: int }\n\
                  global M: Mob? = null\n\
                  @nomangle\n\
                  fn spawn() { M = Mob { hp: 10, armor: 5 } }\n\
                  fn main() { }\n";
        let reordered = v1.replace("hp: int, armor: int", "armor: int, hp: int");
        let native = "value NativePoint { x: float }\n\
                      value NativeRow { point: NativePoint, a: int, b: int, c: int, d: int }\n\
                      global ROWS: [NativeRow] = []\n\
                      fn main() { ROWS.push(NativeRow { point: NativePoint { x: 1.0 }, a: 0, b: 0, c: 0, d: 0 }) }\n";
        let native_wide = native.replace("x: float", "x: float, y: float = 0.0");
        for (before, after, drifted) in [
            (v0, v1, "Mob"),
            (v1, v0, "Mob"),
            (v1, reordered.as_str(), "Mob"),
            (native, native_wide.as_str(), "NativePoint"),
        ] {
            let mut fx = ProjectFixture::boot(before);
            let baseline = fx.main_text();

            let reload = fx.edit(after).expect("recompile edited layout");
            match &reload {
                SkullReload::ShapeChanged { types } => assert!(
                    types.iter().any(|t| t == drifted),
                    "the restart request must name the drifted type, got {types:?}"
                ),
                other => {
                    panic!(
                        "a struct shape edit must request a restart (ShapeChanged), got {other:?}"
                    )
                }
            }
            assert_eq!(
                fx.main_text(),
                baseline,
                "the new-shape main.lua write must be DEFERRED -- the old process must never dofile it"
            );

            fx.force_full().expect("force_full");
            assert_ne!(
                fx.main_text(),
                baseline,
                "canonicalize after the kill must write the deferred new-shape program"
            );

            let again = fx.edit(after).expect("recompile same");
            assert!(
                matches!(again, SkullReload::NoChange),
                "the shape delta must be consumed by the restart, got {again:?}"
            );
        }
    }

    #[test]
    fn mutable_global_membership_edit_requests_restart_and_keeps_old_global_layout() {
        let v0 = "global A: int = 1\nfn main() { }\n";
        let v1 = "global A: int = 1\nglobal B: int = 2\nfn main() { }\n";
        let mut fx = ProjectFixture::boot(v0);
        let baseline = fx.main_text();
        let old_layout = fx
            .project
            .cache
            .global_layout()
            .expect("baseline global layout")
            .to_string();

        let reload = fx.edit(v1).expect("membership edit");
        match reload {
            SkullReload::FullRequired { reasons } => assert!(
                reasons
                    .iter()
                    .any(|reason| reason.contains("global layout changed")),
                "membership drift must name the global layout: {reasons:?}"
            ),
            other => panic!("adding a mutable global must require restart, got {other:?}"),
        }
        assert_eq!(
            fx.main_text(),
            baseline,
            "an incompatible layout must not replace the old process artifact"
        );
        assert_eq!(
            fx.project
                .cache
                .global_layout()
                .expect("retained global layout"),
            old_layout,
            "a refused publication must retain the old global layout"
        );

        fx.force_full().expect("restart canonicalization");
        let current = fx.main_text();
        assert_ne!(
            current, baseline,
            "force_full must materialize the new layout"
        );
        assert_ne!(
            fx.project.cache.global_layout().expect("new global layout"),
            old_layout,
            "restart canonicalization must publish the changed layout"
        );
    }

    #[test]
    fn shader_resource_interface_edit_requests_full_reload() {
        let v0 = "@shader(fragment)\n\
                  fn frag(tc: Vec2) -> Vec4 {\n\
                    return Vec4 { x: tc.x, y: 0.0, z: 1.0, w: 1.0 }\n\
                  }\n\
                  global KEY: string = rock_shader(frag)\n\
                  fn main() { }\n";
        let v1 = "@shader(fragment)\n\
                  fn frag(tex: Sampler2D, tc: Vec2) -> Vec4 {\n\
                    return texture(tex, tc)\n\
                  }\n\
                  global KEY: string = rock_shader(frag)\n\
                  fn main() { }\n";
        let mut fx = ProjectFixture::boot(v0);
        let baseline = fx.main_text();

        let reload = fx.edit(v1).expect("shader recompile");
        assert!(
            matches!(reload, SkullReload::Full { .. }),
            "a shader resource/interface change must use the full path, got {reload:?}"
        );
        let current = fx.main_text();
        assert_ne!(
            current, baseline,
            "the resource edit must change the registered artifact"
        );
    }
}

#[cfg(test)]
mod wire_transition_tests {
    use super::super::SkullReload;
    use super::ProjectFixture;

    const V0: &str = r#"
@lifecycle(phase="wire", owner="process")
deco init wire {}

const DIRECT: int = 1
const BASE: int = 2
const DERIVED: int = BASE + 1
const UNRELATED: int = 9

fn unrelated() -> int { return UNRELATED }
@wire fn plain_wire() { let _ = unrelated() }
@wire(onchange=[DIRECT]) fn direct_wire() { let _ = DIRECT }
@wire(onchange=[DERIVED]) fn transitive_wire() { let _ = DERIVED }
fn main() {}
"#;

    #[test]
    fn gated_wire_removal_requires_restart_and_keeps_the_last_good_artifact() {
        let mut fx = ProjectFixture::boot(V0);
        let baseline = fx.main_text();
        let removed = V0.replace(
            "@wire(onchange=[DIRECT]) fn direct_wire() { let _ = DIRECT }\n",
            "",
        );
        let reload = fx.edit(&removed).expect("gated removal route");
        match reload {
            SkullReload::FullRequired { reasons } => assert!(
                reasons
                    .iter()
                    .any(|reason| reason.contains("removing a gated @wire")),
                "removal reason must request a fresh projection: {reasons:?}"
            ),
            other => panic!("gated removal must not publish live state: {other:?}"),
        }
        assert_eq!(fx.main_text(), baseline);
        assert_eq!(
            fx.project.source_text(&fx.path("main.rock")).as_deref(),
            Some(removed.as_str()),
            "the current source may remain restart-pending while the published artifact stays old"
        );
    }
}

#[cfg(test)]
mod force_full_tests {

    use super::super::IncrementalProject;
    use super::ProjectFixture;
    use crate::skull_backend::SkullReload;

    #[test]
    fn force_full_folds_overlay_matches_fresh_and_wipes() {
        // Needs a `main`: emission is reachability-driven, so a fixture with no
        // entry point at all emits no user code and the two builds would compare
        // equal for a reason that has nothing to do with the fold.
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\nfn main() { print(\"{use_it()}\") }\n";
        let v2 = "fn helper() -> int { 5 }\nfn use_it() -> int { helper() }\nfn main() { print(\"{use_it()}\") }\n";

        let mut fx = ProjectFixture::boot(v1);
        let main_v1 = fx.main_text();
        assert!(!fx.incremental.exists(), "init must leave no overlay");

        let reload = fx.edit(v2).expect("recompile v2");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a const body edit should route to the overlay (Incremental tier)"
        );
        assert!(fx.incremental.exists(), "body edit must produce an overlay");
        assert_eq!(
            fx.main_text(),
            main_v1,
            "a body edit must NOT touch main.lua (overlay carries the change)"
        );

        fx.force_full().expect("force_full");
        let main_forced = fx.main_text();
        assert!(!fx.incremental.exists(), "force_full must wipe the overlay");
        assert_ne!(
            main_forced, main_v1,
            "force_full must fold the v2 edit into main.lua, not serve the stale v1 baseline"
        );

        let fresh_main = fx.path("fresh_main.lua");
        let fresh_inc = fx.path("fresh_incremental.lua");
        let fresh =
            IncrementalProject::init(fx.dir.path().to_str().unwrap(), &fresh_main, &fresh_inc)
                .expect("fresh init v2");
        let main_fresh = std::fs::read_to_string(&fresh_main).expect("fresh main.lua");
        assert_eq!(
            main_forced, main_fresh,
            "force_full main.lua must be byte-identical to a fresh full build of the edited source"
        );
        assert_eq!(
            fx.project.artifact_fingerprint().unwrap(),
            fresh.artifact_fingerprint().unwrap(),
            "force_full must publish the fingerprint of the Artifact it writes"
        );
    }
}

#[cfg(test)]
mod value_test_exec {

    use super::super::*;

    #[test]
    fn value_mismatch_fails_with_detail() {
        let src = "@test(returns = 43)\nfn r() -> int { return 42 }\n";
        let outcome = skull_build("val.rock", src, None).expect("build ok");
        assert_eq!(outcome.passed, 0);
        assert_eq!(outcome.fails, ["r: value mismatch: expected <43> got <42>"]);
    }

    #[test]
    fn value_test_error_still_fails() {
        let src = "@test(returns = 1)\nfn e() -> int { error(\"boom\")\n return 1 }\n";
        let fails = skull_build("val.rock", src, None).expect("build ok").fails;
        assert!(
            fails.first().is_some_and(|fail| fail.contains("error:")),
            "{fails:?}"
        );
    }

    #[test]
    fn a_type_hole_mismatch_fails_the_unit() {
        let src = "@test(type_holes = [[0, \"string\"]])\nfn a_test_with_a_long_descriptive_name() { let x: ? = 1 }\n";
        let err = skull_build("holes.rock", src, None).expect_err("hole mismatch must reject");
        assert!(
            err.reason.contains(
                "`a_test_with_a_long_descriptive_name`: hole 0: expected `string`, inferred `int`"
            ),
            "unexpected reason: {}",
            err.reason
        );
    }
}

#[cfg(test)]
mod backend_stats_workspace_tests {
    use super::super::skull_emit_timed;
    use rock_host::compilation_units::load_sources_for_project;

    #[test]
    fn timed_emission_prepares_comptime_inputs_and_populates_attribution() {
        let dir = tempfile::tempdir().expect("temporary project");
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"timed-preparation\"\n\n[build.vars]\nREQUIRED = \"prepared\"\n",
        )
        .expect("stick.toml");
        std::fs::write(
            dir.path().join("main.rock"),
            "fn main() { print(build_var(\"REQUIRED\")) }\n",
        )
        .expect("main.rock");

        let sources = load_sources_for_project(dir.path()).expect("discover timed project");
        let (ws, entry) = crate::bone_backend::workspace_frontend_timed(&sources)
            .0
            .expect("timed project checks");
        let (ok, times) = skull_emit_timed(ws, entry, dir.path());

        assert!(ok, "timed emission must succeed for a prepared project");
        assert!(
            times.attribution.bodies_examined > 0,
            "successful timed emission must populate its body denominator"
        );
        assert!(
            !times.emit_phases.is_empty(),
            "successful timed emission must populate production phase attribution"
        );
    }

    #[test]
    fn timed_emission_uses_invoked_project_build_vars_with_dependency_first() {
        let dir = tempfile::tempdir().expect("temporary projects");
        let project = dir.path().join("application");
        let dependency = dir.path().join("dependency");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dependency).unwrap();
        std::fs::write(
            project.join("stick.toml"),
            "[package]\nname = \"application\"\n[dependencies]\ndependency = { path = \"../dependency\" }\n[build.vars]\nREQUIRED = \"application-value\"\n",
        ).unwrap();
        std::fs::write(
            dependency.join("stick.toml"),
            "[package]\nname = \"dependency\"\n",
        )
        .unwrap();
        std::fs::write(
            dependency.join("library.rock"),
            "fn library_value() -> int { return 7 }\n",
        )
        .unwrap();
        std::fs::write(
            project.join("application.rock"),
            "fn main() { print(build_var(\"REQUIRED\")); print(library_value()) }\n",
        )
        .unwrap();
        let sources = load_sources_for_project(&project).unwrap();
        let (_, entry, _) = crate::bone_backend::parse_user_modules(&sources).unwrap();
        assert!(
            std::path::Path::new(entry.as_str()).starts_with(
                std::fs::canonicalize(&dependency).expect("canonical dependency path")
            ),
            "fixture must exercise dependency-first entry selection without main.rock"
        );
        let result = crate::backend_stats::do_backend_stats(project.to_str().unwrap(), 1, 0, false);
        assert!(
            result.is_ok(),
            "profiling must use the invoked project's build vars: {result:?}"
        );
    }
}

fn file_limit_package(first: &str, second: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("timeout package");
    std::fs::write(
        dir.path().join("stick.toml"),
        "[package]\nname = \"file-limit\"\ntype = \"lib\"\n",
    )
    .unwrap();
    // The marker proves the first test body ran, so a timeout is the test's
    // own and not a loading or compile failure.
    let first = format!(
        "local started = assert(io.open([=[{}]=], 'w')); \
         assert(started:write('started')); assert(started:close()); {first}",
        dir.path().join("test-start").display()
    );
    std::fs::write(
        dir.path().join("first.rock"),
        format!("@test inline(lua) fn first_test() = \"{first}\"\n"),
    )
    .unwrap();
    if let Some(second) = second {
        std::fs::write(
            dir.path().join("second.rock"),
            format!("@test inline(lua) fn second_test() = \"{second}\"\n"),
        )
        .unwrap();
    }
    dir
}

type LimitedBuild = Result<super::SkullOutcome, crate::test_unit::UnitFailure>;

fn build_with_limit(dir: &tempfile::TempDir, limit: Duration) -> LimitedBuild {
    super::skull_build_package_with_limit(dir.path().to_str().unwrap(), limit, 1, None)
}

#[track_caller]
fn failures_after(result: LimitedBuild, passed: usize) -> Vec<String> {
    match result {
        Ok(super::SkullOutcome {
            passed: actual,
            fails,
            ..
        }) if actual == passed && !fails.is_empty() => fails,
        other => panic!("expected {passed} passes and failures: {other:?}"),
    }
}

#[test]
fn test_limit_bounds_program_loading() {
    let start = bone_core::prelude::Instant::now();
    let result = super::run_emitted_tests(
        "while true do end".to_string(),
        &[],
        &[],
        Duration::from_millis(100),
        1,
        None,
        false,
    );
    assert!(start.elapsed() < Duration::from_millis(100) + Duration::from_secs(30));
    let error = result.expect_err("loading must time out");
    assert!(
        error
            .rendered
            .contains("loading emitted program: timed out"),
        "{error:?}"
    );
}

fn add_tests(dir: &tempfile::TempDir, tests: &[(&str, &str)]) {
    let file = dir.path().join("first.rock");
    let mut source = std::fs::read_to_string(&file).unwrap();
    for (name, body) in tests {
        source.push_str(&format!("@test inline(lua) fn {name}() = \"{body}\"\n"));
    }
    std::fs::write(file, source).unwrap();
}

#[test]
fn test_limit_reruns_the_tests_after_a_stopped_one() {
    let dir = file_limit_package("os.execute('sleep 0.1')", None);
    add_tests(
        &dir,
        &[
            ("hung_test", "while true do end"),
            ("crashing_test", "os.exit(3)"),
            ("exiting_test", "os.exit(0)"),
            ("last_test", "os.execute('sleep 0.1')"),
        ],
    );
    let result = build_with_limit(&dir, Duration::from_secs(1));
    let fails = failures_after(result, 2);
    assert_eq!(fails.len(), 3, "{fails:?}");
    assert!(
        fails[0]
            .rsplit("::")
            .next()
            .unwrap()
            .starts_with("hung_test: ")
            && fails[0].contains("first.rock")
            && fails[0].contains("CPU-time budget")
            && fails[0].contains("while it ran"),
        "{fails:?}"
    );
    assert!(
        fails[1]
            .rsplit("::")
            .next()
            .unwrap()
            .starts_with("crashing_test: ")
            && fails[1].contains("child process stopped while it ran"),
        "{fails:?}"
    );
    assert!(
        fails[2]
            .rsplit("::")
            .next()
            .unwrap()
            .starts_with("exiting_test: ")
            && fails[2].contains("process exited 0 before all tests reported while it ran"),
        "{fails:?}"
    );
}

#[test]
fn marker_shaped_stdout_cannot_report_a_test_that_exits() {
    let dir = file_limit_package(
        "assert(io.stdout:write('__SKULL__', string.char(9), 'PASS', string.char(9,10))); os.exit(0)",
        None,
    );
    let fails = failures_after(build_with_limit(&dir, Duration::from_secs(10)), 0);
    assert_eq!(fails.len(), 1);
    assert!(fails[0].contains("process exited 0 before all tests reported while it ran"));
}

#[test]
fn test_limit_bounds_the_reruns_of_a_shard() {
    let dir = file_limit_package("while true do end", None);
    let hung = (0..super::SHARD_RESTARTS + 1)
        .map(|i| format!("hung_test_{i}"))
        .collect::<Vec<_>>();
    add_tests(
        &dir,
        &hung
            .iter()
            .map(|name| (name.as_str(), "while true do end"))
            .collect::<Vec<_>>(),
    );
    let result = build_with_limit(&dir, Duration::from_millis(200));
    match result {
        Ok(super::SkullOutcome {
            passed: 0,
            fails,
            diagnostics,
            ..
        }) => {
            assert_eq!(fails.len(), super::SHARD_RESTARTS + 2, "{fails:?}");
            let charged = fails
                .iter()
                .filter(|f| f.contains("CPU-time budget") && f.contains("while it ran"))
                .count();
            assert_eq!(
                charged,
                super::SHARD_RESTARTS + 1,
                "one stopped test per process: {fails:?}"
            );
            assert_eq!(
                diagnostics.len(),
                super::SHARD_RESTARTS + 1,
                "{diagnostics:?}"
            );
            let last = format!("hung_test_{}: never reported", super::SHARD_RESTARTS);
            assert!(
                fails
                    .iter()
                    .any(|f| f.rsplit("::").next().unwrap_or(f).starts_with(&last)
                        && f.contains("stopped before it ran")),
                "{fails:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[cfg(unix)]
fn wait_for_test_watchdog_fixture(
    script: &str,
    wall_limit: Duration,
) -> (crate::common::ProcessResult, LuaTestSequence) {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watchdog fixture");
    let mut sequence = LuaTestSequence::new(2);
    let run = crate::common::ProcessManager::wait_with_test_watchdog(
        &mut child,
        crate::common::WaitPolicy::Cpu {
            cpu_limit: Duration::from_millis(100),
            wall_limit,
        },
        "watch-token".into(),
        &mut sequence,
    )
    .expect("wait for watchdog fixture");
    // Child::try_wait can reap the child itself; ECHILD proves the watchdog did.
    let waited = unsafe {
        libc::waitpid(
            child.id() as libc::pid_t,
            std::ptr::null_mut(),
            libc::WNOHANG,
        )
    };
    let error = std::io::Error::last_os_error();
    assert_eq!(waited, -1, "watchdog must reap its child: {run:?}");
    assert_eq!(error.raw_os_error(), Some(libc::ECHILD), "{error}");
    (run, sequence)
}

#[cfg(unix)]
#[test]
fn watchdog_replays_only_complete_ordered_terminal_rows() {
    use crate::test_protocol::{LuaTestResult, LuaTestRow};
    use std::os::unix::process::ExitStatusExt;

    let prefix = r#"
printf 'ROCK_TEST\tforged\t0\tstart\n' >&2
printf 'ROCK_TEST\twatch-token\t1\tstart\n' >&2
printf 'ROCK_TEST\twatch-token\t0\t1\tpass\t\n' >&2
printf 'ROCK_TEST\twatch-token\t0\tstart\n' >&2
printf 'ROCK_TEST\twatch-token\t0\tstart\n' >&2
printf 'ROCK_TEST\twatch-token\t0\tnot-ms\tpass\t\n' >&2
printf 'ROCK_TEST\twatch-token\t0\t12.' >&2
printf '500\tvalue\t42\n' >&2
printf 'ROCK_TEST\twatch-token\t0\t1\tfail\trepeated\n' >&2
"#;
    for (suffix, code, signal, active) in [
        ("exit 0", Some(0), None, None),
        ("kill -TERM $$", None, Some(libc::SIGTERM), None),
        (
            r"printf 'ROCK_TEST\twatch-token\t1\tstart\nROCK_TEST\twatch-token\t1\t1\tpass\t' >&2; kill -TERM $$",
            None,
            Some(libc::SIGTERM),
            Some(1),
        ),
    ] {
        let (run, sequence) =
            wait_for_test_watchdog_fixture(&format!("{prefix}\n{suffix}"), Duration::from_secs(10));
        assert!(!run.timed_out, "{run:?}");
        assert_eq!(run.output.status.code(), code);
        assert_eq!(run.output.status.signal(), signal);
        assert_eq!(sequence.running(), active);
        assert_eq!(
            sequence.finished,
            [LuaTestRow {
                ms: 12.5,
                result: LuaTestResult::Value("42".into())
            }],
        );
    }
}

#[cfg(unix)]
#[test]
fn plain_luajit_without_runtime_layout() {
    const CHILD: &str = "ROCK_PLAIN_LUAJIT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let file = std::env::current_dir().unwrap().join("plain.rock");
        super::skull_test(&[file.to_string_lossy().into_owned()], None, None).unwrap();
        return;
    }
    let luajit = std::process::Command::new("sh")
        .args(["-c", "command -v luajit"])
        .output()
        .unwrap();
    assert!(luajit.status.success());
    let luajit = std::fs::canonicalize(String::from_utf8(luajit.stdout).unwrap().trim()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(&luajit, bin.join("luajit")).unwrap();
    std::fs::write(
        root.path().join("plain.rock"),
        "@test inline(lua) fn plain_child() = \"print('plain LuaJIT ran')\"\n",
    )
    .unwrap();
    let mut failures = Vec::new();
    for use_override in [false, true] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "skull_backend::tests::plain_luajit_without_runtime_layout",
                "--nocapture",
            ])
            .current_dir(root.path())
            .env(CHILD, "1")
            .env("ROCK_DIR", root.path())
            .env("PATH", &bin)
            .env_remove("ROCK_LUAJIT")
            .env_remove("STONE_HEADLESS")
            .env_remove("ROCK_TEST_FILTER");
        if use_override {
            command.env("ROCK_LUAJIT", &luajit);
        }
        let output = command.output().unwrap();
        let text = test_utils::strip_ansi_sgr(&format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
        if !output.status.success() || !text.contains("1/1 @tests passed") {
            failures.push(format!(
                "plain runtime (override={use_override}) failed:\n{text}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(test)]
mod watch_route_tests {

    use super::super::SkullReload;
    use super::{ExecOutcome, ProjectFixture, exec_lua};

    fn run(program: &str) -> String {
        match exec_lua(program) {
            ExecOutcome::Output(output) => output,
            ExecOutcome::Errored(error) => panic!("{error}\n--- program ---\n{program}"),
        }
    }

    #[test]
    fn a_callee_body_edit_reaches_its_bound_caller() {
        let v0 = "global SCORE: int = 0\n\
                  fn leaf(a: int) -> int { a + 1 }\n\
                  @nomangle fn bump() { SCORE = leaf(SCORE) }\n";
        let mut fx = ProjectFixture::boot(v0);
        let before = fx.main_text();
        assert!(before.contains("__rock_mk_bump"), "bump binds its callee");
        let reload = fx.edit(&v0.replace("a + 1", "a + 10")).unwrap();
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "{reload:?}"
        );
        let script = format!(
            "_G.HOT_RELOAD = false\n{before}\nbump()\n\
             _G.HOT_RELOAD = true\n{}\n_G.HOT_RELOAD = false\nbump()\nprint(SCORE)\n",
            fx.overlay_text()
        );
        assert_eq!(run(&script).trim(), "11");
    }

    #[test]
    fn a_caller_edit_keeps_the_abi_of_an_unchanged_cross_module_generic_callback() {
        let caller = "global SCORE: int = 0\n\
                      fn apply<T>(value: T, callback: fn(T) -> T) -> T { callback(value) }\n\
                      @nomangle fn bump() { SCORE = SCORE + 1 }\n\
                      @nomangle fn invoke() -> int { apply(7, unchanged) }\n";
        let mut fx = ProjectFixture::boot_files(&[
            ("stick.toml", "[package]\nname = \"callbackwatch\"\n"),
            ("caller.rock", caller),
            (
                "callback.rock",
                "fn unchanged(value: int) -> int { value + 3 }\n",
            ),
        ]);
        let before = fx.main_text();
        let reload = fx
            .edit_files(&[(
                "caller.rock",
                &caller.replace("apply(7, unchanged)", "apply(17, unchanged)"),
            )])
            .unwrap();
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "{reload:?}"
        );
        let after = fx.overlay_text();
        let script = format!(
            "_G.HOT_RELOAD = false\n{before}\nassert(invoke() == 10)\nbump()\n\
             _G.HOT_RELOAD = true\n{after}\n_G.HOT_RELOAD = false\n\
             assert(invoke() == 20)\nbump()\nassert(SCORE == 2)\nprint('PASS')\n"
        );
        assert_eq!(run(&script).trim(), "PASS");
    }

    #[test]
    fn a_global_initializer_embed_is_watched_and_rebuilt() {
        let mut fx = ProjectFixture::boot_files(&[
            ("stick.toml", "[package]\nname = \"globalembedwatch\"\n"),
            (
                "main.rock",
                "const BANNER: string = embed_string(\"banner.txt\")\nfn main() { print(BANNER) }\n",
            ),
            ("banner.txt", "global-embed-v1"),
        ]);
        let banner = fx.path("banner.txt").canonicalize().unwrap();
        assert!(
            fx.project
                .comptime_watch_files_with_hashes()
                .iter()
                .any(|(path, _)| path == &banner)
        );
        let reload = fx.edit_files(&[("banner.txt", "global-embed-v2")]).unwrap();
        assert!(matches!(reload, SkullReload::Full { .. }), "{reload:?}");
        let main = fx.main_text();
        assert!(main.contains("global-embed-v2") && !main.contains("global-embed-v1"));
    }

    #[test]
    fn stone_reload_clear_keeps_the_lifecycle_plan_and_empties_sims() {
        let manifest = "[package]\nname = \"deco_cleanup_test\"\ntype = \"bin\"\n\
                        [language]\nmodules = \"v1\"\n\
                        [dependencies]\nstone = { package = \"stone\" }\nstone-core = { package = \"stone-core\" }\n\
                        [build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n\
                        [build.vars]\nGIT_SHA = \"test\"\nBUILD_UNIX = \"0\"\nBUILD_DATE = \"test\"\n\
                        CHANNEL = { builtin = \"channel\" }\nPLATFORM = { builtin = \"platform\" }\n";
        let source = "use stone::*;\nuse stone_core::*;\n\
                      @shutdown\nfn cleanup() {}\n\
                      @sim\nco fn dummy_sim() -> Coroutine<Wait> {\n  yield wait_frame()\n}\n\
                      fn main() {}\n";
        let fx = ProjectFixture::boot_files(&[("stick.toml", manifest), ("main.rock", source)]);
        let text = fx.main_text();
        let clears: Vec<&str> = text
            .lines()
            .filter_map(|line| {
                line.split_once(" = __rock_mk_")
                    .map(|(name, _)| name)
                    .or_else(|| {
                        line.strip_prefix("function ")
                            .and_then(|rest| rest.split_once('('))
                            .map(|(name, _)| name)
                    })
            })
            .filter(|name| name.starts_with("clear_all_systems_") && !name.contains("__"))
            .collect();
        assert_eq!(clears.len(), 1, "{clears:?}");
        // HOT_RELOAD skips the entry wrapper and with it the native runtime.
        let script = format!(
            "local function load() _G.HOT_RELOAD = true dofile({path:?}) _G.HOT_RELOAD = false end\n\
             local function sims() local n = 0 for _ in pairs(_G.SIMS) do n = n + 1 end return n end\n\
             local function shutdowns() return _G.__ROCK_LIFECYCLE_PLAN.shutdown[0] end\n\
             load()\n\
             local plan = shutdowns()\n\
             assert(plan >= 1, 'the shutdown plan holds the handler')\n\
             assert(sims() == 1, 'one sim after load')\n\
             {clear}()\n\
             assert(shutdowns() == plan, 'clear keeps the plan')\n\
             assert(sims() == 0, 'clear empties SIMS')\n\
             load()\n\
             assert(shutdowns() == plan, 'reload keeps one plan')\n\
             assert(sims() == 1, 'reload registers the sim once')\n\
             print('PASS')\n",
            path = fx.main.display().to_string(),
            clear = clears[0],
        );
        // Stone's program is too large for the piped-stdin runner.
        let script_path = fx.path("reload_clear.lua");
        std::fs::write(&script_path, script).unwrap();
        let output = std::process::Command::new(rock_engine_host::runtime::luajit_path())
            .arg(&script_path)
            .env("LUA_PATH", rock_engine_host::runtime::lua_path_value())
            .env(
                rock_engine_host::runtime::lib_path_var(),
                rock_engine_host::runtime::lib_path_with_inherited(),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "PASS",
            "{output:?}"
        );
    }
}

#[test]
fn comment_only_closure_edit_does_not_publish_an_overlay() {
    let source = "global KEEP: fn() -> int = () => 0\n\
                  fn install(x: int) {\n    KEEP = () => x + 1\n}\n\
                  fn main() { install(10); print(KEEP()) }";
    let mut fixture = ProjectFixture::boot(source);
    let before = fixture.main_text();
    let above = format!("// Initialize the retained callback\n\n{source}");
    let commented = above.replace("    KEEP", "    // Preserve the capture\n\n    KEEP");
    for text in [&above, &commented] {
        assert!(matches!(
            fixture.edit(text).unwrap(),
            super::SkullReload::NoChange
        ));
    }
    assert_eq!(fixture.main_text(), before);
    assert!(matches!(
        fixture.edit(&commented.replace("x + 1", "x + 20")).unwrap(),
        super::SkullReload::Incremental { .. }
    ));
}
