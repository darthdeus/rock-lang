pub use colored::*;
pub use fs_extra::{copy_items, dir::CopyOptions};
pub use notify_debouncer_full::DebouncedEvent;
pub use std::path::PathBuf;
pub use walkdir::WalkDir;

#[cfg(windows)]
pub use std::os::windows::io::AsRawHandle;

// rock-cli's prelude: symbols the kept code reaches through `crate::*`. Core
// types come straight from rock_core + rock_tokenizer, host helpers from
// rock_host, and the rest from std / common deps.
pub use anyhow::{Result, bail};
pub use atomic_refcell::AtomicRefCell;
#[allow(unused_imports)]
pub use log::{debug, error, info, trace, warn};
pub use rock_core::*;
pub use rock_host::process_management::*;
pub use rock_host::source_files::{
    SourceFilesExt, source_file_from_code, source_file_new_preprocessed,
};
pub use rock_host::stick::{
    RendererBackend, StickConfig,
    graph::{PackageGraph, PackageId, PackageNode},
};
pub use rock_tokenizer::*;
pub use std::collections::{HashMap, HashSet};
pub use std::path::Path;
pub use std::sync::Arc;
pub use std::sync::mpsc::channel;
pub use std::time::Duration;
pub use ustr::Ustr;

pub mod ablation_report;
pub mod allocation_join;
pub mod allocator;
pub mod backend_stats;
pub mod bone_backend;
pub mod child_lifetime;
pub mod common;
pub mod compile_cmd;
mod compile_measurements;
pub mod compile_slots;
mod completion_episodes;
mod completion_sweep;
pub mod comptime;
mod dispatcher;
pub mod docs_index;
mod error_test;
pub mod file_watcher;
mod formatting;
pub mod glsl_test;
pub mod invocation_log;
mod iter_completion;
mod iter_lsp;
mod iter_parse;
#[cfg(feature = "backend-js")]
mod js_runner;
#[cfg(feature = "backend-js")]
pub mod js_test;
mod lint_cmd;
pub mod lint_pass;
pub mod lint_union;
pub mod live_project;
mod lsp_survey;
mod lsp_test;
mod monoty_census;
mod package_test_sources;
mod parallel_runner;
pub mod skull_backend;
mod test;
pub mod test_protocol;
pub use test::{TestFilter, TestMetadata, find_test_functions_from_sources};
pub mod modules_analysis;
mod modules_audit;
mod modules_fix;
mod modules_migrate;
pub mod plain;
mod process;
pub mod registry;
mod text_utils;
pub mod unified_compile;
pub mod watch;
pub use allocation_join::run as allocation_join_run;
pub use completion_episodes::completion_episodes;
pub use completion_sweep::completion_sweep;
pub use iter_completion::iter_completion;
pub use iter_lsp::iter_lsp;
pub use iter_parse::iter_parse_skull;
pub use iter_parse::iter_tokenize;
pub use lint_cmd::do_lint;
pub use lsp_survey::lsp_survey;
pub use modules_audit::Format as ModulesFormat;
pub use modules_audit::run as modules_audit_run;
pub use modules_fix::run as modules_fix_run;
pub use modules_migrate::Mode as ModulesMode;
pub use modules_migrate::run as modules_migrate_run;
pub use monoty_census::do_monoty_census;
pub use process::{compiler_process, main_with};

pub use common::*;
pub use dispatcher::*;
pub use file_watcher::*;
pub use formatting::*;
pub use registry::{BackendSpec, CliCommand, Operation};
pub use text_utils::*;
pub use unified_compile::*;
pub use watch::*;

/// Compiler backend selected via `--backend` / `ROCK_BACKEND`. Each variant's
/// name, served operations and policy are registered in
/// the supplied `Registrations::backends` table; the game compile pipeline
/// keys on the variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Bone,
    Skull,
    Js,
    Glsl,
}

/// Resolve the effective backend for an invocation. An explicit choice (the
/// `--backend` flag or `ROCK_BACKEND` env, pre-parsed into `explicit`) is
/// honored verbatim -- the capability gate may then reject an unsupported
/// command. With no explicit choice, the default is `skull`. The
/// backend-agnostic kept commands (fmt, completions) are dispatched
/// before the backend switch, so a `skull` default never rejects them.
pub fn resolve_backend(explicit: Option<Backend>, _command: &CliCommand) -> Backend {
    explicit.unwrap_or(Backend::Skull)
}

