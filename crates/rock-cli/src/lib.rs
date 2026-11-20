pub use colored::*;
pub use notify_debouncer_full::DebouncedEvent;
pub use std::path::PathBuf;
pub use walkdir::WalkDir;

#[cfg(windows)]
pub use std::os::windows::io::AsRawHandle;

pub use anyhow::{Result, bail};
pub use atomic_refcell::AtomicRefCell;
pub use log::{debug, error, info, trace, warn};
pub use rock_core::*;
pub use rock_host::process_management::*;
pub use rock_host::source_files::SourceFilesExt;
pub use rock_host::stick::{
    StickConfig,
    graph::{PackageGraph, PackageId, PackageNode},
};
pub use rock_tokenizer::*;
pub use std::collections::{HashMap, HashSet};
pub use std::path::Path;
pub use std::sync::Arc;
pub use std::sync::mpsc::channel;
pub use std::time::Duration;
pub use ustr::Ustr;

pub(crate) mod allocation_join;
pub mod allocator;
pub(crate) mod backend_stats;
pub(crate) mod body_costs;
pub mod bone_backend;
pub mod child_lifetime;
pub mod common;
pub mod compile_budget;
pub(crate) mod compile_cmd;
pub(crate) mod compile_slots;
pub mod comptime;
mod dispatcher;
pub(crate) mod docs_index;
mod error_test;
pub mod file_watcher;
mod formatting;
pub mod glsl_test;
pub mod image_snapshot;
pub(crate) mod incremental_stats;
pub(crate) mod invocation_log;
#[cfg(feature = "backend-js")]
mod js_runner;
#[cfg(feature = "backend-js")]
pub mod js_test;
mod lint_cmd;
pub(crate) mod lint_pass;
pub mod lint_union;
pub mod live_project;
mod lsp_survey;
mod package_test_sources;
mod parallel_runner;
pub mod skull_backend;
mod test;
pub(crate) mod test_protocol;
mod test_unit;
mod type_holes;
pub(crate) use test::TestFilter;
pub mod plain;
mod process;
pub mod registry;
pub mod runtime_process;
pub mod watch;
pub(crate) use lint_cmd::do_lint;
pub(crate) use lsp_survey::lsp_survey;
pub use plain::compiler_process;
pub use process::main;

pub use common::*;
pub use dispatcher::*;
pub use file_watcher::*;
pub(crate) use formatting::*;
pub use registry::{BackendSpec, CliCommand, Operation};
pub(crate) use watch::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Skull,
    Js,
    Glsl,
}

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
    pub check: bool,
    pub iters: usize,
    pub warmup: usize,
    pub verbose: bool,
    pub quiet: bool,
    pub frames: usize,
    pub seconds: Option<f64>,
    pub gc_window: usize,
    pub gc_window_start: usize,
    pub gc_window_mode: String,
    pub perf_format: String,
    pub test_threads: Option<usize>,
    /// `--channel`: release channel for `[build.vars]` resolution.
    /// None = the project's default_channel.
    pub channel: Option<String>,
    /// `--target-os`: artifact target for the `platform` builtin. Cross-builds
    /// package on one OS for another; None = infer from channel, then host.
    pub target_os: Option<String>,
    /// `--var NAME=VALUE` overrides, raw; parsed/validated at resolution.
    pub build_var_overrides: Vec<String>,
    pub update_snapshots: bool,
    pub jobs: Option<usize>,
    pub full_asset_jobs: Option<usize>,

    pub list: bool,

    pub app_args: Vec<String>,

    pub jv: bool,

    /// Resolved compiler backend for this invocation (default `skull`; see
    /// `from_matches`). Override with `--backend`.
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

    /// `rock lint --union <lib>`: judge a library's reachability against every
    /// consumer in the repo at once, reporting only what none of them reach.
    pub union: bool,
    /// Where `--union` looks for consumers. Defaults to the lib's parent, which
    /// is right when the lib sits beside its consumers (`libs/example/`); a nested lib
    /// (`libs/example-core`) needs the repo root passed explicitly.
    pub consumers: Option<String>,
}

