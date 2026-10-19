//! Shared process lifecycle for the plain and private CLI hosts.
use crate::*;
use crate as rock_cli;

/// Run a CLI process with a static host table, including parsing, logging and cleanup.
/// Call once, before spawning application threads.
pub fn main_with(registrations: &'static crate::registry::Registrations) {
    // The command runs on its own thread because the main thread's stack is
    // fixed by the OS (often 8 MiB) and compiler passes recurse over the
    // source tree; see `rock_core::nesting` for the bound.
    let command = std::thread::Builder::new()
        .name("rock".into())
        .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
        .spawn(move || run_process(registrations))
        .unwrap_or_else(|error| {
            eprintln!("Error: cannot start the compiler thread: {error}");
            std::process::exit(1);
        });
    if let Err(panic) = command.join() {
        std::panic::resume_unwind(panic);
    }
}

/// Build rayon's global pool with the compiler stack; `threads` of `None`
/// keeps rayon's default count. Ignore errors: a pool already initialized
/// elsewhere is kept.
#[cfg(not(feature = "sequential"))]
fn install_compiler_pool(threads: Option<usize>) {
    let mut pool = rayon::ThreadPoolBuilder::new().stack_size(rock_core::nesting::COMPILER_STACK_BYTES);
    if let Some(threads) = threads {
        pool = pool.num_threads(threads);
    }
    let _ = pool.build_global();
}

fn run_process(registrations: &'static crate::registry::Registrations) {
    crate::registry::install(registrations);
    // Early entries (`rock lsp`) run before the allocator default, the logger
    // and clap: they install their own process state.
    #[cfg(not(feature = "sequential"))]
    let prepare = || install_compiler_pool(None);
    #[cfg(feature = "sequential")]
    let prepare = || {};
    if let Some(code) = rock_cli::registry::run_early_entry(std::env::args().skip(1), prepare) {
        std::process::exit(code);
    }

    // Apply the process-wide purge default; early entries keep their default.
    allocator::purge_freed_pages_immediately();

    // Install the one logger before anything can log (see
    // the compiler contract). Idempotent; reads ROCK_LOG.
    rock_log::init();

    // `ROCK_MONO_ABLATION=1`: the Stage-0e totality ablation
    // (the compiler contract S 0e). Off by default and read once here
    // so every later consult is an atomic load rather than an env lookup.
    skull_mir::ablation::init_from_env();

    if let Some(name) = std::env::args().nth(1) {
        if !name.starts_with('-') && CliCommand::named(&name).is_none() {
            eprintln!("Error: command `{name}` is unavailable in this build");
            std::process::exit(1);
        }
    }

    let args = Args::parse();

    // Same shape and exit code as an invalid `ROCK_BACKEND`.
    if let Err(error) = rock_cli::common::thread_count_request() {
        eprintln!("error: {error}");
        std::process::exit(2);
    }

    if compiler_process(args.command, args.list) {
        if let Err(error) = rock_host::compiler_thp::enter_compiler_process() {
            log::error!(target: "CLI", "cannot start compiler with its THP default: {error}");
            std::process::exit(1);
        }
    }

    let metadata = (registrations.command_metadata)(args.command);
    if let Some(command) = metadata.build_command {
        unsafe { std::env::set_var("ROCK_BUILD_COMMAND", command) };
    }

    // Record the invocation's --channel/--var selection before dispatch; the
    // compile chokepoints resolve [build.vars] against it per project
    // (the compiler contract). A malformed --var fails here, by name.
    let implied_channel = metadata.implied_channel.map(str::to_owned);
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
    // instead. Only raises, never lowers a ROCK_LOG=trace base.)
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

    (registrations.initialize_process)(&args);

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
    // project` took 6.2 s at 6 threads, 6.6 s at 8 and 9.2 s at 16, with system
    // time rising from 6 s to 19 s. The C test lane keeps a wider pool of
    // its own (c_test.rs).
    //
    // Honor RAYON_NUM_THREADS / ROCK_NUM_THREADS if set; otherwise
    // cap at 6.
    //
    // Under `--features sequential` we skip building the pool entirely:
    // with rock-semantic's loading also serial under that feature, no
    // rayon worker threads spawn at all, so a sampling profile of the
    // front end carries no rayon pool/teardown frames.
    #[cfg(not(feature = "sequential"))]
    {
        let cap_threads = rock_cli::common::requested_thread_count().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(6)
        });
        install_compiler_pool(Some(cap_threads));
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

/// Whether the command should enter the compiler allocator policy.
pub fn compiler_process(command: &CliCommand, list: bool) -> bool {
    match (registry::registered().command_metadata)(command).compiler_process {
        registry::CompilerProcess::Always => true,
        registry::CompilerProcess::WhenListed => list,
        registry::CompilerProcess::Never => false,
    }
}