/// Shared invocation values plus inert transport filled by registered host decoders.
pub struct Args {
    pub command: &'static CliCommand,
    pub files: Vec<String>,
    /// Comma-separated test-name or source-path substrings from `--filter` or
    /// `ROCK_TEST_FILTER`; the skull test runner applies the parsed selection.
    pub test_filter: Option<String>,
    pub test_filter_explicit: bool,
    pub skip_gpu: bool,
    pub output: Option<String>,
    pub allocation_census: Option<String>,
    pub allocation_sites: Option<String>,
    /// GLSL profile for `rock shader`; only `330core` is supported.
    pub profile: Option<String>,
    pub no_stdlib: bool,
    pub stdout: bool,
    pub check: bool,
    pub iters: usize,
    pub warmup: usize,
    pub verbose: bool,
    pub quiet: bool,
    pub frames: usize,
    pub seconds: Option<f64>,
    pub warmup_seconds: f64,
    pub gc_window: usize,
    pub gc_window_start: usize,
    pub gc_window_mode: String,
    pub perf_format: String,
    pub top: usize,
    pub sim_log_dir: Option<String>,
    pub sim_entry: Option<String>,
    pub type_validation: bool,
    pub headless: bool,
    /// Compiler renderer target override; the manifest remains unchanged.
    pub renderer: Option<RendererBackend>,
    pub test_threads: Option<usize>,
    pub lib: Option<String>,
    pub clean: bool,
    /// `--channel`: release channel for `[build.vars]` resolution
    /// (the compiler contract). None = the project's default_channel.
    pub channel: Option<String>,
    /// `--target-os`: artifact target for the `platform` builtin. Cross-builds
    /// package on one OS for another; None = infer from channel, then host.
    pub target_os: Option<String>,
    /// `--var NAME=VALUE` overrides, raw; parsed/validated at resolution.
    pub build_var_overrides: Vec<String>,
    pub trace: bool,
    pub timeout: u64,
    pub no_timeout: bool,
    pub update_snapshots: bool,
    pub jobs: Option<usize>,
    pub jobs_force: bool,
    pub full_asset_jobs: Option<usize>,
    pub isolated: bool,
    pub workers: bool,
    pub multi_order: bool,

    /// For `rock completion-sweep`: keep every Nth derived probe position
    /// (1 = exhaustive). Controls corpus size vs coverage.
    pub stride: usize,

    /// For `rock iter-completion`: fail (exit nonzero) when the worst
    /// per-keystroke median exceeds this many ms -- the completion perf gate
    /// (the compiler contract M0). None = report only.
    pub budget_ms: Option<f64>,

    pub list: bool,
    pub sim_plan: bool,

    pub app_args: Vec<String>,

    pub jv: bool,
    /// `--lua-line-directives`: sugar for `ROCK_SKULL_LINE_DIRECTIVES=1`.
    pub lua_line_directives: bool,
    pub agent_headless: bool,
    pub agent_access: bool,
    pub level: Option<String>,
    pub daemon_sock: Option<String>,

    /// For `rock test`: re-run tests on every `.rock` file change in
    /// the given paths. Watches the same paths passed to `rock test`
    /// (defaults to `tests/`). Compilation/test failures don't abort
    /// the loop; Ctrl-C to exit.
    pub watch: bool,
    pub port: u16,

    pub single_file: bool,
    pub itch: bool,

    /// Resolved compiler backend for this invocation (default `skull`; see
    /// `from_matches`). `skull` is bone type-check + skull codegen; `bone` is
    /// type-check only. Override with `--backend` or `ROCK_BACKEND=bone|skull`.
    pub backend: Backend,

    /// Build lint pass: `--no-lints` drops the `Warning`/`Advice` tiers from a
    /// build's output; `--lint-summary` compacts them to one count line.
    /// `Error`-severity findings render and fail the build under both.
    pub no_lints: bool,
    pub lint_summary: bool,

