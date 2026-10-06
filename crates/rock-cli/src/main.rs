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

    // Before any child or thread: begin() exports the parent marker for
    // nested `rock` runs and starts the background git probe.
    rock_cli::invocation_log::begin(
        args.command.cli_name(),
        &format!("{:?}", args.backend).to_lowercase(),
    );

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

    // Cap rayon's global thread pool. Default rayon uses
    // available_parallelism (== logical core count), but on hybrid
    // CPUs (Alder Lake / Raptor Lake) and high-core-count machines
    // the per-stage parallel jobs in the compiler are too small to
    // amortize work-stealing overhead beyond ~16 threads. On a
    // 14900KF (32 logical), bitgun's total_semantic drops from 61 ms
    // to 56 ms when capped at 16.
    //
    // Honor RAYON_NUM_THREADS / ROCK_NUM_THREADS if set; otherwise
    // cap at 16. Ignore errors - if rayon's pool was already
    // initialized somewhere else we just keep going.
    //
    // Under `--features sequential` we skip building the pool entirely:
    // with rock-semantic's loading also serial under that feature, no
    // rayon worker threads spawn at all, so a sampling profile of the
    // front end carries no rayon pool/teardown frames.
    #[cfg(not(feature = "sequential"))]
    {
        let cap_threads = std::env::var("ROCK_NUM_THREADS")
            .ok()
            .or_else(|| std::env::var("RAYON_NUM_THREADS").ok())
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .min(16)
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
