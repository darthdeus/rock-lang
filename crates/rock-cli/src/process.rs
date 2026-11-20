//! The `rock` process lifecycle.
use crate as rock_cli;
use crate::*;

/// Run the `rock` process, including parsing, logging and cleanup.
/// Call once, before spawning application threads.
pub fn main() {
    crate::compile_budget::mark_process_start();
    // The command runs on its own thread because the main thread's stack is
    // fixed by the OS (often 8 MiB) and compiler passes recurse over the
    // source tree; see `rock_core::nesting` for the bound.
    let command = std::thread::Builder::new()
        .name("rock".into())
        .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
        .spawn(run_process)
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
    let mut pool =
        rayon::ThreadPoolBuilder::new().stack_size(rock_core::nesting::COMPILER_STACK_BYTES);
    if let Some(threads) = threads {
        pool = pool.num_threads(threads);
    }
    let _ = pool.build_global();
}

fn run_process() {
    // `rock lsp` runs before the allocator default, the logger and clap: it
    // installs its own process state.
    let mut raw = std::env::args().skip(1);
    if raw.next().as_deref() == Some("lsp") {
        // `rock lsp` typechecks whole projects, in the server and in its
        // one-shot reports, so it takes the compiler's THP default.
        if let Err(error) = rock_host::compiler_thp::enter_compiler_process() {
            eprintln!("error: cannot start with the compiler THP default: {error}");
            std::process::exit(1);
        }
        crate::allocator::keep_freed_pages_for_reuse();
        #[cfg(not(feature = "sequential"))]
        install_compiler_pool(None);
        std::process::exit(crate::plain::run_lsp(raw));
    }

    // Install the one logger before anything can log. Idempotent; reads
    // ROCK_LOG.
    rock_log::init();
    let (verify, scope) = mir_checks(
        std::env::args().nth(1).as_deref(),
        std::env::var("ROCK_MIR_CHECKS").ok().as_deref(),
    );
    skull_mir::verify::set_checks(Some(verify));
    skull_mir::verify::set_scope_checks(scope);

    if let Some(name) = std::env::args().nth(1) {
        if !name.starts_with('-') && CliCommand::named(&name).is_none() {
            eprintln!("Error: command `{name}` is unavailable in this build");
            std::process::exit(1);
        }
    }

    let args = Args::parse();

    // Same shape and exit code as clap rejecting an argument.
    if let Err(error) = rock_cli::common::thread_count_request() {
        eprintln!("error: {error}");
        std::process::exit(2);
    }

    if compiler_process(args.command, args.list) {
        if let Err(error) = rock_host::compiler_thp::enter_compiler_process() {
            log::error!(target: "CLI", "cannot start compiler with its THP default: {error}");
            std::process::exit(1);
        }
    } else {
        crate::allocator::keep_freed_pages_for_reuse();
    }

    // Line directives need a LuaJIT that reads them, which a release channel
    // does not promise; a release build keeps them off.
    if skull::line_directives_mode() {
        if let Some(channel) = args.channel.as_deref().filter(|channel| *channel != "dev") {
            eprintln!(
                "Error: Lua line directives are a development option; channel `{channel}` is a release channel"
            );
            rock_cli::invocation_log::exit(1);
        }
    }

    if let Err(e) = rock_cli::comptime::set_invocation_selection(
        args.channel.clone(),
        args.target_os.clone(),
        &args.build_var_overrides,
    ) {
        eprintln!("Error: {e}");
        rock_cli::invocation_log::exit(1);
    }

    // `--verbose` only raises the level, never lowers a ROCK_LOG=trace base.
    if args.verbose {
        rock_log::set_level(rock_log::level().max(3));
    }

    crate::frame_commands::initialize_process(&args);

    // After the env setup above and before rayon: begin() exports the parent
    // marker for nested `rock` runs and starts the delayed status-probe thread.
    rock_cli::invocation_log::begin(
        args.command.name,
        &format!("{:?}", args.backend).to_lowercase(),
    );

    // On many-core hosts a larger default pool adds compiler contention. Sequential builds skip the pool.
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
    if let Err(e) = outcome {
        log::error!(target: "CLI", "{}", e);
        rock_cli::invocation_log::exit(1);
    }

    cleanup_all_children();
    rock_cli::invocation_log::finish(Some(0), "return");
    exit_without_teardown();
}

/// End a successful command without process teardown. The kernel reclaims
/// the address space in one pass at exit; letting `exit` run would first have
/// the allocator hand every page back with one `madvise` per span (about 75 ms
/// after a large compile) and each thread release its heap.
fn exit_without_teardown() -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(coverage)]
    unsafe {
        unsafe extern "C" {
            fn __llvm_profile_write_file() -> std::ffi::c_int;
        }
        __llvm_profile_write_file();
    }
    // SAFETY: `_exit` takes no pointers; the buffered standard streams are
    // flushed above, and every file the command wrote is closed by now.
    unsafe { libc::_exit(0) }
}

/// Whether the MIR verifier and the printer's scope guard run. Both catch
/// only compiler bugs. An ordinary compile skips both; `rock test` keeps the
/// guard, which is nearly free, but not the verifier, which costs a large
/// test build over a tenth of its compile. `ROCK_MIR_CHECKS` sets both.
fn mir_checks(command: Option<&str>, setting: Option<&str>) -> (bool, bool) {
    match skull_mir::verify::checks_requested(setting) {
        Some(on) => (on, on),
        None => (false, command == Some("test")),
    }
}

#[cfg(test)]
mod tests {
    use super::mir_checks;

    /// `(verifier, scope guard)`: `rock test` keeps the cheap printer guard
    /// but not the MIR verifier, which costs a large program's test build
    /// over a tenth of its compile.
    #[test]
    fn rock_test_keeps_only_the_scope_guard_unless_asked() {
        assert_eq!(mir_checks(Some("test"), None), (false, true));
        assert_eq!(mir_checks(Some("test"), Some("0")), (false, false));
        assert_eq!(mir_checks(Some("test"), Some("1")), (true, true));
        assert_eq!(mir_checks(Some("compile"), None), (false, false));
        assert_eq!(mir_checks(Some("compile"), Some("1")), (true, true));
        assert_eq!(mir_checks(None, None), (false, false));
    }
}
