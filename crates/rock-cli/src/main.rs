#![allow(clippy::uninlined_format_args)]
#![allow(clippy::format_in_format_args)]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub use rock_cli::*;
// use notify::{Config, RecommendedWatcher, Watcher};

fn main() {
    // `rock lsp <subcommand>` is the LSP server + one-shot CLI handlers (there
    // is no separate `rock-lsp` binary — folding it in keeps a single `rock`
    // artifact that can't go stale against the embedded stdlib). Intercepted
    // here, BEFORE `rock_log::init()`, because the LSP path installs its own
    // env_logger (ROCK_LSP_DEBUG -> file) and a global logger can only be set
    // once. Forward everything after `lsp` to `rock_lsp::run_cli` with a
    // synthetic argv[0] so its clap usage/errors read as `rock lsp ...`.
    {
        let mut argv = std::env::args();
        let _prog = argv.next();
        if argv.next().as_deref() == Some("lsp") {
            let mut forwarded = vec!["rock lsp".to_string()];
            forwarded.extend(argv);
            std::process::exit(match rock_lsp::run_cli(forwarded) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            });
        }
    }

    // Install the one logger before anything can log (see
    // docs/logging-architecture.md). Idempotent; reads STONE_LOG.
    rock_log::init();

    // `ROCK_MONO_ABLATION=1`: the Stage-0e totality ablation
    // (`docs/monomorphization-plan.md` S 0e). Off by default and read once here
    // so every later consult is an atomic load rather than an env lookup.
    skull_mir::ablation::init_from_env();

    let args = Args::parse();

    if compiler_process(args.command, args.list) {
        if let Err(error) = rock_host::compiler_thp::enter_compiler_process() {
            log::error!(target: "CLI", "cannot start compiler with its THP default: {error}");
            std::process::exit(1);
        }
    }

    // Sims force telemetry offline and must not read a production secret merely
    // to compile; build-var commands use this to distinguish the invocation.
    unsafe { std::env::set_var("ROCK_BUILD_COMMAND", args.command.cli_name()) };

    // Record the invocation's --channel/--var selection before dispatch; the
    // compile chokepoints resolve [build.vars] against it per project
    // (docs/comptime-plan.md). A malformed --var fails here, by name.
    let implied_channel = matches!(args.command, CliCommand::Web).then(|| "web".to_string());
    if let Err(e) = rock_cli::comptime::set_invocation_selection(
        args.channel.clone(),
        implied_channel,
        args.target_os.clone(),
        &args.build_var_overrides,
    ) {
        eprintln!("Error: {e}");
        rock_cli::invocation_log::exit(1);
    }

    // `--verbose` raises the unified logger to debug. (Pre-unification
    // this was env_logger's `default_filter_or("debug")` inside
    // setup_environment; that env_logger is now dead for rock-cli —
    // rock_log won set_logger first — so the flag is wired here
    // instead. Only raises, never lowers a STONE_LOG=trace base.)
    if args.verbose {
        rock_log::set_level(rock_log::level().max(3));
    }

    // `--trace` (perfetto flamegraph capture) lived in the legacy rock
    // pipeline's `setup_environment`; rock-cli no longer links it. Reject
    // rather than silently ignore so a profiling invocation fails loudly.
    if args.trace {
        eprintln!("Error: --trace is not supported (the perfetto tracing path lived in the removed rock pipeline)");
        rock_cli::invocation_log::exit(1);
    }

    // `--jv` is sugar for STONE_JV=1: build_luajit_command reads that env to
    // lead the game's luajit with -jv. Set it here so both routes converge on
    // one mechanism, and both the in-process spawn and any child inherit it.
    // SAFETY: single-threaded -- runs before the rayon pool (below) or any
    // watcher/game thread spawns.
    if args.jv {
        unsafe { std::env::set_var("STONE_JV", "1") };
    }
    // `--agent-headless`, `--level`, `--daemon-sock` are sugar for the game's
    // own environment variables in the same way; the spawned game inherits
    // them. SAFETY: as above, still single-threaded here.
    for (key, value) in args.agent_launch_env() {
        unsafe { std::env::set_var(key, value) };
    }
    // A game started from a source checkout cannot find the daemon built
    // beside this binary on its own (it resolves from luajit's directory,
    // ROCK_DIR, or PATH); hand it down unless the user chose one.
    if matches!(args.command, rock_cli::CliCommand::Run | rock_cli::CliCommand::Stone)
        && std::env::var("ROCK_AGENT_DAEMON_BIN").map(|v| v.is_empty()).unwrap_or(true)
    {
        if let Some(daemon) = std::env::current_exe().ok().and_then(|exe| rock_cli::sibling_daemon_bin(&exe)) {
            unsafe { std::env::set_var("ROCK_AGENT_DAEMON_BIN", daemon) };
        }
    }

    // After the env setup above and before rayon: begin() exports the parent
    // marker for nested `rock` runs and starts the delayed status-probe thread.
    rock_cli::invocation_log::begin(
        args.command.cli_name(),
        &format!("{:?}", args.backend).to_lowercase(),
    );

    // Cap rayon's global thread pool. Default rayon uses
    // available_parallelism (== logical core count), but on hybrid and
    // many-core hosts the compiler's per-stage parallel jobs mostly add
    // contention past ~6 threads: on a loaded 32-thread host `rock compile
    // mag` took 6.2 s at 6 threads, 6.6 s at 8 and 9.2 s at 16, with system
    // time rising from 6 s to 19 s. The C test lane keeps a wider pool of
    // its own (c_test.rs).
    //
    // Honor RAYON_NUM_THREADS / ROCK_NUM_THREADS if set; otherwise
    // cap at 6. Ignore errors - if rayon's pool was already
    // initialized somewhere else we just keep going.
    //
    // Under `--features sequential` we skip building the pool entirely:
    // with rock-semantic's loading also serial under that feature, no
    // rayon worker threads spawn at all, so a sampling profile of the
    // front end carries no rayon pool/teardown frames.
    #[cfg(not(feature = "sequential"))]
    {
        let cap_threads = rock_cli::c_test::requested_thread_count().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(6)
        });
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(cap_threads)
            .build_global();
    }

    let context = CommandContext::new(args);
    let outcome = context.run();
    // The ablation census prints even when the command itself failed: a build
    // that died partway still recorded every fatal it reached, and discarding
    // them would make the taxonomy depend on how far the build got.
    let had_fatals = rock_cli::ablation_report::report("whole run");
    if let Err(e) = outcome {
        log::error!(target: "CLI", "{}", e);
        rock_cli::invocation_log::exit(1);
    }
    if had_fatals {
        rock_cli::invocation_log::exit(1);
    }

    cleanup_all_children();
    rock_cli::invocation_log::finish(Some(0), "return");
}