impl Args {
    pub fn parse() -> Self {
        Self::from_matches(Self::command().get_matches())
    }

    pub fn command() -> clap::Command {
        use clap::{Arg, ArgAction};
        let var = Arg::new("var")
            .long("var")
            .value_name("NAME=VALUE")
            .action(ArgAction::Append)
            .help("Override a declared [build.vars] entry for this invocation (repeatable)");
        let command = clap::Command::new("rock")
            .version(env!("CARGO_PKG_VERSION"))
            .propagate_version(true)
            .about("Rock Compiler")
            .disable_help_subcommand(true)
            .subcommand_required(true)
            .arg_required_else_help(true)
            .arg(Arg::new("filter").long("filter").value_name("PAT")
                .help("For `rock test`: run @tests whose function name or source path contains a comma-separated substring."))
            .arg(Arg::new("skip-gpu").long("skip-gpu").action(ArgAction::SetTrue)
                .help("For GLSL tests and shader-validate: skip profiles with no display or GL driver; available profiles still run."))
            .arg(Arg::new("output").long("output")
                .help("Output path; package requires an explicit output directory."))
            .args([
            Arg::new("allocation-census").long("allocation-census")
                .value_name("PATH")
                .help("For `rock sim`: opt into a sim-wide gross Lua allocation census and write a complete JSON artifact to PATH (discovery evidence only)."),
            Arg::new("allocation-sites").long("allocation-sites")
                .value_name("DIR")
                .help("For `rock sim`: instrument compiler-known allocation sites in isolated sims and write frame-aware joined reports to DIR."),
            ])
            .arg(Arg::new("profile").long("profile"))
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
            .args([
            Arg::new("frames").short('n').long("frames").default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Frame command process frames; perf command measured frames (default 1200)"),
            Arg::new("seconds").long("seconds")
                .value_parser(clap::value_parser!(f64))
                .help("Perf wall-clock safety cap from runtime launch, including readiness and warmup. Incomplete intervals fail."),
            Arg::new("warmup-frames").long("warmup-frames")
                .value_parser(clap::value_parser!(usize))
                .help("Perf warmup frames after scenario readiness (default: scenario declaration). Zero disables warmup."),
            Arg::new("gc-window").long("gc-window").default_value("0")
                .value_parser(clap::value_parser!(usize))
                .help("`rock perf` only: record N frames of gross Lua allocator request traffic (opening at --gc-window-start), byte flow, and request populations. This is request traffic, not retained heap growth. 0 = disabled."),
            Arg::new("gc-window-start").long("gc-window-start").default_value("100")
                .value_parser(clap::value_parser!(usize))
                .help("`rock perf` only: frame the --gc-window opens on (default 100). Window compilation is counted separately; warmup does not guarantee a settled JIT; the allocation window must fit within the declared measured interval."),
            Arg::new("gc-window-mode").long("gc-window-mode").default_value("stopped")
                .value_parser(["stopped", "running"])
                .help("`rock perf --gc-window` only: `stopped` pauses GC after a collection; gross requests still include JIT compilation; `running` preserves the ordinary collector schedule. The mode is recorded in profiler output."),
            ])
            .arg(Arg::new("perf-format").long("format").default_value("pretty")
                .value_parser(["pretty", "json"])
                .help("Output format for reports: \"pretty\" (default) or \"json\"."))
            .arg(Arg::new("test-threads").long("test-threads").value_name("NUM")
                .value_parser(clap::value_parser!(usize))
                .help("Number of threads to use for running tests (default: number of CPU cores, use 1 for sequential)"))
            .arg(Arg::new("channel").long("channel").value_name("CHANNEL")
                .help("Release channel for [build.vars] resolution (must be declared in the project's [build] channels)"))
            .arg(var.clone())
            .arg(Arg::new("target-os").long("target-os").value_name("OS")
                .help("Artifact target for the `platform` builtin (linux, osx, windows). Cross-builds package on one OS for another; defaults to the host."))
            .args([
            Arg::new("update-snapshots").long("update-snapshots").action(ArgAction::SetTrue)
                .help("For `rock sim`: rewrite snapshot baselines instead of diffing."),
            Arg::new("jobs").long("jobs").short('j').value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: run up to N sim children concurrently (default: CPU count, bounded by total RAM at 3.6 GiB per lane and free GPU memory less a 6 GiB reserve at 2 GiB per lane; requests above the RAM budget are rejected, while requests above only the free-GPU budget run at that budget with a warning; at most the core count; a named run narrows N to its name count first; 1 = serial). Applies to the whole-suite and multi-name forms."),
            Arg::new("full-asset-jobs").long("full-asset-jobs").value_name("R")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: allow at most R of the concurrent children to hold full-size assets (STONE_ASSET_STUB=0 with STONE_IMAGE_MAX_SIDE=0, 2-4 GB of GPU memory each); the remaining slots up to --jobs run the other entries. Default 2. Bounded by --jobs."),

            Arg::new("list").long("list").action(ArgAction::SetTrue)
                .help("For `rock sim`: list every `@sim` wire name in the project, one per line, then exit."),
            ])
            .arg(Arg::new("no-lints").long("no-lints").action(ArgAction::SetTrue)
                .help("Drop lint warnings/advice from build output. Error-severity lints still run and still fail the build."))
            .arg(Arg::new("lint-summary").long("lint-summary").action(ArgAction::SetTrue)
                .help("Report build lint warnings/advice as a one-line count instead of rendering each. Error-severity lints always render in full."))
            .arg(Arg::new("lints").long("lints")
                .help("For `rock lint <path>`: run ONLY the named lints (comma-separated), including opt-in ones a build never runs."))
            .arg(Arg::new("union").long("union").action(ArgAction::SetTrue)
                .help("For `rock lint <lib>`: judge a library's reachability against every consumer in the repo at once, reporting only functions that NONE of them reach. The only sound way to run unused_fn on a lib."))
            .arg(Arg::new("consumers").long("consumers")
                .help("For `rock lint --union`: directory to search for consumers (default: the lib's parent). Pass the repo root for a nested lib like `libs/example-core`."))
            .arg(Arg::new("backend").long("backend")
                .value_parser(BackendSpec::value_parser())
                .help(format!("Compiler backend (default: skull). {}.", registry::BackendSpec::all().iter().map(|spec| format!("{}{}", spec.name, if spec.omitted_feature.is_some() { " (not in this build)" } else { "" })).collect::<Vec<_>>().join(", "))))
            .args([
            Arg::new("jv").long("jv").action(ArgAction::SetTrue)
                .help("Run the game's luajit with -jv (verbose JIT trace log to stderr). Same as STONE_JV=1; works for stone/frame/run/perf/sim."),
            ])
            // Actual app arguments are parsed from the selected command.
            .arg(
                Arg::new("app_args")
                    .last(true)
                    .num_args(0..)
                    .hide(true),
            );
        let mut command = command.mut_args(|arg| {
            if arg.get_id() == "app_args" || arg.get_id() == "var" {
                arg
            } else {
                arg.global(true)
            }
        });
        for cli_command in CliCommand::all() {
            let mut subcommand = clap::Command::new(cli_command.name)
                .arg(Arg::new("files").num_args(0..))
                .arg(
                    Arg::new("app_args")
                        .last(true)
                        .num_args(0..)
                        .help("Arguments to pass through to the Rock app (everything after --)"),
                )
                .arg(var.clone());
            if cli_command.name == "fmt" {
                subcommand = subcommand.arg(
                    Arg::new("check")
                        .long("check")
                        .action(ArgAction::SetTrue)
                        .help("Check if files are already formatted (exit 1 if not)."),
                );
            }
            command = command.subcommand(subcommand);
        }
        command
    }