    /// `rock lint --lints=a,b`: run exactly these lints instead of the default
    /// tier. The only way to reach an opt-in lint (`bone_lints::OPT_IN_LINTS`),
    /// which a build and the LSP never run. `rock lint` only -- a build's
    /// catalog is not the author's to narrow per invocation.
    pub lints: Option<String>,

    /// `rock lint --only-types=Mob,Item` / `--ignore-types=Color,Vec2`: the
    /// receiver types `method_syntax` sweeps, as a whitelist and a blocklist. A
    /// conversion happens one type at a time, so the sweep has to narrow from
    /// the invocation; `--ignore-types` subtracts from whatever `--only-types`
    /// leaves.
    pub only_types: Option<String>,
    pub ignore_types: Option<String>,

    /// `rock lint --union <lib>`: judge a library's reachability against every
    /// consumer in the repo at once, reporting only what none of them reach.
    pub union: bool,
    /// `rock modules audit --closed`: fail unless the selected root and every
    /// package it reaches, other than the compiler-owned std, is modules-v1.
    pub closed: bool,
    /// `rock modules migrate --apply`: write the planned edits (the default,
    /// and `--check`, only plan).
    pub apply: bool,
    /// `rock modules migrate --consumer <dir>` (repeatable): the consumer set
    /// whose references decide the package's exports (spec 15.2).
    pub consumer: Vec<String>,
    /// `rock fix --imports`: add missing `use` items.
    pub imports: bool,
    /// Optional host-supplied recipe selected under `rock fix`.
    pub fix: Option<&'static registry::FixHandler>,
    /// Where `--union` looks for consumers. Defaults to the lib's parent, which
    /// is right when the lib sits beside its consumers (`libs/example/`); a nested lib
    /// (`libs/example-core`) needs the repo root passed explicitly.
    pub consumers: Option<String>,

    /// For `rock test`: force package-aware compilation (the surrounding
    /// stick.toml is compiled as one unit, so @test files see sibling
    /// definitions). Without the flag, `rock test` still auto-detects a
    /// surrounding package; set `ROCK_TEST_NO_PACKAGE=1` to fall back to
    /// the legacy single-file path.
    pub package: bool,
}

impl Args {
    pub fn parse() -> Self {
        Self::from_matches(Self::command().get_matches())
    }