fn compiler_process(command: CliCommand, list: bool) -> bool {
    matches!(command, CliCommand::Compile | CliCommand::CompileSingle
        | CliCommand::Test | CliCommand::TestCompile)
        || (command == CliCommand::Sim && list)
}

#[cfg(test)]
mod allocator_tests {
    unsafe extern "C" {
        fn mi_option_get(option: std::ffi::c_int) -> std::ffi::c_long;
    }

    #[test]
    fn compiler_thp_default_does_not_reach_runtime_child() {
        let exe = std::env::current_exe().expect("test executable");
        for (key, value, compiler, runtime) in [
            ("MIMALLOC_ALLOW_THP", None, "0", "1"),
            ("MIMALLOC_ALLOW_THP", Some("1"), "1", "1"),
            ("MIMALLOC_ALLOW_THP", Some("0"), "0", "0"),
            ("MIMALLOC_ALLOW_THP", Some("ON"), "1", "1"),
            ("mimalloc_allow_thp", Some("1"), "1", "1"),
            ("mimalloc_allow_thp", Some("0"), "0", "0"),
        ] {
            let mut child = std::process::Command::new(&exe);
            child.args(["--exact", "allocator_tests::thp_process_child", "--nocapture"])
                .env("ROCK_THP_TEST_DEFAULT", if value.is_none() { "1" } else { "0" })
                .env("ROCK_THP_TEST_ROLE", "compiler")
                .env("ROCK_THP_TEST_EXPECT", compiler)
                .env("ROCK_THP_TEST_RUNTIME_EXPECT", runtime)
                .env_remove("ROCK_COMPILER_THP_DEFAULT")
                .env_remove("MIMALLOC_ALLOW_THP")
                .env_remove("mimalloc_allow_thp");
            if let Some(value) = value {
                child.env(key, value);
            }
            let out = child.output().expect("spawn compiler witness");
            assert!(out.status.success(), "{key}={value:?}: {}{}",
                String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            print!("{key}={value:?}: {}", String::from_utf8_lossy(&out.stdout));
        }
    }

    #[test]
    fn compiler_process_kinds() {
        use super::CliCommand::*;
        for command in [Compile, CompileSingle, Test, TestCompile] {
            assert!(super::compiler_process(command, false));
        }
        assert!(super::compiler_process(Sim, true));
        for command in [Sim, Run, Stone, Frame, Watch, Perf] {
            assert!(!super::compiler_process(command, false));
        }
    }

    #[test]
    fn thp_process_child() {
        let Ok(role) = std::env::var("ROCK_THP_TEST_ROLE") else { return; };
        if role == "compiler" {
            rock_host::compiler_thp::enter_compiler_process().expect("compiler restart");
        }
        let expected: i64 = std::env::var("ROCK_THP_TEST_EXPECT").unwrap().parse().unwrap();
        let option = unsafe { mi_option_get(43) };
        assert_eq!(option as i64, expected, "{role} allocator option");
        if role == "runtime" && std::env::var("ROCK_THP_TEST_DEFAULT").as_deref() == Ok("1") {
            assert!(std::env::var_os("MIMALLOC_ALLOW_THP").is_none());
            assert!(std::env::var_os("ROCK_COMPILER_THP_DEFAULT").is_none());
        }
        println!("{role} THP option={option}");
        #[cfg(target_os = "linux")]
        {
            let disabled = unsafe {
                libc::prctl(libc::PR_GET_THP_DISABLE, 0usize, 0usize, 0usize, 0usize)
            };
            assert_eq!(disabled, i32::from(expected == 0), "{role} kernel THP state");
            println!("{role} THP disabled={disabled}");
        }
        if role == "compiler" {
            // Exercise the shared production spawn recipe used by tests, sims and games.
            unsafe { std::env::set_var("ROCK_LUAJIT", std::env::current_exe().unwrap()) };
            let mut runtime = rock_host::project::build_luajit_command(
                std::path::Path::new("--exact"),
                &["allocator_tests::thp_process_child".into(), "--nocapture".into()],
            );
            runtime.env("ROCK_THP_TEST_ROLE", "runtime")
                .env("ROCK_THP_TEST_EXPECT", std::env::var("ROCK_THP_TEST_RUNTIME_EXPECT").unwrap());
            let out = runtime.output().expect("spawn runtime witness");
            assert!(out.status.success(), "runtime child: {}{}",
                String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            print!("{}", String::from_utf8_lossy(&out.stdout));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let root = tempfile::tempdir().expect("Lua runner proxy");
                let proxy = root.path().join("luajit");
                let exe = std::env::current_exe().unwrap().to_string_lossy()
                    .replace('\'', "'\"'\"'");
                std::fs::write(&proxy, format!(
                    "#!/usr/bin/env bash\nexec '{exe}' --exact allocator_tests::thp_process_child --nocapture\n"
                )).unwrap();
                std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o755)).unwrap();
                unsafe { std::env::set_var("ROCK_LUAJIT", &proxy) };
                let expected = std::env::var("ROCK_THP_TEST_RUNTIME_EXPECT").unwrap();
                let out = rock_host::lua_runner::run_lua_code_with_timeout_and_env(
                    "", Some(std::time::Duration::from_secs(10)),
                    &[("ROCK_THP_TEST_ROLE", "runtime"), ("ROCK_THP_TEST_EXPECT", &expected)],
                ).expect("stdin Lua runtime witness");
                assert!(out.success, "stdin Lua runtime: {}", out.combined());
                print!("stdin Lua runtime: {}", out.stdout);
            }
        }
    }
}