    pub fn from_matches(m: clap::ArgMatches) -> Self {
        let mut build_var_overrides = m
            .get_many::<String>("var")
            .map(|vals| vals.cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let (command_name, m) = m.subcommand().expect("command required");
        let command = CliCommand::named(command_name).expect("registered subcommand");
        build_var_overrides.extend(
            m.get_many::<String>("var")
                .map(|vals| vals.cloned())
                .into_iter()
                .flatten(),
        );
        let many_str = |name: &str| -> Vec<String> {
            m.get_many::<String>(name)
                .map(|vs| vs.cloned().collect())
                .unwrap_or_default()
        };
        let test_filter = m.get_one::<String>("filter").cloned();
        let perf = command_name == "perf";
        Args {
            command,
            files: many_str("files"),
            test_filter: test_filter
                .clone()
                .or_else(|| std::env::var("ROCK_TEST_FILTER").ok()),
            test_filter_explicit: test_filter.is_some(),
            skip_gpu: m.get_flag("skip-gpu"),
            output: m.get_one::<String>("output").cloned(),
            allocation_census: m.get_one::<String>("allocation-census").cloned(),
            allocation_sites: m.get_one::<String>("allocation-sites").cloned(),
            profile: m.get_one::<String>("profile").cloned(),
            check: command_name == "fmt" && m.get_flag("check"),
            iters: *m.get_one::<usize>("iters").unwrap(),
            // Perf counts warmup in frames, from its own option.
            warmup: if perf {
                m.get_one::<usize>("warmup-frames")
                    .copied()
                    .unwrap_or(usize::MAX)
            } else {
                *m.get_one::<usize>("warmup").unwrap()
            },
            verbose: m.get_flag("verbose"),
            quiet: m.get_flag("quiet"),
            frames: if perf
                && m.value_source("frames") == Some(clap::parser::ValueSource::DefaultValue)
            {
                1200
            } else {
                *m.get_one::<usize>("frames").unwrap()
            },
            seconds: m.get_one::<f64>("seconds").copied(),
            gc_window: *m.get_one::<usize>("gc-window").unwrap(),
            gc_window_start: *m.get_one::<usize>("gc-window-start").unwrap(),
            gc_window_mode: m.get_one::<String>("gc-window-mode").cloned().unwrap(),
            perf_format: m
                .get_one::<String>("perf-format")
                .cloned()
                .unwrap_or_else(|| "pretty".to_string()),
            test_threads: m.get_one::<usize>("test-threads").copied(),
            channel: m.get_one::<String>("channel").cloned(),
            target_os: m.get_one::<String>("target-os").cloned(),
            build_var_overrides,
            update_snapshots: m.get_flag("update-snapshots"),
            jobs: m.get_one::<usize>("jobs").copied(),
            full_asset_jobs: m.get_one::<usize>("full-asset-jobs").copied(),
            list: m.get_flag("list"),
            app_args: many_str("app_args"),
            jv: m.get_flag("jv"),
            no_lints: m.get_flag("no-lints"),
            lint_summary: m.get_flag("lint-summary"),
            lints: m.get_one::<String>("lints").cloned(),
            union: m.get_flag("union"),
            consumers: m.get_one::<String>("consumers").cloned(),
            backend: m
                .get_one::<Backend>("backend")
                .copied()
                .unwrap_or(Backend::Skull),
        }
    }
}

/// Language fixtures from the same source revision as this compiler.
#[cfg(feature = "test-support")]
pub fn test_corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod overlay_test_support;

mod asset_selection;
mod compiler_watch;
pub(crate) mod frame_commands;
pub(crate) mod frame_runtime;
pub mod game_runtime;
pub mod headless_scope;
mod log_record;
pub mod package;
pub(crate) mod stone_runtime;
pub(crate) mod stone_watch;

mod run_sim;
mod sim_command;
mod sim_environment;
mod sim_observability;

mod sdk_check;

#[cfg(test)]
mod completion_tests {
    #[test]
    fn shell_completions_offer_the_report_formats() {
        let mut out = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut crate::Args::command(),
            "rock",
            &mut out,
        );
        let script = String::from_utf8(out).expect("the completion script is text");
        assert!(script.contains("fmt"));
        assert!(script.contains("--check"));
        let format_arm = script
            .split("--format)")
            .nth(1)
            .expect("the script completes --format");
        let values = format_arm.lines().nth(1).unwrap_or_default();
        assert!(values.contains("pretty json"), "{values}");
    }
}
