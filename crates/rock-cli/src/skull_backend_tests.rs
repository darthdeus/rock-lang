use crate::compile_cmd;
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn source_fingerprint_support_files_follow_successful_live_compiles() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stick.toml"),
        "[package]\nname = \"fingerprint-watch\"\n[build.vars]\nREV = { source_fingerprint = { roots = [\"Root\"], files = [\"support.txt\"] } }\n").unwrap();
    std::fs::write(
        dir.path().join("main.rock"),
        "value Root { count: int }\nfn main() { print(build_var(\"REV\")) }\n",
    )
    .unwrap();
    let support = dir.path().join("support.txt");
    std::fs::write(&support, "first decoder").unwrap();
    let support = support.canonicalize().unwrap();
    let main = dir.path().join("main.lua");
    let overlay = dir.path().join("incremental.lua");
    let mut project =
        super::IncrementalProject::init(dir.path().to_str().unwrap(), &main, &overlay).unwrap();
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
    let first_hash = recorded_hash(&project);
    let first_revision = revision(&project);
    std::fs::write(&support, "second decoder").unwrap();
    assert_eq!(
        recorded_hash(&project),
        first_hash,
        "watch seeding must describe compiled bytes, not reread current disk"
    );
    assert!(matches!(
        project
            .recompile(std::slice::from_ref(&support), &main, &overlay)
            .unwrap(),
        super::SkullReload::Full { .. }
    ));
    let second_hash = recorded_hash(&project);
    let second_revision = revision(&project);
    assert_ne!(first_hash, second_hash);
    assert_ne!(first_revision, second_revision);
    std::fs::remove_file(&support).unwrap();
    assert!(
        project
            .recompile(std::slice::from_ref(&support), &main, &overlay)
            .is_err()
    );
    assert_eq!(recorded_hash(&project), second_hash);
    assert_eq!(revision(&project), second_revision);
    let manifest = dir.path().join("stick.toml");
    std::fs::write(
        &manifest,
        "[package]\nname = \"fingerprint-watch\"\n[build.vars]\nREV = \"fixed\"\n",
    )
    .unwrap();
    project.recompile(&[manifest], &main, &overlay).unwrap();
    assert!(
        project
            .comptime_watch_files_with_hashes()
            .iter()
            .all(|(path, _)| path != &support)
    );
    assert_eq!(revision(&project), "fixed");
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

/// A project whose only source is `main.rock`, compiled once at boot.
struct ProjectFixture {
    dir: tempfile::TempDir,
    root: String,
    project: super::IncrementalProject,
    main: std::path::PathBuf,
    incremental: std::path::PathBuf,
    source: std::path::PathBuf,
}

impl ProjectFixture {
    fn boot(source_text: &str) -> Self {
        Self::try_boot(source_text).expect("initial build")
    }

    fn try_boot(source_text: &str) -> anyhow::Result<Self> {
        Self::try_boot_with_manifest(None, source_text)
    }