    /// Construct the clap `Command` for the rock CLI. Public so
    /// `rock completions` (`registrations.rs`) can hand it to
    /// `clap_complete::generate`.
    pub fn command() -> clap::Command {
        use clap::{Arg, ArgAction};
        let command = clap::Command::new("rock")
            .version(env!("CARGO_PKG_VERSION"))
            .about("Rock Compiler")
            .arg(
                Arg::new("command")
                    .required(true)
                    .value_parser(CliCommand::value_parser()),
            )
            .arg(Arg::new("files").num_args(0..))
            .arg(Arg::new("filter").long("filter").value_name("PAT")
                .help("For `rock test`: run @tests whose function name or source path contains a comma-separated substring."))
            .arg(Arg::new("skip-gpu").long("skip-gpu").action(ArgAction::SetTrue)
                .help("For GLSL tests and shader-validate: skip profiles with no display or GL driver; available profiles still run."))
            .arg(Arg::new("output").long("output").alias("out"))
            .arg(Arg::new("profile").long("profile"))
            .arg(Arg::new("no-stdlib").long("no-stdlib").action(ArgAction::SetTrue)
                .help("Compile without the standard library"))
            .arg(Arg::new("stdout").long("stdout").action(ArgAction::SetTrue)
                .help("Print formatted output to stdout instead of updating files - for fmt command"))
            .arg(Arg::new("check").long("check").action(ArgAction::SetTrue)
                .help("For `rock fmt`: check if files are already formatted (exit 1 if not). For `rock completion-sweep`: enforce the completion response invariants at every position (exit 1 on violation)."))
            .arg(Arg::new("iters").long("iters").default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Number of iterations for compilation (used for profiling)"))
            .arg(Arg::new("warmup").long("warmup").default_value("3")
                .value_parser(clap::value_parser!(usize))
                .help("Number of untimed warmup runs before the timed iters (used by `backend-stats`). Default 3. Set to 0 to disable."))
            .arg(Arg::new("verbose").long("verbose").action(ArgAction::SetTrue)
                .help("Enable verbose debug output"))
            .arg(Arg::new("quiet").long("quiet").action(ArgAction::SetTrue)
                .help("Disable timing output (Phase Timing, IO Breakdown)"))
            .arg(Arg::new("perf-format").long("format").default_value("pretty")
                .help("Output format for reports and module tools: \"pretty\" (default) or \"json\"."))
            .arg(Arg::new("type-validation").long("type-validation").action(ArgAction::SetTrue)
                .help("Enable type validation warnings for unresolved types after inference"))
            .arg(Arg::new("renderer").long("renderer").value_parser(["gl", "wgpu"])
                .help("Select the renderer target for this invocation (`gl` or `wgpu`); manifest default remains unchanged"))
            .arg(Arg::new("test-threads").long("test-threads").value_name("NUM")
                .value_parser(clap::value_parser!(usize))
                .help("Number of threads to use for running tests (default: number of CPU cores, use 1 for sequential)"))
            .arg(Arg::new("lib").long("lib")
                .help("Load a project as a library context when running a Rock file."))
            .arg(Arg::new("clean").long("clean").action(ArgAction::SetTrue)
                .help("Force a clean build by clearing the cache before compilation"))
            .arg(Arg::new("channel").long("channel").value_name("CHANNEL")
                .help("Release channel for [build.vars] resolution (must be declared in the project's [build] channels)"))
            .arg(Arg::new("var").long("var").value_name("NAME=VALUE")
                .action(ArgAction::Append)
                .help("Override a declared [build.vars] entry for this invocation (repeatable)"))
            .arg(Arg::new("target-os").long("target-os").value_name("OS")
                .help("Artifact target for the `platform` builtin (linux, osx, windows). Cross-builds package on one OS for another; defaults to the host."))
            .arg(Arg::new("lua-line-directives").long("lua-line-directives").action(ArgAction::SetTrue)
                .help("Number emitted Lua with Rock file:line through LuaJIT line directives, so errors, tracebacks and jit.util report Rock source. Needs a LuaJIT 2.1 from commit c41d727d (2026-10-20) or later. A development option: rejected with a --channel other than dev. Same as ROCK_SKULL_LINE_DIRECTIVES=1."))
            .arg(Arg::new("trace").long("trace").action(ArgAction::SetTrue)
                .help("Enable trace file generation (trace.folded and trace.perfetto) for profiling"))
            .arg(Arg::new("timeout").long("timeout").default_value("60")
                .value_parser(clap::value_parser!(u64))
                .help("Timeout for run command in seconds (default: 60)"))
            .arg(Arg::new("no-timeout").long("no-timeout").action(ArgAction::SetTrue)
                .help("Disable timeout for run command (allow infinite execution)"))
            .arg(Arg::new("stride").long("stride").default_value("10")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock completion-sweep`: keep every Nth probe position (1 = exhaustive)."))
            .arg(Arg::new("budget-ms").long("budget-ms")
                .value_parser(clap::value_parser!(f64))
                .help("For `rock iter-completion`: fail when the worst per-keystroke median exceeds this many ms (the completion perf gate)."))
            .arg(Arg::new("watch").long("watch").action(ArgAction::SetTrue)
                .help("For `rock test`: re-run tests on source changes."))
            .arg(Arg::new("package").long("package").action(ArgAction::SetTrue)
                .help("For `rock test`: compile the surrounding stick.toml package (and its deps) so @test files can call sibling-file symbols. Auto-detected when the path lives inside a package; pass --package=false (or env ROCK_TEST_NO_PACKAGE=1) to force single-file mode."))
            .arg(Arg::new("no-lints").long("no-lints").action(ArgAction::SetTrue)
                .help("Drop lint warnings/advice from build output. Error-severity lints still run and still fail the build."))
            .arg(Arg::new("lint-summary").long("lint-summary").action(ArgAction::SetTrue)
                .help("Report build lint warnings/advice as a one-line count instead of rendering each. Error-severity lints always render in full."))
            .arg(Arg::new("lints").long("lints")
                .help("For `rock lint <path>`: run ONLY the named lints (comma-separated), including opt-in ones a build never runs. `--lints=method_syntax` is how a migration lint is surfaced."))
            .arg(Arg::new("only-types").long("only-types")
                .help("For `rock lint --lints=method_syntax`: comma-separated type names to sweep, to the exclusion of every other (--only-types=Mob,Item). A name that is no type in the project rejects."))
            .arg(Arg::new("ignore-types").long("ignore-types")
                .help("For `rock lint --lints=method_syntax`: comma-separated type names to drop from the sweep (--ignore-types=Color,Vec2). A name that is no type in the project is reported."))
            .arg(Arg::new("union").long("union").action(ArgAction::SetTrue)
                .help("For `rock lint <lib>`: judge a library's reachability against every consumer in the repo at once, reporting only functions that NONE of them reach. The only sound way to run unused_fn on a lib."))
            .arg(Arg::new("closed").long("closed").action(ArgAction::SetTrue)
                .help("For `rock modules audit`: exit non-zero if the root or any package it reaches (other than the compiler-owned std) is legacy."))
            .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue).conflicts_with("check")
                .help("For `rock modules migrate`: write the planned edits after verifying them (without it, or with --check, only the plan is printed)."))
            .arg(Arg::new("consumer").long("consumer").value_name("DIR").action(ArgAction::Append)
                .help("For `rock modules migrate`: a consumer package whose references decide what becomes `pub` (repeatable)."))
            .arg(Arg::new("imports").long("imports").action(ArgAction::SetTrue)
                .help("For `rock fix`: add the `use` items a modules-v1 fragment's unresolved names need, when one accessible candidate exists (plans by default; --apply writes)."))
            .args(registry::registered().fixes.iter().map(|fix|
                Arg::new(fix.flag).long(fix.flag).action(ArgAction::SetTrue).help(fix.help)
            ))
            .arg(Arg::new("consumers").long("consumers")
                .help("For `rock lint --union`: directory to search for consumers (default: the lib's parent). Pass the repo root for a nested lib like `libs/example-core`."))
            .arg(Arg::new("backend").long("backend")
                .value_parser(BackendSpec::value_parser())
                .help(format!("Compiler backend (default: skull). {}. Also settable via ROCK_BACKEND.", registry::BackendSpec::all().iter().map(|spec| format!("{}{}", spec.name, if spec.omitted.is_some() { " (not in this build)" } else { "" })).collect::<Vec<_>>().join(", "))))
            .arg(Arg::new("app_args").last(true).num_args(0..)
                .help("Arguments to pass through to the Rock app (everything after --)"));
        (registry::registered().arguments)(command)
    }

    /// Convert matches from this registry-backed CLI command definition.
    pub fn from_matches(m: clap::ArgMatches) -> Self {
        let many_str = |name: &str| -> Vec<String> {
            m.get_many::<String>(name)
                .map(|vs| vs.cloned().collect())
                .unwrap_or_default()
        };
        let command = *m
            .get_one::<&'static CliCommand>("command")
            .expect("command required");
        let test_filter = m.get_one::<String>("filter").cloned();
        let mut args = Args {
            command,
            files: many_str("files"),
            test_filter: test_filter
                .clone()
                .or_else(|| std::env::var("ROCK_TEST_FILTER").ok()),
            test_filter_explicit: test_filter.is_some(),
            skip_gpu: m.get_flag("skip-gpu"),
            output: m.get_one::<String>("output").cloned(),
            allocation_census: Default::default(),
            allocation_sites: Default::default(),
            profile: m.get_one::<String>("profile").cloned(),
            no_stdlib: m.get_flag("no-stdlib"),
            stdout: m.get_flag("stdout"),
            check: m.get_flag("check"),
            iters: *m.get_one::<usize>("iters").unwrap(),
            warmup: *m.get_one::<usize>("warmup").unwrap(),
            verbose: m.get_flag("verbose"),
            quiet: m.get_flag("quiet"),
            frames: Default::default(),
            seconds: Default::default(),
            warmup_seconds: Default::default(),
            gc_window: Default::default(),
            gc_window_start: Default::default(),
            gc_window_mode: Default::default(),
            perf_format: m
                .get_one::<String>("perf-format")
                .cloned()
                .unwrap_or_else(|| "pretty".to_string()),
            top: Default::default(),
            sim_log_dir: Default::default(),
            sim_entry: Default::default(),
            type_validation: m.get_flag("type-validation"),
            headless: Default::default(),
            renderer: m
                .get_one::<String>("renderer")
                .map(|value| match value.as_str() {
                    "gl" => RendererBackend::Gl,
                    "wgpu" => RendererBackend::Wgpu,
                    _ => unreachable!("renderer value parser admitted an unknown value"),
                }),
            test_threads: m.get_one::<usize>("test-threads").copied(),
            lib: m.get_one::<String>("lib").cloned(),
            clean: m.get_flag("clean"),
            channel: m.get_one::<String>("channel").cloned(),
            target_os: m.get_one::<String>("target-os").cloned(),
            build_var_overrides: m
                .get_many::<String>("var")
                .map(|vals| vals.cloned().collect())
                .unwrap_or_default(),
            trace: m.get_flag("trace"),
            timeout: *m.get_one::<u64>("timeout").unwrap(),
            no_timeout: m.get_flag("no-timeout"),
            update_snapshots: Default::default(),
            jobs: Default::default(),
            jobs_force: Default::default(),
            full_asset_jobs: Default::default(),
            isolated: Default::default(),
            workers: Default::default(),
            multi_order: Default::default(),
            stride: *m.get_one::<usize>("stride").unwrap(),
            budget_ms: m.get_one::<f64>("budget-ms").copied(),
            list: Default::default(),
            sim_plan: Default::default(),
            app_args: many_str("app_args"),
            jv: Default::default(),
            lua_line_directives: m.get_flag("lua-line-directives"),
            agent_headless: Default::default(),
            agent_access: Default::default(),
            level: Default::default(),
            daemon_sock: Default::default(),
            watch: m.get_flag("watch"),
            port: Default::default(),
            single_file: Default::default(),
            itch: Default::default(),
            package: m.get_flag("package"),
            no_lints: m.get_flag("no-lints"),
            lint_summary: m.get_flag("lint-summary"),
            lints: m.get_one::<String>("lints").cloned(),
            only_types: m.get_one::<String>("only-types").cloned(),
            ignore_types: m.get_one::<String>("ignore-types").cloned(),
            union: m.get_flag("union"),
            closed: m.get_flag("closed"),
            apply: m.get_flag("apply"),
            consumer: m
                .get_many::<String>("consumer")
                .map(|v| v.cloned().collect())
                .unwrap_or_default(),
            imports: m.get_flag("imports"),
            fix: registry::registered()
                .fixes
                .iter()
                .find(|fix| m.get_flag(fix.flag)),
            consumers: m.get_one::<String>("consumers").cloned(),
            // Backend selection. An explicit `--backend` flag wins; otherwise an
            // explicit `ROCK_BACKEND` env var; otherwise the implicit default is
            // `skull`. An explicit `--backend skull`/`bone` on a command that
            // backend can't serve still rejects (the capability gate's
            // diagnostic). The backend-agnostic kept commands (fmt, completions,
            // host commands) are dispatched before the backend switch, so the
            // default never rejects them.
            backend: {
                let explicit = m.get_one::<Backend>("backend").copied().or_else(|| {
                    let name = std::env::var("ROCK_BACKEND").ok()?;
                    match BackendSpec::named(&name) {
                        Some(spec) => Some(spec.backend),
                        // Same shape and exit code as clap's rejection of an
                        // unknown `--backend` value.
                        None => {
                            let names: Vec<&str> = BackendSpec::all().iter().map(|b| b.name).collect();
                            eprintln!(
                                "error: invalid value '{name}' for 'ROCK_BACKEND'\n  [possible values: {}]",
                                names.join(", ")
                            );
                            std::process::exit(2);
                        }
                    }
                });
                resolve_backend(explicit, command)
            },
        };
        (registry::registered().decode_arguments)(&m, &mut args);
        args
    }
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod overlay_test_support;
