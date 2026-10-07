//! End-to-end tests of stone engine's hot-reload cleanup contract.
//!
//! Spawns a real `rock stone-test` against a minimal stone-using project,
//! reads the on-disk compiled main.lua, then runs a hot-reload simulation
//! under LuaJIT with HOT_RELOAD=true to skip `__rock_main_wrapper` (and
//! therefore stone's runtime/SDL init). The compiler-owned lifecycle plan is
//! published before that wrapper and remains the only system population the
//! reload path consumes.

use std::path::Path;
use std::process::Command;
use std::time::Duration;
use crate::stone_harness::StoneHarness;

/// Run LuaJIT against a script written to a tempfile. The piped-stdin path
/// (`rock_host::lua_runner::run_lua_code`) deadlocks for large scripts (~50k lines of stone
/// engine output), so this version spawns `luajit <path>` with output
/// captured.
fn run_lua_file(script: &str) -> std::io::Result<std::process::Output> {
    let tmp = std::env::temp_dir().join(format!(
        "rock_stone_test_{}_{}.lua",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::write(&tmp, script)?;
    let luajit = rock_host::project::luajit_path();
    Command::new(luajit)
        .arg(&tmp)
        .env("LUA_PATH", rock_host::project::lua_path_value())
        .output()
}

const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Path to the workspace's `stone/` directory, used as the dependency
/// in seed `stick.toml`. Resolves at test runtime so the seed isn't
/// pinned to one developer's checkout layout.
fn stone_path() -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")); // .../crates/rock-cli
    let workspace = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    workspace
        .join("stone")
        .canonicalize()
        .expect("stone/ exists")
        .display()
        .to_string()
}

fn make_seed(main_rock: &str) -> Vec<(String, String)> {
    let stick = format!(
        "[package]\n\
         name = \"deco_cleanup_test\"\n\
         version = \"0.1.0\"\n\
         type = \"bin\"\n\n\
         [dependencies]\n\
         stone = {{ path = \"{}\" }}\n",
        stone_path(),
    );
    let stick = format!(
        "{stick}\n\n[build]\nchannels = [\"dev\"]\ndefault_channel = \"dev\"\n\n\
         [build.vars]\n\
         GIT_SHA = {{ value = \"stone-test\" }}\n\
         BUILD_UNIX = {{ value = \"0\" }}\n\
         BUILD_DATE = {{ value = \"stone-test\" }}\n\
         CHANNEL = {{ builtin = \"channel\" }}\n\
         PLATFORM = {{ builtin = \"platform\" }}\n"
    );
    vec![
        ("stick.toml".to_string(), stick),
        ("main.rock".to_string(), main_rock.to_string()),
    ]
}

/// The emitted global of Stone's `clear_all_systems`, read from the program the
/// driver loads: its mangling carries Stone's module digest under modules v1.
fn clear_all_systems_symbol(main_lua: &str) -> String {
    let text = std::fs::read_to_string(main_lua).expect("read compiled main.lua");
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            // Either callee binding: `name = __rock_mk_name(...)` or `function name(`.
            line.split_once(" = __rock_mk_")
                .map(|(name, _)| name)
                .or_else(|| line.strip_prefix("function ").and_then(|rest| rest.split_once('(')).map(|(name, _)| name))
        })
        .filter(|name| name.starts_with("clear_all_systems_") && !name.contains("__"))
        .collect();
    assert_eq!(names.len(), 1, "one emitted clear_all_systems: {names:?}");
    names[0].to_string()
}

/// Pin the lifecycle-plan behavior across a clear and reload: the immutable
/// compiler-owned shutdown population remains one plan, rather than a mutable
/// decorator registry that can accumulate duplicate entries.
#[test]
fn shutdown_systems_clear_contract_via_real_engine() {
    let seed_owned = make_seed(
        "@shutdown\n\
         fn cleanup() {\n\
             // body intentionally trivial — only the registration matters\n\
         }\n\n\
         fn main() {\n\
             // never runs in test mode (HOT_RELOAD=true skips the wrapper)\n\
         }\n",
    );
    // Materialize references for StoneHarness::spawn signature.
    let seed_refs: Vec<(&str, &str)> = seed_owned
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();

    let h = StoneHarness::spawn(&seed_refs);
    h.await_reload_count(1, SETTLE_TIMEOUT);
    let main_lua_path = h.compiled_lua_path();
    let main_lua_path = main_lua_path.display().to_string();

    // Simulate two loads with `clear_all_systems()` between them, the
    // shape `do_hot_reload()` uses. HOT_RELOAD=true on the first load
    // so __rock_main_wrapper (which would call stone_run_game and need
    // libstone.dylib) short-circuits. The staged lifecycle plan publishes
    // before the wrapper and is not rebuilt by a mutable registry clear.
    //
    // We `dofile` the harness's on-disk main.lua rather than embedding
    // its text — embedding via `[==[...]==]` collides with the engine's
    // own long-string literals.
    let script = format!(
        "local main_lua = {path:?}\n\
         _G.HOT_RELOAD = true\n\
         dofile(main_lua)\n\
         _G.HOT_RELOAD = false\n\n\
         -- Phase 1: post-initial-load\n\
         local shutdown_plan = _G.__ROCK_LIFECYCLE_PLAN.shutdown\n\
         local len_after_load = shutdown_plan and shutdown_plan.len or 0\n\
         if len_after_load < 1 then\n\
             print(\"FAIL: post-load lifecycle shutdown plan len = \" .. tostring(len_after_load) .. \", expected at least the seed's own handler\")\n\
             os.exit(1)\n\
         end\n\n\
         -- Phase 2: engine's pre-dofile mutable-registry clear\n\
         {clear}()\n\
         if _G.__ROCK_LIFECYCLE_PLAN.shutdown.len ~= len_after_load then\n\
             print(\"FAIL: post-clear lifecycle shutdown plan len = \" .. tostring(_G.__ROCK_LIFECYCLE_PLAN.shutdown.len) .. \", expected \" .. tostring(len_after_load) .. \"\")\n\
             os.exit(1)\n\
         end\n\n\
         -- Phase 3: hot reload re-runs decorator_init_code\n\
         _G.HOT_RELOAD = true\n\
         dofile(main_lua)\n\
         _G.HOT_RELOAD = false\n\n\
         -- Phase 4: back to one plan's worth, not a duplicated registry\n\
         if _G.__ROCK_LIFECYCLE_PLAN.shutdown.len ~= len_after_load then\n\
             print(\"FAIL: post-reload lifecycle shutdown plan len = \" .. tostring(_G.__ROCK_LIFECYCLE_PLAN.shutdown.len) .. \", expected \" .. tostring(len_after_load) .. \"\")\n\
             os.exit(1)\n\
         end\n\
         print(\"PASS\")\n",
        path = main_lua_path,
        clear = clear_all_systems_symbol(&main_lua_path),
    );

    let _ = std::fs::write("/tmp/stone_decorator_cleanup_test.lua", &script);
    let output = run_lua_file(&script).expect("luajit spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.trim().contains("PASS"),
        "shutdown systems cleanup test failed:\nstatus: {:?}\nstdout: {}\nstderr: {}",
        output.status, stdout, stderr,
    );
}