    fn try_boot_with_manifest(manifest: Option<&str>, source_text: &str) -> anyhow::Result<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(manifest) = manifest {
            std::fs::write(dir.path().join("stick.toml"), manifest).expect("stick.toml");
        }
        let root = dir.path().to_str().expect("tempdir path").to_string();
        let main = dir.path().join("main.lua");
        let incremental = dir.path().join("incremental.lua");
        let source = dir.path().join("main.rock");
        std::fs::write(&source, source_text).expect("initial source");
        let project = super::IncrementalProject::init(&root, &main, &incremental)?;
        Ok(Self {
            dir,
            root,
            project,
            main,
            incremental,
            source,
        })
    }

    /// Replace `main.rock` and recompile it through the production route.
    fn edit(&mut self, text: &str) -> anyhow::Result<super::SkullReload> {
        std::fs::write(&self.source, text).expect("edited source");
        self.project.recompile(
            std::slice::from_ref(&self.source),
            &self.main,
            &self.incremental,
        )
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

fn session_file(path: &str, text: &str) -> bone_incremental::session::SessionFile {
    bone_incremental::session::SessionFile {
        path: path.to_string(),
        text: text.to_string(),
        package: Some(skull_parser::PackageTag {
            id: 1,
            self_name: "@/<backend-test>".to_string(),
            identity_storage: skull_parser::IdentityStorage::Exempt,
            root: String::new(),
        }),
        global_policy: skull_parser::GlobalPolicy::Any,
    }
}

fn files(src: &str) -> Vec<bone_incremental::session::SessionFile> {
    vec![session_file("main.rock", src)]
}

#[cfg(test)]
mod overlay_runtime_tests {
    //! Compare replaying the two-file overlay with a fresh compile.

    use super::super::{IncrementalProject, SkullReload};
    use super::files;
    use super::{ExecOutcome, exec_lua, normalize_stdout};
    use crate::bone_backend::bone_session;
    use std::path::Path;

    use super::overlay_test_support::{SYS_STUB, run_parts, stub_for};

    /// Execute the files written by `IncrementalProject`, including the game entry
    /// wrapper. The two small host hooks make this the same Lua artifact the
    /// engine consumes while keeping the test independent of a renderer.
    fn run_project_artifacts(
        main_path: &Path,
        incremental_path: &Path,
        call_after_overlay: bool,
    ) -> String {
        let main = std::fs::read_to_string(main_path).expect("read generated main.lua");
        let overlay = std::fs::read_to_string(incremental_path).unwrap_or_default();
        let tail = if call_after_overlay {
            "\nmain()\n"
        } else {
            "\n"
        };
        let program = format!(
            "function clear_all_systems() end\nfunction __rock_main_wrapper(f) f() end\n{main}\n{overlay}{tail}"
        );
        match exec_lua(&program) {
            ExecOutcome::Output(output) => normalize_stdout(&output),
            ExecOutcome::Errored(error) => panic!(
                "generated IncrementalProject artifacts failed to execute:\n{error}\n--- main.lua ---\n{main}\n--- incremental.lua ---\n{overlay}"
            ),
        }
    }

    fn run_production_body_overlay(manifest: Option<&str>) -> String {
        let v0 = "fn probe() -> int { static value = 1\n value = value + 1\n value }\nfn main() { print(probe()) }\n";
        let v1 = "fn probe() -> int { static value = 1\n value = value + 10\n value }\nfn main() { print(probe()) }\n";
        let mut fx = super::ProjectFixture::try_boot_with_manifest(manifest, v0)
            .expect("initial production build");
        assert!(
            !fx.incremental.exists(),
            "initial load must not leave an overlay"
        );
        let reload = fx.edit(v1).expect("body edit through production recompile");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "body edit must publish an overlay, got {reload:?}"
        );
        assert!(
            fx.incremental.exists(),
            "body edit must write incremental.lua"
        );

        // The first call is made by main.lua's production entry wrapper. The
        // second call happens after the overlay is loaded, so the static cell
        // proves both changed code and retained state.
        run_project_artifacts(&fx.main, &fx.incremental, true)
    }

    #[test]
    fn production_overlay_executes_v1_body_edit_and_preserves_state() {
        let legacy = run_production_body_overlay(None);
        assert_eq!(
            legacy.trim(),
            "2\n12",
            "legacy control must execute the overlay"
        );

        let v1 = run_production_body_overlay(Some(
            "[package]\nname = \"overlay-v1\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
        ));
        assert_eq!(
            v1.trim(),
            "2\n12",
            "v1 overlay must execute changed code with retained state"
        );
    }

    /// A program build defines only what its entry reaches, so an edit that
    /// starts calling an own function nothing called before must define it
    /// in the published artifacts.
    #[test]
    fn production_reload_defines_an_own_function_an_edit_starts_calling() {
        let mut fx = super::ProjectFixture::boot(
            "fn later(x: int) -> int { return x * 3 + 2 }\nfn main() { print(0) }\n",
        );
        assert!(
            !fx.main_text().contains("later"),
            "the unreached function must not be defined at boot"
        );
        fx.edit("fn later(x: int) -> int { return x * 3 + 2 }\nfn main() { print(later(4)) }\n")
            .expect("recompile after the edit");
        let out = run_project_artifacts(&fx.main, &fx.incremental, true);
        assert_eq!(out.trim().lines().last(), Some("14"), "{out}");
    }

    #[test]
    fn production_v1_namespace_edit_takes_full_reload_and_runs_new_definition() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("stick.toml"),
            "[package]\nname = \"namespace-v1\"\ntype = \"bin\"\n\n[language]\nmodules = \"v1\"\n",
        )
        .expect("stick.toml");
        let main_path = dir.path().join("main.lua");
        let incremental_path = dir.path().join("incremental.lua");
        let main_source = dir.path().join("main.rock");
        let module_source = dir.path().join("a.rock");
        std::fs::write(&main_source, "fn main() { print(a::bump(2)) }\n")
            .expect("initial main source");
        std::fs::write(
            &module_source,
            "mod a;\npub fn bump(n: int) -> int { n + 1 }\n",
        )
        .expect("initial module source");
        let mut project = IncrementalProject::init(
            dir.path().to_str().expect("tempdir path"),
            &main_path,
            &incremental_path,
        )
        .expect("initial v1 namespace build");
        let baseline = run_project_artifacts(&main_path, &incremental_path, false);
        assert_eq!(baseline.trim(), "3");

        std::fs::write(&main_source, "fn main() { print(a::twice(3)) }\n")
            .expect("edited main source");
        std::fs::write(
            &module_source,
            "mod a;\npub fn bump(n: int) -> int { n + 1 }\npub fn twice(n: int) -> int { n * 2 }\n",
        )
        .expect("edited module source");
        let reload = project
            .recompile(
                &[module_source.clone(), main_source.clone()],
                &main_path,
                &incremental_path,
            )
            .expect("namespace edit through production recompile");
        assert!(
            matches!(reload, SkullReload::Full { .. }),
            "a namespace-affecting edit must publish the supported full path, got {reload:?}"
        );
        assert!(
            !incremental_path.exists(),
            "full namespace reload must wipe the overlay"
        );
        let updated = run_project_artifacts(&main_path, &incremental_path, false);
        assert_eq!(
            updated.trim(),
            "6",
            "the full artifact must contain the new namespace definition"
        );
        assert_ne!(
            updated.trim(),
            baseline.trim(),
            "namespace edit must not serve stale output"
        );
    }

    /// Reads a yielded event in the fixtures below.
    const SCORE4: &str = "fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }\n}\n";
    const SCORE2: &str = "fn score(e: Ev) -> int {\n  return match e {\n    Ev::Tick(x) => x\n    Ev::Act(_) => -1\n  }\n}\n";

    macro_rules! pump_source {
        () => {
            "@nomangle\nfn pump() {\n  let c = GEN\n  if c != null {\n    let e = c.resume()\n    if e != null { print(\"{score(e)}\") } else { print(\"-1\") }\n  }\n}\n"
        };
    }
    /// Host hooks: `start()` makes the coroutine, `pump()` resumes and prints it.
    const START_PUMP: &str = concat!("@nomangle\nfn start() { GEN = gen() }\n", pump_source!());
    const OUTER_START_PUMP: &str =
        concat!("@nomangle\nfn start() { GEN = outer() }\n", pump_source!());

    /// A Session warmed at v0: `baseline` is the frozen `main.lua`, and `cache`
    /// accumulates across overlays exactly as `IncrementalProject`'s does.
    struct Live {
        session: bone_incremental::session::Session,
        mid: bone_semantic::prelude::ModuleId,
        baseline: String,
        cache: skull::mir_emit::SkullEmitCache,
    }

    impl Live {
        fn boot(v0: &str) -> Self {
            let session = bone_session(files(v0)).expect("session v0");
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

        /// Apply an edit whose premise is that it stays in the body-only tier.
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

        /// Apply an edit that must leave the body-only tier; the rebuilt program.
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

    /// Load `program` with its stub, then run `steps` and compare stdout.
    fn assert_run(program: &str, steps: &[&str], expected: &str, ctx: &str) {
        let mut parts = vec![program, stub_for(program)];
        parts.extend_from_slice(steps);
        assert_stdout(run_parts(&parts), expected, ctx);
    }

    // A body-only reload can first construct a previously unused value type;
    // its constructor must already exist in the frozen full baseline.
    #[test]
    fn overlay_referencing_baseline_value_ctor_runtime_eq() {
        // An opaque Lua parameter requires boxing; equality and field reads can
        // compare scalar leaves without ever invoking the constructor.
        let decl = "value P { x: int, y: int }\nglobal INPUT: int = 5\n\
                    inline(lua) fn ctor_sum(p: P) -> int = \"return p.x + p.y\"\n";
        let v0 = format!(
            "{decl}fn compute() -> int {{ 1 }}\nfn main() {{ print(\"{{compute()}}\") }}\n"
        );
        let v1 = format!(
            "{decl}fn compute() -> int {{ ctor_sum(P {{ x: INPUT, y: INPUT + 1 }}) }}\nfn main() {{ print(\"{{compute()}}\") }}\n"
        );

        let mut live = Live::boot(&v0);

        let overlay = live.overlay(&v1);
        let fresh = live.full();

        assert_run(
            &live.baseline,
            &[&overlay, "main()"],
            "11",
            "overlay value constructor",
        );
        assert_run(&fresh, &["main()"], "11", "fresh value constructor build");
    }

    /// A minimal game-shaped module: one `@update` system whose body writes
    /// `tick_val` to an observable global, plus the engine's deco/registry/clear
    /// machinery and a frame runner. `run_frame()` drives the registry exactly as
    /// the engine's `run_update_systems` does (iterate, call `system.func()`).
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

    /// Run the registry-driving frame and report the observable global.
    const SIM_DRIVER: &str = "run_frame() print(TICK_OUT)";

    /// An existing lifecycle entry must invoke the edited body through its
    /// retained CodeRef. The overlay contains no decorator rebind call.
    #[test]
    fn sim_overlay_refreshes_existing_code_ref_registry_entry() {
        let v0 = sim_fixture(1);
        let v1 = sim_fixture(2);

        let mut live = Live::boot(&v0);

        // Baseline: the registry runs the v0 body.
        assert_run(
            &live.baseline,
            &[SIM_DRIVER],
            "1",
            "baseline frame should run the v0 system body",
        );

        let result = live.body_edit(&v1);
        let overlay = live.emit(&result);
        let fresh = live.full();

        assert_run(
            &fresh,
            &[SIM_DRIVER],
            "2",
            "fresh full build should run the v1 body",
        );
        assert_run(
            &live.baseline,
            &[&overlay, SIM_DRIVER],
            "2",
            "the retained registry CodeRef must run the EDITED decorated body",
        );
    }

    /// Slot publication belongs to the OUTER successful overlay boundary. A
    /// later top-level initializer failure must leave an already-stored value
    /// on v0, even though the overlay has staged the edited callback body.
    /// The mutually recursive edge keeps the direct call as a global lookup.
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

    /// A failed initializer restores compiler-owned constants and newly emitted
    /// specializations even when the program has no escaping function values.
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
  for i = 1, phase.len do
    local entry = phase._data[i]
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

    /// A `co fn` BODY edit, delivered through a REAL `Session.update_file`, must
    /// re-emit the coroutine into the overlay so a fresh invocation runs the new
    /// body -- exactly like an ordinary fn. The corpus sweep can't catch this: it
    /// fabricates `reinferred` from every function directly, bypassing the Session
    /// (where the fast-path body reparse slices the item from `fn`, dropping `co`).
    /// This drives the genuine edit pipeline end to end.
    #[test]
    fn coroutine_body_edit_overlay_runs_new_body() {
        let v0 = "co fn counter() -> Coroutine<int> {\n  yield 1\n  yield 2\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";
        let v1 = "co fn counter() -> Coroutine<int> {\n  yield 10\n  yield 20\n}\n\
                  fn main() {\n  let c = counter()\n  let a = c.resume() ?? -1\n  let b = c.resume() ?? -1\n  print(\"{a} {b}\")\n}\n";

        let mut live = Live::boot(v0);
        assert_run(&live.baseline, &["main()"], "1 2", "baseline co fn body");

        let result = live.body_edit(v1);
        let overlay = live.emit(&result);
        let fresh = live.full();
        assert_run(
            &fresh,
            &["main()"],
            "10 20",
            "fresh full build runs the v1 co fn body",
        );
        assert_run(
            &live.baseline,
            &[&overlay, "main()"],
            "10 20",
            "overlay must run the EDITED coroutine body, not the stale one",
        );
    }

    /// A FLAT-ENUM-yielding `co fn` body edit, through the overlay, must re-emit the
    /// frame-slot yield channel and CodeRefs for the ctor's `__advance`/`__resume`
    /// fields, then a freshly-created coroutine must run the new body, threading the
    /// yield through the slots -- no `{__variant}` box on the concrete consumer path.
    /// Guards the frame-slot channel against a hot-reload regression; the scalar-yield
    /// test above does not exercise the channel (int yields keep the multi-return
    /// path).
    #[test]
    fn flat_enum_coroutine_body_edit_overlay_threads_and_runs_new_body() {
        // The `Act(() -> ())` closure payload makes Ev NON-cdata-eligible, so it is a
        // genuinely FLAT (boxed) enum that rides the frame-slot channel -- an
        // all-scalar-payload enum would scalarize as a `_ct` cdata instead (mirrors
        // `tests/coroutines/enum_thread_yield.rock`).
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Pair({a}, {b})\n  yield Ev::Done\n}}\n\
                 {SCORE4}\
                 fn main() {{\n  let c = gen()\n  let e0 = c.resume()\n  let e1 = c.resume()\n  let s0 = if e0 != null {{ score(e0) }} else {{ -1 }}\n  let s1 = if e1 != null {{ score(e1) }} else {{ -1 }}\n  print(\"{{s0}} {{s1}}\")\n}}\n"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut live = Live::boot(&v0);
        assert_run(
            &live.baseline,
            &["main()"],
            "1 3",
            "baseline flat-enum co fn (Tick(1)=1, Pair(1,2)=3)",
        );

        let overlay = live.overlay(&v1);
        let fresh = live.full();
        assert_run(
            &fresh,
            &["main()"],
            "10 30",
            "fresh v1: Tick(10)=10, Pair(10,20)=30",
        );
        assert_run(
            &live.baseline,
            &[&overlay, "main()"],
            "10 30",
            "overlay must run the EDITED flat-enum body via the re-emitted frame-slot channel",
        );
    }

    /// The load-bearing hot-reload case: a flat-enum coroutine created + first-resumed
    /// under v0, then resumed again ACROSS a body-edit overlay on the SAME live object
    /// (the engine's per-frame poll pattern). The live object keeps its v0
    /// `__advance`/`__resume` closures, but `__advance` calls `gen_0__step` by GLOBAL
    /// name, so a mid-flight resume runs the NEW step -- writing the new yield leaves
    /// into the SAME reused `__y*` slots on the object/frame (the enum shape is
    /// unchanged), which the (baseline, unchanged) `pump` reads directly. If
    /// `__advance` had captured the step by value, resume #2 would run the stale v0
    /// body (Pair(1,2)=3, not Pair(10,20)=30).
    #[test]
    fn live_flat_enum_coroutine_survives_overlay_and_threads_new_body() {
        // `Act(() -> ())` forces Ev to be a FLAT (boxed) enum on the frame-slot
        // channel (an all-scalar-payload enum would be a `_ct` cdata instead).
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Pair({a}, {b})\n  yield Ev::Done\n}}\n\
                 {SCORE4}\
                 {START_PUMP}"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut live = Live::boot(&v0);

        let overlay = live.overlay(&v1);

        // start() + pump() under v0 (Tick(1)=1), then overlay, then pump() x3 on the
        // SAME live coroutine under v1: Pair(10,20)=30, Done=100, then done -> -1.
        assert_run(
            &live.baseline,
            &["start()", "pump()", &overlay, "pump()", "pump()", "pump()"],
            "1\n30\n100\n-1",
            "live coroutine: v0 first yield (Tick=1), then v1 body across the reload (Pair=30, Done=100, done)",
        );
    }

    const EXPECT_LAYOUT_ERROR: &str = "local ok, err = pcall(pump); assert(not ok and tostring(err):find('frame layout changed', 1, true)); pump()";

    /// A new yield point renumbers coroutine states, so a suspended v0 frame
    /// must be killed cleanly when the edited step runs.
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

        let mut live = Live::boot(&v0);

        let overlay = live.overlay(&v1);

        // v0: pump() -> Tick(1)=1, suspended at state 1 (before print("fire")).
        // Overlay renumbers: the new Ev::Done yield becomes state 1, so the live
        // frame's __state=1 would resume at the WRONG yield point (pre-stamp it
        // replayed the already-delivered Tick(1), then ran fire/cleanup a pump
        // late). The per-state variant tags in the layout stamp catch the
        // renumbering: resume #2 stamp-kills the coroutine -> -1, no replay.
        assert_run(
            &live.baseline,
            &[
                "start()",
                "pump()",
                &overlay,
                EXPECT_LAYOUT_ERROR,
                "pump()",
                "pump()",
            ],
            "1\n-1\n-1\n-1",
            "a yield-renumbering overlay must stamp-kill the live coroutine, not teleport its resume point",
        );
    }

    /// `await` under hot reload: a live outer+inner delegation chain must pick up
    /// a BodyOnly edit to the INNER co fn's body. The chain is advance-by-field
    /// (outer holds the inner object) + step-by-global-name (inner's stale advance
    /// resolves `inner_0__step` at call time), and both mechanisms must compose
    /// across the delegation boundary. Guards the Part B kill/advance/delegate
    /// refactor.
    #[test]
    fn live_yield_star_delegation_survives_inner_body_overlay() {
        let prog = |a: i32, b: i32| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn inner() -> Coroutine<Ev> {{\n  yield Ev::Tick({a})\n  yield Ev::Tick({b})\n}}\n\
                 co fn outer() -> Coroutine<Ev> {{\n  await inner()\n}}\n\
                 {SCORE2}\
                 {OUTER_START_PUMP}"
            )
        };
        let (v0, v1) = (prog(1, 2), prog(10, 20));

        let mut live = Live::boot(&v0);

        let overlay = live.overlay(&v1);

        // pump #1 under v0: outer primes inner, re-yields inner's Tick(1). Overlay
        // (layout-stable: constants only). pump #2 resumes the live chain: the
        // inner's stale advance runs the NEW inner_0__step -> Tick(20). pump #3:
        // inner done -> outer's delegate exits, outer completes -> -1.
        assert_run(
            &live.baseline,
            &["start()", "pump()", &overlay, "pump()", "pump()"],
            "1\n20\n-1",
            "a live await chain must thread a BodyOnly inner edit (v0 Tick(1), then v1 Tick(20), then done)",
        );
    }

    /// A layout cancellation of an outer coroutine suspended mid-`await`
    /// must chain into the delegated inner: explicit `kill()` recurses through
    /// `__inner` (firing the inner's on_killed), and layout cancellation must match --
    /// otherwise the inner is orphaned and its cleanup hook never runs.
    #[test]
    fn stamp_killed_outer_chains_kill_into_delegated_inner() {
        let prog = |extra: &str| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 global INNER_KILLED: int = 0\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 fn note_inner_killed() {{ INNER_KILLED = INNER_KILLED + 1 }}\n\
                 co fn inner() -> Coroutine<Ev> {{\n  on_killed(() => note_inner_killed())\n  yield Ev::Tick(1)\n  yield Ev::Tick(2)\n}}\n\
                 co fn outer() -> Coroutine<Ev> {{\n{extra}  await inner()\n}}\n\
                 {SCORE2}\
                 {OUTER_START_PUMP}\
                 @nomangle\n\
                 fn report() {{ print(\"{{INNER_KILLED}}\") }}\n"
            )
        };
        let (v0, v1) = (prog(""), prog("  let z = mk(9)\n"));

        let mut live = Live::boot(&v0);
        let overlay = live.overlay(&v1);

        // pump #1: outer primes inner -> Tick(1). Overlay shifts OUTER's layout.
        // pump #2: outer stamp-killed -> -1, and the inner's on_killed must fire.
        assert_run(
            &live.baseline,
            &[
                "start()",
                "pump()",
                &overlay,
                EXPECT_LAYOUT_ERROR,
                "report()",
            ],
            "1\n-1\n1",
            "a stamp-killed outer must chain-kill its delegated inner (inner's on_killed fires exactly once)",
        );
    }

    /// `gen` must be rejected naming `needle`. Emission collects per-fn skips
    /// rather than failing the whole unit, so either surface is accepted, but
    /// the co fn must not emit.
    fn assert_co_fn_rejected(src: &str, needle: &str) {
        let session = bone_session(files(src)).expect("session");
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

    /// A coroutine hook (`on_killed`/`on_resume`/`on_hot_reload`) argument must
    /// be a lambda literal: the hook is peeled to the constructor, which can
    /// only re-render a lambda with the frame context. Any other fn-typed
    /// expression used to SILENTLY install no hook --
    /// it must be a compile error instead.
    #[test]
    fn non_lambda_coroutine_hook_arg_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   fn cleanup() { print(\"x\") }\n\
                   co fn gen() -> Coroutine<Ev> {\n  on_killed(cleanup)\n  yield Ev::Tick(1)\n}\n\
                   fn main() { let c = gen() }\n";
        assert_co_fn_rejected(src, "lambda");
    }

    /// `yield` inside a `defer` body is a compile error (a defer body cannot
    /// suspend -- it may run during terminal cleanup with no resume protocol).
    #[test]
    fn yield_inside_defer_body_rejected() {
        let src = "enum Ev { Tick(int), Act(() -> ()) }\n\
                   co fn gen() -> Coroutine<Ev> {\n  defer { yield Ev::Tick(1) }\n  yield Ev::Tick(2)\n}\n\
                   fn main() { let c = gen() }\n";
        assert_co_fn_rejected(src, "defer");
    }

    /// Layout drift via the Interface tier (which does NOT protect live
    /// coroutines): a full rebuild is a re-`dofile`, not a restart -- mutable
    /// globals carry the `_G.HOT_RELOAD` preservation guard, so a live coroutine
    /// held in a global SURVIVES the full reload, and its stale `__advance`
    /// resolves the step by global name into the NEW program. A realistic
    /// Interface edit (widen a variant + use the new payload, which also inserts
    /// a local) would then drive the old frame with a step compiled for a
    /// different layout -- the stamp kills the survivor cleanly instead.
    #[test]
    fn live_coroutine_survives_full_reload_and_shape_edit_stamp_kills() {
        let prog = |enum_tick: &str, yield_tick: &str, score_tick: &str| {
            format!(
                "enum Ev {{ {enum_tick}, Pair(int, int), Done, Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 fn mk(n: int) -> int {{ return n }}\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let a = mk(7)\n  let b = mk(35)\n  yield {yield_tick}\n  yield Ev::Pair(a, b)\n  yield Ev::Done\n}}\n\
                 fn score(e: Ev) -> int {{\n  return match e {{\n    {score_tick}\n    Ev::Pair(p, q) => p + q\n    Ev::Done => 100\n    Ev::Act(_) => -1\n  }}\n}}\n\
                 {START_PUMP}"
            )
        };
        // Widen Tick BEYOND the current max payload width (Pair's 2 -> Tick's 3),
        // so the frame-slot yield channel genuinely grows -- the shape edit shifts
        // the layout stamp (a within-width widen would be absorbed by the union
        // channel and, post-P2-fix, is correctly layout-stable). The enum edit is
        // Interface tier, delivered via a FULL rebuild.
        let v0 = prog("Tick(int)", "Ev::Tick(a)", "Ev::Tick(x) => x");
        let v1 = prog(
            "Tick(int, int, int)",
            "Ev::Tick(a, b, a)",
            "Ev::Tick(x, y, z) => x + y + z",
        );

        let mut live = Live::boot(&v0);

        let full1 = live.full_reload(&v1);

        // Premise (non-inverted, stays green): the live coroutine SURVIVES the full
        // re-dofile -- the mutable-global preservation guard keeps GEN.
        assert_run(
            &live.baseline,
            &[
                "start()",
                "pump()",
                "_G.HOT_RELOAD = true",
                &full1,
                "print(_G.GEN ~= nil)",
            ],
            "7\ntrue",
            "a live coroutine in a mutable global must survive a full re-dofile (HOT_RELOAD guard)",
        );

        // Resuming the survivor: the v1 step's layout stamp (yield channel
        // widened by the Tick payload growing past the union width) rejects the
        // v0 frame. The error reaches the caller and a later pump prints -1.
        // Pre-stamp this crashed the consumer on the shifted frame
        // read, through the very tier the hot-reload contract once claimed was safe.
        assert_run(
            &live.baseline,
            &[
                "start()",
                "pump()",
                "_G.HOT_RELOAD = true",
                &full1,
                "print(_G.GEN ~= nil)",
                EXPECT_LAYOUT_ERROR,
            ],
            "7\ntrue\n-1",
            "a shape-edit full reload must stamp-kill the surviving live coroutine, not corrupt it",
        );
    }

    /// Equal type names can hide incompatible layouts in a live LuaJIT process.
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

        // Survival premise: the live v0 instance rides through the full
        // re-dofile untouched -- `"Mob?"` didn't change, so the guard skips
        // the re-init and the old-shape table persists.
        assert_run(
            &live.baseline,
            &[
                "spawn()",
                "describe()",
                "_G.HOT_RELOAD = true",
                &full1,
                "print(_G.M ~= nil and _G.M.armor == nil)",
            ],
            "10\ntrue",
            "a live struct instance must survive the full re-dofile WITHOUT the added field",
        );

        // Corruption: the v1 reader assumes `armor: int` on an instance that
        // never had it -- `m.hp + m.armor` is arithmetic on nil.
        let composed = run_parts(&[
            &live.baseline,
            stub_for(&live.baseline),
            "spawn()",
            "describe()",
            "_G.HOT_RELOAD = true",
            &full1,
            "describe()",
        ]);
        // Assert the SPECIFIC failure: any-error-is-success would also pass on a
        // timeout or an unrelated load error, which is how a pin like this rots
        // into proving nothing.
        let diag = composed
            .expect_errored("expected the recompiled reader to die on the nil-valued added field");
        assert!(
            diag.contains("arithmetic on") && diag.contains("nil"),
            "expected nil-arithmetic on the field the live instance never had, got:\n{diag}"
        );
    }

    /// A new cdef cannot replace a layout already loaded by LuaJIT.
    #[test]
    fn value_struct_reorder_full_reload_is_caught_by_the_layout_check() {
        let v0 = "value P { x: int, y: int }\n\
                  global CUR: P = P { x: 0, y: 0 }\n\
                  @nomangle\n\
                  fn store_p() { CUR = P { x: 1, y: 2 } }\n\
                  @nomangle\n\
                  fn show() { print(\"{CUR.x},{CUR.y}\") }\n\
                  fn main() { }\n";
        let v1 = "value P { y: int, x: int }\n\
                  global CUR: P = P { x: 0, y: 0 }\n\
                  @nomangle\n\
                  fn store_p() { CUR = P { x: 1, y: 2 } }\n\
                  @nomangle\n\
                  fn show() { print(\"{CUR.x},{CUR.y}\") }\n\
                  fn main() { }\n";

        let mut live = Live::boot(v0);

        let full1 = live.full_reload(v1);

        // v0 alone: sanity -- set/show agree on the layout.
        assert_run(
            &live.baseline,
            &["store_p()", "show()"],
            "1,2",
            "value-struct v0 fixture",
        );

        // Composed: the v1 chunk re-cdefs P (silent no-op), so LuaJIT keeps the
        // v0 layout. Left alone this misassigns silently -- the v1 positional
        // ctor (y first) feeds the frozen v0 ctype (x first) and `show()` prints
        // the SWAP, `2,1`, with no error anywhere.
        //
        // It no longer gets that far. The emitted leaf-layout check
        // (`value_leaf_layout_checks`) compares skull's computed offsets against
        // `ffi.offsetof` at chunk load, sees v1's `y@0` against the frozen
        // `y@4`, and aborts. The restart policy is still the PRIMARY response --
        // `struct_shape_hash` hashes fields in order, so the real driver drifts
        // and respawns cold long before a chunk like this loads (this fixture
        // composes the two chunks by hand, bypassing that gate). The check is
        // the loud backstop for anything that ever slips past it, which is what
        // makes this class detectable rather than silent.
        let composed = run_parts(&[
            &live.baseline,
            stub_for(&live.baseline),
            "store_p()",
            "show()",
            "_G.HOT_RELOAD = true",
            &full1,
            "store_p()",
            "show()",
        ]);
        // Assert the SPECIFIC diagnostic: any-error-is-success would also pass on
        // an unrelated load failure or a timeout, which is how a test like this
        // rots into proving nothing.
        let diag = composed.expect_errored(
            "a reordered value struct reloaded into a frozen ctype must be caught by \
             the leaf-layout check, not silently misassigned",
        );
        assert!(
            diag.contains("skull layout drift") && diag.contains("P.y"),
            "expected the leaf-layout drift diagnostic naming the moved field, got:\n{diag}"
        );
    }

    /// An edited `@on_reload` callback must refresh through its retained CodeRef,
    /// and the callback registration must survive a second overlay and fire.
    #[test]
    fn on_reload_code_ref_refreshes_and_refires_across_overlays() {
        let prog = |step: i32, unrelated: i32| {
            format!(
                "global FIRE_COUNT: int = 0\n\
                 @nomangle\n\
                 @on_reload\n\
                 fn bump_fire_count() {{ FIRE_COUNT = FIRE_COUNT + {step} }}\n\
                 fn unrelated() -> int {{ return {unrelated} }}\n\
                 @nomangle\n\
                 fn report() {{ print(\"{{FIRE_COUNT}}\") }}\n"
            )
        };
        let (v0, v1, v2) = (prog(1, 1), prog(2, 2), prog(2, 3));

        let mut live = Live::boot(&v0);

        let overlay1 = live.overlay(&v1);
        let overlay2 = live.overlay(&v2);

        // The first edit changes the registered callback's body; the second only
        // changes an unrelated body. Both fires must run the refreshed callback.
        assert_run(
            &live.baseline,
            &[
                &overlay1,
                "__rock_fire_hot_reload_callbacks()",
                "report()",
                &overlay2,
                "__rock_fire_hot_reload_callbacks()",
                "report()",
            ],
            "2\n4",
            "the refreshed @on_reload callback must persist and fire once per overlay",
        );
    }

    /// A coroutine created after a body edit must run the new capturing lambda.
    #[test]
    fn cofn_capturing_lambda_body_edit_overlay_created_after_reload() {
        let prog = |op: char| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 3\n  let f = (x: int) => x {op} base\n  yield Ev::Tick(f(4))\n}}\n\
                 {SCORE2}\
                 {START_PUMP}"
            )
        };
        let (v0, v1) = (prog('+'), prog('*'));

        let mut live = Live::boot(&v0);

        let overlay = live.overlay(&v1);
        // Created AFTER the overlay -> the v1 lambda (4*3=12), then done -> -1.
        assert_run(
            &live.baseline,
            &[&overlay, "start()", "pump()", "pump()"],
            "12\n-1",
            "a coroutine created after the reload runs the new hoisted lambda body (4*3=12)",
        );
    }

    /// A LIVE coroutine created BEFORE the reload survives a layout-stable body
    /// edit, and its stored capturing record resolves the edited body through
    /// the same logical CodeRef as a record created afterwards.
    #[test]
    fn cofn_lambda_record_snapshot_refreshes_across_reload() {
        let prog = |op: char| {
            format!(
                "enum Ev {{ Tick(int), Act(() -> ()) }}\n\
                 global GEN: Coroutine<Ev>? = null\n\
                 co fn gen() -> Coroutine<Ev> {{\n  let base = 5\n  let f = (x: int) => x {op} base\n  yield Ev::Tick(f(3))\n  yield Ev::Tick(f(3))\n  let g = (x: int) => x {op} base\n  yield Ev::Tick(g(3))\n}}\n\
                 {SCORE2}\
                 {START_PUMP}"
            )
        };
        let (v0, v1) = (prog('+'), prog('*'));

        let mut live = Live::boot(&v0);
        let overlay = live.overlay(&v1);
        // start() + pump() under v0: f(3)=8. Overlay (op *). The same stored
        // record now resolves lambda0's edited body, then g uses lambda1.
        let fresh = Live::boot(&v1).baseline;
        let fresh_output = "15\n15\n15\n-1";
        assert_run(
            &fresh,
            &["start()", "pump()", "pump()", "pump()", "pump()"],
            fresh_output,
            "fresh v1 lambda records",
        );
        assert_run(
            &live.baseline,
            &["start()", "pump()", &overlay, "pump()", "pump()", "pump()"],
            "8\n15\n15\n-1",
            "a retained lambda record must refresh after the overlay and match fresh v1 yields",
        );
    }

    #[test]
    fn named_and_capture_free_values_refresh_through_reusable_slots() {
        let program = |named_delta: i32, free_value: i32| {
            format!(
                "global NAMED: (fn(int) -> int)? = null\n\
                 global FREE: (fn() -> int)? = null\n\
                 @nomangle fn named(x: int) -> int {{ return x + {named_delta} }}\n\
                 @nomangle fn make_free() -> fn() -> int {{ return () => {free_value} }}\n\
                 @nomangle fn start() {{\n\
                   NAMED = named\n\
                   FREE = make_free()\n\
                 }}\n\
                 @nomangle fn invoke() {{\n\
                   let a = NAMED\n\
                   let b = FREE\n\
                   if a != null {{ if b != null {{ print(\"{{a(3)}} {{b()}}\") }} }}\n\
                 }}\n"
            )
        };
        let v0 = program(1, 10);
        let v1 = program(2, 20);
        let mut live = Live::boot(&v0);
        assert_run(
            &live.baseline,
            &["start()", "invoke()"],
            "4 10",
            "baseline named/capture-free values",
        );

        let overlay = live.overlay(&v1);
        assert_run(
            &live.baseline,
            &["start()", &overlay, "invoke()"],
            "5 20",
            "composed slot values",
        );
        assert_run(
            &live.full(),
            &["start()", "invoke()"],
            "5 20",
            "fresh slot values",
        );
    }
}
#[cfg(test)]
mod shape_drift_tests {