/// Pin the post-fix behavior of `clear_all_systems()` for SIMS: after one
/// load + clear + one reload, the SIMS map holds exactly one entry, not
/// two. Sibling to the SHUTDOWN_SYSTEMS test above; covers the second
/// codegen-driven `decorator_init_code` contributor (sim_registrations.rs
/// — `@sim` annotations emit `register_sim("name", body)` at module init).
/// Without the `SIMS.clear()` line in `clear_all_systems()`, the same key
/// gets re-inserted on reload — masking the leak for `Map` keyed by sim
/// name, but still a real leak for any change that renames the sim
/// (orphans the old key forever).
#[test]
fn sims_clear_contract_via_real_engine() {
    let seed_owned = make_seed(
        "@sim\n\
         co fn dummy_sim() -> Coroutine<Wait> {\n\
             yield wait_frame()\n\
         }\n\n\
         fn main() {\n\
             // never runs in test mode (HOT_RELOAD=true skips the wrapper)\n\
         }\n",
    );
    let seed_refs: Vec<(&str, &str)> = seed_owned
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();

    let h = StoneHarness::spawn(&seed_refs);
    h.await_reload_count(1, SETTLE_TIMEOUT);
    let main_lua_path = h.compiled_lua_path();
    let main_lua_path = main_lua_path.display().to_string();

    // SIMS is `Map<string, () -> Coroutine<Wait>>` which compiles down
    // to a plain Lua table (no `.len` field — Map's `.len()` UFCS walks
    // the table). From Lua we count entries directly via `pairs`.
    let script = format!(
        "local main_lua = {path:?}\n\
         local function map_count(t)\n\
             local n = 0\n\
             for _ in pairs(t) do n = n + 1 end\n\
             return n\n\
         end\n\n\
         _G.HOT_RELOAD = true\n\
         dofile(main_lua)\n\
         _G.HOT_RELOAD = false\n\n\
         -- Phase 1: post-initial-load\n\
         local n1 = map_count(_G.SIMS)\n\
         if n1 ~= 1 then\n\
             print(\"FAIL: post-load SIMS count = \" .. tostring(n1) .. \", expected 1\")\n\
             os.exit(1)\n\
         end\n\n\
         -- Phase 2: engine's pre-dofile clear\n\
         {clear}()\n\
         local n2 = map_count(_G.SIMS)\n\
         if n2 ~= 0 then\n\
             print(\"FAIL: post-clear SIMS count = \" .. tostring(n2) .. \", expected 0 (clear must wipe SIMS — without SIMS.clear() this entry survives)\")\n\
             os.exit(1)\n\
         end\n\n\
         -- Phase 3: hot reload re-runs decorator_init_code\n\
         _G.HOT_RELOAD = true\n\
         dofile(main_lua)\n\
         _G.HOT_RELOAD = false\n\n\
         -- Phase 4: should be 1 again, not 2\n\
         local n3 = map_count(_G.SIMS)\n\
         if n3 ~= 1 then\n\
             print(\"FAIL: post-reload SIMS count = \" .. tostring(n3) .. \", expected 1 — the cleanup contract is broken (clear_all_systems missed SIMS)\")\n\
             os.exit(1)\n\
         end\n\
         print(\"PASS\")\n",
        path = main_lua_path,
        clear = clear_all_systems_symbol(&main_lua_path),
    );

    let _ = std::fs::write("/tmp/stone_sims_cleanup_test.lua", &script);
    let output = run_lua_file(&script).expect("luajit spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.trim().contains("PASS"),
        "sims cleanup test failed:\nstatus: {:?}\nstdout: {}\nstderr: {}",
        output.status, stdout, stderr,
    );
}