    use super::super::SkullReload;
    use super::ProjectFixture;

    #[test]
    fn struct_shape_edit_requests_restart_and_defers_the_write() {
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
        let mut fx = ProjectFixture::boot(v0);
        let baseline = fx.main_text();

        let reload = fx.edit(v1).expect("recompile v1");
        match &reload {
            SkullReload::ShapeChanged { types } => assert!(
                types.iter().any(|t| t == "Mob"),
                "the restart request must name the drifted type, got {types:?}"
            ),
            other => {
                panic!("a struct shape edit must request a restart (ShapeChanged), got {other:?}")
            }
        }
        assert_eq!(
            fx.main_text(),
            baseline,
            "the new-shape main.lua write must be DEFERRED -- the old process must never dofile it"
        );

        // The restart path canonicalizes: force_full writes the new program.
        fx.force_full().expect("force_full");
        assert_ne!(
            fx.main_text(),
            baseline,
            "canonicalize after the kill must write the deferred new-shape program"
        );

        // Re-baselined: recompiling the same source again is a no-op, not a
        // second restart request.
        let again = fx.edit(v1).expect("recompile same");
        assert!(
            matches!(again, SkullReload::NoChange),
            "the shape delta must be consumed by the restart, got {again:?}"
        );
    }

    /// Layout edits that freeze against the running process: marking a `value`
    /// type `nobox` moves its globals from one boxed slot to leaf globals while
    /// the rendered type (all the preservation guard compares) stays the same,
    /// and a value-struct field reorder changes the process ctype.
    #[test]
    fn value_layout_edits_request_restart() {
        for (v0, v1, ty) in [
            (
                "value NV2 { x: float, y: float }\n\
                 global SPOT: NV2 = NV2 { x: 1.0, y: 2.0 }\nfn main() { }\n",
                "nobox value NV2 { x: float, y: float }\n\
                 global SPOT: NV2 = NV2 { x: 1.0, y: 2.0 }\nfn main() { }\n",
                "NV2",
            ),
            (
                "value P { x: int, y: int }\nfn probe(p: P) -> int { p.x }\nfn main() { }\n",
                "value P { y: int, x: int }\nfn probe(p: P) -> int { p.x }\nfn main() { }\n",
                "P",
            ),
        ] {
            let mut fx = ProjectFixture::boot(v0);
            let reload = fx.edit(v1).expect("recompile v1");
            assert!(
                matches!(&reload, SkullReload::ShapeChanged { types } if types.iter().any(|t| t == ty)),
                "{ty}: a layout edit must restart, got {reload:?}"
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
    fn body_and_signature_edits_stay_on_the_hot_paths() {
        let v0 = "struct Mob { hp: int }\n\
                  fn heal(m: Mob) -> int { m.hp }\n\
                  fn main() { }\n";
        let body = "struct Mob { hp: int }\n\
                    fn heal(m: Mob) -> int { m.hp + 1 }\n\
                    fn main() { }\n";
        let sig = "struct Mob { hp: int }\n\
                   fn heal(m: Mob, bonus: int) -> int { m.hp + bonus }\n\
                   fn main() { }\n";
        let mut fx = ProjectFixture::boot(v0);

        let reload = fx.edit(body).expect("body edit");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "a body edit must stay a hot overlay, got {reload:?}"
        );

        let reload = fx.edit(sig).expect("sig edit");
        assert!(
            matches!(reload, SkullReload::Full { .. }),
            "a signature-only edit carries no data-shape drift -- full reload, NOT a restart, got {reload:?}"
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
            fx.project.source_text(&fx.source).as_deref(),
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

    /// init at v1 -> body-edit to v2 (writes the overlay) -> force_full. The
    /// canonical `main.lua` must equal a fresh full build of v2, must DIFFER from
    /// the v1 baseline (proving the edit was actually folded in, not served
    /// stale), and `incremental.lua` must be wiped.
    #[test]
    fn force_full_folds_overlay_matches_fresh_and_wipes() {
        // Needs a `main`: emission is reachability-driven, so a fixture with no
        // entry point at all emits no user code and the two builds would compare
        // equal for a reason that has nothing to do with the fold.
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\nfn main() { print(\"{use_it()}\") }\n";
        let v2 = "fn helper() -> int { 5 }\nfn use_it() -> int { helper() }\nfn main() { print(\"{use_it()}\") }\n";

        // init at v1: writes main.lua(v1), wipes any overlay.
        let mut fx = ProjectFixture::boot(v1);
        let main_v1 = fx.main_text();
        assert!(!fx.incremental.exists(), "init must leave no overlay");

        // Body edit to v2: a BodyOnly recompile writes the overlay; main.lua
        // stays the v1 baseline. Passing the changed file exercises the fast path
        // (re-read only `main.rock`); if it failed to pick up v2, the byte-identity
        // check against a fresh v2 build below would catch it.
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

        // force_full: fold the overlay back into main.lua, wipe the overlay.
        fx.force_full().expect("force_full");
        let main_forced = fx.main_text();
        assert!(!fx.incremental.exists(), "force_full must wipe the overlay");
        assert_ne!(
            main_forced, main_v1,
            "force_full must fold the v2 edit into main.lua, not serve the stale v1 baseline"
        );

        // Ground truth: a from-scratch full build of v2 (separate output path,
        // same on-disk source). force_full's main.lua must match it byte-for-byte.
        let fresh_main = fx.dir.path().join("fresh_main.lua");
        let fresh_inc = fx.dir.path().join("fresh_incremental.lua");
        let fresh =
            IncrementalProject::init(&fx.root, &fresh_main, &fresh_inc).expect("fresh init v2");
        let main_fresh = std::fs::read_to_string(&fresh_main).expect("fresh main.lua");
        assert_eq!(
            main_forced, main_fresh,
            "force_full main.lua must be byte-identical to a fresh full build of the edited source"
        );
        assert_eq!(
            fx.project.artifact_fingerprint(),
            fresh.artifact_fingerprint(),
            "force_full must publish the fingerprint of the Artifact it writes"
        );
    }

    /// A project with a type error (a call to a non-existent function) must make
    /// `init` FAIL, not silently emit a partial `main.lua`. The bug: emit_full
    /// drops the un-checkable body but keeps its `__deco_init` registration, so
    /// the registered `func` is a nil global -> a baffling runtime nil-call
    /// instead of the clean compile error. `recompile` gates on
    /// `session.diagnostics()`; `init` must too.
    #[test]
    fn init_rejects_project_with_type_error() {
        // `boot` calls a function that does not exist -> bone collects a
        // diagnostic but Session::new still succeeds, so without the gate
        // emit_full produces a partial program.
        let dir = tempfile::tempdir().expect("tempdir");
        let main_lua = dir.path().join("main.lua");
        std::fs::write(
            dir.path().join("main.rock"),
            "fn boot() { does_not_exist() }\n",
        )
        .unwrap();

        let res = IncrementalProject::init(
            dir.path().to_str().unwrap(),
            &main_lua,
            &dir.path().join("incremental.lua"),
        );
        assert!(
            res.is_err(),
            "init must reject a project whose body references a non-existent function"
        );
        assert!(
            !main_lua.exists(),
            "init must NOT write a partial main.lua when the project has type errors"
        );
    }

    /// A recompile that hits a type error must retain the last-good compiler
    /// Session as well as the last-good written artifact. The restart fold can
    /// therefore proceed against the retained program instead of discovering a
    /// broken intermediate state.
    #[test]
    fn failed_recompile_retains_session_and_last_good_artifact() {
        // init at a valid v1 -> main.lua(v1).
        let v1 = "fn helper() -> int { 3 }\nfn use_it() -> int { helper() }\n";
        let mut fx = ProjectFixture::boot(v1);
        let main_v1 = fx.main_text();

        // Edit to a broken v2 (calls a non-existent function). recompile bails
        // and must restore the v1 compiler Session before returning.
        let v2 = "fn helper() -> int { 3 }\nfn use_it() -> int { nope() }\n";
        let reload = fx.edit(v2);
        assert!(reload.is_err(), "recompile must bail on the broken edit");

        // A fold after the failed update now sees the retained v1 Session and
        // succeeds without changing the last-good artifact.
        let res = fx.force_full();
        assert!(
            res.is_ok(),
            "force_full must retain the previous good Session"
        );
        assert_eq!(
            fx.main_text(),
            main_v1,
            "a failed recompile must leave the last-good main.lua untouched"
        );
    }
}

#[cfg(test)]
mod incremental_lint_snapshot_tests {
    use super::super::SkullReload;
    use super::overlay_test_support::{run_parts, stub_for};
    use super::{ExecOutcome, ProjectFixture};

    #[test]
    fn failed_body_publication_rolls_back_cache_before_the_next_edit() {
        let v1 =
            "fn helper() -> int { 3 }\nfn other() -> int { 10 }\nfn main() { print(helper()) }\n";
        let v2 =
            "fn helper() -> int { 5 }\nfn other() -> int { 10 }\nfn main() { print(helper()) }\n";
        let v3 =
            "fn helper() -> int { 3 }\nfn other() -> int { 11 }\nfn main() { print(other()) }\n";
        let mut fx = ProjectFixture::boot(v1);
        if fx.incremental.is_file() {
            std::fs::remove_file(&fx.incremental).expect("initial overlay");
        }
        std::fs::create_dir(&fx.incremental).expect("blocked overlay");

        assert!(
            fx.project
                .apply_edit(&fx.source, v2, &fx.main, &fx.incremental)
                .is_err(),
            "a body publication to a directory must fail"
        );
        assert_eq!(fx.project.source_text(&fx.source).as_deref(), Some(v1));

        std::fs::remove_dir(&fx.incremental).expect("unblock overlay");
        let reload = fx
            .project
            .apply_edit(&fx.source, v3, &fx.main, &fx.incremental)
            .expect("next body edit");
        assert!(
            matches!(reload, SkullReload::Incremental { .. }),
            "reload: {reload:?}"
        );
        let main = fx.main_text();
        let output = run_parts(&[&main, stub_for(&main), &fx.overlay_text(), "main()"]);
        match output {
            ExecOutcome::Output(output) => {
                assert_eq!(super::normalize_stdout(&output).trim(), "3\n11")
            }
            ExecOutcome::Errored(error) => panic!("recovered overlay failed: {error}"),
        }
    }
}

#[cfg(test)]
mod value_test_exec {
    //! `@test(returns = <literal>)` end-to-end on the luajit executor: the
    //! harness emits a VALUE marker, the host compares
    //! (`test_protocol::compare_value`). Discovery rejections (the
    //! returns+expect_failure contradiction) must fail the unit, never route
    //! through the expect-compile-failure escape.

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
    fn returns_plus_expect_failure_is_a_unit_failure_not_expected_pass() {
        let src = "@test(expect_failure = true, returns = 42)\nfn contra() -> int { return 42 }\n";
        let err =
            skull_build("val.rock", src, None).expect_err("contradiction must reject the unit");
        assert!(
            err.reason.contains("contradictory"),
            "unexpected reason: {}",
            err.reason
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
    use crate::unified_compile::discover_project_sources;

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

        let sources = discover_project_sources(dir.path()).expect("discover timed project");
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
        let sources = discover_project_sources(&project).unwrap();
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

type LimitedBuild = Result<super::SkullOutcome, super::SkullFailure>;

fn build_with_limit(dir: &tempfile::TempDir, limit: Duration) -> LimitedBuild {
    super::skull_build_package_with_limit(dir.path().to_str().unwrap(), limit, 1, None)
}

/// The failure list of a run that must have failed with exactly `passed` passes.
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
    // The program never finishes loading; the bound only leaves room for a
    // loaded host.
    assert!(start.elapsed() < Duration::from_millis(100) + Duration::from_secs(30));
    let error = result.expect_err("loading must time out");
    assert!(
        error
            .rendered
            .contains("loading emitted program: timed out"),
        "{error:?}"
    );
}

#[test]
fn test_limit_resets_for_each_test() {
    // Healthy tests whose sum exceeds the limit: the limit bounds one test's
    // inactivity, not the file's total time.
    let dir = file_limit_package("os.execute('sleep 0.3')", None);
    add_tests(
        &dir,
        &[
            ("second_test_in_file", "os.execute('sleep 0.3')"),
            ("third_test_in_file", "os.execute('sleep 0.3')"),
            ("fourth_test_in_file", "os.execute('sleep 0.3')"),
        ],
    );
    let result = build_with_limit(&dir, Duration::from_secs(1));
    assert!(
        matches!(&result, Ok(run) if run.passed == 4 && run.fails.is_empty()),
        "{result:?}"
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
    // A hung test, a crashing one and an early exit each stop their process;
    // the tests after them still run, and only the three that stopped fail,
    // by name.
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
fn test_limit_bounds_the_reruns_of_a_shard() {
    // A defect that hangs every test costs a bounded number of processes; the
    // tests after the last one are still named.
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
    policy: crate::common::WaitPolicy,
) -> crate::common::ProcessResult {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watchdog fixture");
    let run = crate::common::ProcessManager::wait_with_policy_and_tests::<()>(
        &mut child,
        policy,
        false,
        false,
        Some(crate::common::TestWatch {
            token: "watch-token".to_string(),
            tests: 2,
        }),
        None,
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
    run
}

#[cfg(unix)]
#[test]
fn test_limit_kills_a_blocked_test_at_the_wall_backstop() {
    let script = r#"
printf 'ROCK_TEST_START\twatch-token\t0\n' >&2
exec sleep 600
"#;
    let run = wait_for_test_watchdog_fixture(
        script,
        crate::common::WaitPolicy::Test {
            cpu_limit: Duration::from_millis(100),
            wall_limit: Duration::from_secs(1),
        },
    );
    assert!(
        run.timed_out,
        "a blocked child must eventually be bounded: {run:?}"
    );
    assert_eq!(
        run.timeout_phase,
        Some(crate::common::TimeoutPhase::TestWall)
    );
    assert_eq!(run.running_test, Some(0));
}

#[test]
fn package_compile_holds_a_compile_slot_only_while_compiling() {
    let slots = tempfile::tempdir().expect("slot dir");
    let package = tempfile::tempdir().expect("package");
    std::fs::write(
        package.path().join("stick.toml"),
        "[package]\nname = \"slot-probe\"\ntype = \"lib\"\n",
    )
    .unwrap();
    // A held slot is a `held-*` file in the slot directory until released: the
    // test passes only if the compile released its slot before its tests ran.
    std::fs::write(
        package.path().join("probe.rock"),
        format!(
            "@test inline(lua) fn slot_is_free_during_tests() = \"local ls = io.popen('ls -A {}'); \
         local names = ls:read('*a'); ls:close(); assert(names:find('queue'), 'list slots'); \
         assert(not names:find('held%-'), 'compile slot held while tests run')\"\n",
            slots.path().display()
        ),
    )
    .unwrap();
    let package = package.path().to_str().unwrap().to_owned();
    let result = crate::compile_slots::assert_compile_waits_for_slot_in(slots.path(), move || {
        super::skull_build_package_with_limit(&package, Duration::from_secs(10), 1, None)
    });
    assert!(
        matches!(&result, Ok(run) if run.passed == 1 && run.fails.is_empty()),
        "{result:?}"
    );
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
