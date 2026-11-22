//! The `rock` command line: options, commands and their dispatch.
use crate as rock_cli;
use crate::*;
use clap::{CommandFactory, FromArgMatches, ValueEnum};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, ValueEnum)]
pub enum Backend {
    /// Lua through the bone-HIR codegen (default)
    #[default]
    Skull,
    /// A JS-subset executor (node)
    #[cfg_attr(not(feature = "backend-js"), value(help = "not in this build"))]
    Js,
    /// GLSL value tests on the GPU
    Glsl,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::Subcommand, ValueEnum)]
pub enum Command {
    NativeBuild,
    EngineLibs,
    EngineBuildReceipt,
    SdkCheck,
    Sim,
    Frame,
    Perf,
    Stone,
    Package,
    AssetCheck,
    Check,
    Compile,
    Test,
    Run,
    Fmt,
    Lint,
    AllocationJoin,
    LspSurvey,
    BackendStats,
    IncrementalStats,
    Shader,
    ShaderValidate,
    DocsIndex,
}

impl Command {
    pub fn name(self) -> String {
        self.to_possible_value()
            .expect("every command has a name")
            .get_name()
            .to_owned()
    }

    pub(crate) fn logs_invocation(self) -> bool {
        use Command::*;
        matches!(
            self,
            Compile | Test | Run | Lint | Fmt | Frame | Sim | Stone
        )
    }

    /// Whether the command should enter the compiler allocator policy.
    pub fn compiler_process(self, list: bool) -> bool {
        use Command::*;
        matches!(self, Check | Compile | Test) || (list && self == Sim)
    }
}

type Handler = fn(&Args) -> Result<()>;

/// The command a backend serves, or `None` for a direct command or one this
/// backend lacks.
fn backend_handler(backend: Backend, command: Command) -> Option<Handler> {
    use Backend::*;
    use Command::*;
    Some(match (backend, command) {
        (Skull, Sim) => crate::sim_command::handle_sim,
        (Skull, Frame) => crate::frame_commands::handle_frame,
        (Skull, Perf) => crate::frame_commands::handle_perf,
        (Skull, Stone) => |args| {
            let path = args.get_project_dir_or_current()?;
            crate::stone_watch::do_stone_watch(&path, args.quiet, &args.app_args)
        },
        (Skull, Test) => |args| {
            let files = args.get_test_files()?;
            if files.iter().any(|path| path.ends_with(".rock.lsp")) {
                anyhow::bail!(
                    ".rock.lsp fixtures have been retired. Run the editor tests with `cargo test -p rock-cli editor::`."
                );
            }
            let filter = args.test_filter()?;
            rock_cli::skull_backend::skull_test(&files, filter.as_ref(), args.test_threads)
        },
        (Skull, Compile) => |args| {
            let file = args.get_required_file("compile")?;
            rock_cli::skull_backend::skull_compile_single(file, args.get_compile_output()?)
        },
        (Skull, Run) => |args| {
            let file = args.get_required_file("run")?;
            rock_cli::skull_backend::skull_run(file, &args.app_args)
        },
        #[cfg(feature = "backend-js")]
        (Js, Test) => |args| {
            rock_cli::js_test::js_test_with_threads(&args.get_test_files()?, args.test_threads)
        },
        #[cfg(feature = "backend-js")]
        (Js, Run) => |args| rock_cli::js_test::js_run(args.get_required_file("run")?),
        (Glsl, Test) => |args| {
            let files = args.get_test_files()?;
            rock_cli::glsl_test::glsl_test(&files, args.skip_gpu)
        },
        _ => return None,
    })
}

fn is_backend_command(command: Command) -> bool {
    use Command::*;
    matches!(command, Sim | Frame | Perf | Stone | Compile | Test | Run)
}

fn reject_reason(backend: Backend, command: Command) -> String {
    if backend == Backend::Js && cfg!(not(feature = "backend-js")) {
        return "the `js` backend was omitted from this build; enable the `backend-js` feature \
                (rebuild with default features or `--features backend-js`)"
            .to_owned();
    }
    let supported: Vec<String> = Command::value_variants()
        .iter()
        .filter(|command| backend_handler(backend, **command).is_some())
        .map(|command| command.name())
        .collect();
    let backend_name = backend.to_possible_value().expect("named backend");
    format!(
        "the `{}` backend cannot serve `{}`. Supported commands: {}.",
        backend_name.get_name(),
        command.name(),
        supported.join(", "),
    )
}

fn run_direct(args: &Args) -> Result<()> {
    use Command::*;
    match args.command {
        NativeBuild => {
            anyhow::ensure!(!args.files.is_empty(), "usage: rock native-build <project>");
            for project in &args.files {
                for lib in rock_engine_host::engine_build::game_engine_libs(project)? {
                    rock_engine_host::engine_build::build_optional_engine_lib(&lib)?;
                }
            }
            Ok(())
        }
        EngineLibs => {
            let mut libs = Vec::new();
            for project in &args.files {
                libs.extend(rock_engine_host::engine_build::game_engine_libs(project)?);
            }
            println!(
                "{}",
                rock_engine_host::native_libraries::registry_json(&libs)?
            );
            Ok(())
        }
        EngineBuildReceipt => {
            let output = args.output.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: rock engine-build-receipt [<game>...] --output <receipt.json>"
                )
            })?;
            rock_engine_host::engine_build::prepare_engine_build_receipt(
                &args.files,
                std::path::Path::new(output),
            )
        }
        SdkCheck => {
            if args.files.len() != 1 {
                anyhow::bail!("usage: rock sdk-check <project>");
            }
            crate::sdk_check::check(&args.files[0], std::path::Path::new(&rock_host::rock_dir()))?;
            println!("SDK manifest compatibility: OK");
            Ok(())
        }
        Package => {
            let output = args
                .output
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("package requires --output <directory>"))?;
            let project = args.get_project_dir_or_current()?;
            crate::package::run_package(&project, std::path::Path::new(output))
        }
        AssetCheck => {
            let project = args.get_project_dir_or_current()?;
            crate::package::check_project_assets(std::path::Path::new(&project))
        }
        Check if args.alloc_sites => {
            anyhow::ensure!(
                args.files.len() <= 2,
                "check --alloc-sites takes a project or file and an optional path or function filter"
            );
            crate::bone_backend::check_allocation_sites(
                args.files.first().map_or(".", String::as_str),
                args.files.get(1).map(String::as_str),
            )
        }
        Check => {
            let file = args.get_required_file("check")?;
            rock_cli::bone_backend::bone_check_single(file, args.iters)
        }
        Fmt => {
            let paths = args.get_paths_or_current()?;
            if let Err(e) = do_fmt(&paths, args.check) {
                log::error!(target: "FMT", "{}", e);
                rock_cli::invocation_log::exit(1);
            }
            Ok(())
        }
        Lint => rock_cli::do_lint(&args.get_path_or_current()?, args.lints.as_deref()),
        AllocationJoin => crate::allocation_join::run(&args.files),
        LspSurvey => rock_cli::lsp_survey(
            &args.get_path_or_current()?,
            args.iters,
            args.warmup,
            args.json(),
            args.allocation_hints,
        ),
        BackendStats => rock_cli::backend_stats::do_backend_stats(
            &args.get_path_or_current()?,
            args.iters,
            args.warmup,
            args.json(),
        ),
        IncrementalStats => rock_cli::incremental_stats::incremental_stats(
            &args.get_path_or_current()?,
            args.iters,
            args.warmup,
            args.json(),
        ),
        Shader => rock_cli::skull_backend::skull_shader(
            &args.files,
            args.output.clone(),
            args.profile.clone(),
        ),
        ShaderValidate => rock_cli::glsl_test::shader_validate(&args.files, args.skip_gpu),
        DocsIndex => rock_cli::docs_index::run(&args.files, args.output.as_deref()),
        Sim | Frame | Perf | Stone | Compile | Test | Run => {
            unreachable!("backend commands dispatch through backend_handler")
        }
    }
}

/// Options apply to every command; each command reads the ones it uses.
#[derive(clap::Parser)]
#[command(
    name = "rock",
    version,
    about = "Rock Compiler",
    disable_help_subcommand = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
    #[arg(skip)]
    pub files: Vec<String>,
    /// Arguments after `--`, passed to the Rock app.
    #[arg(skip)]
    pub app_args: Vec<String>,
    #[arg(skip)]
    pub check: bool,
    #[arg(skip)]
    pub alloc_sites: bool,
    #[arg(skip)]
    pub allocation_hints: bool,
    /// Override a declared [build.vars] entry for this invocation (repeatable)
    #[arg(long = "var", value_name = "NAME=VALUE")]
    pub build_var_overrides: Vec<String>,

    /// Compiler backend
    #[arg(long, value_enum, default_value_t)]
    pub backend: Backend,
    /// For `rock test`: run @tests whose function name or source path contains a comma-separated substring.
    #[arg(long, value_name = "PAT")]
    pub filter: Option<String>,
    /// For GLSL tests and shader-validate: skip profiles with no display or GL driver; available profiles still run.
    #[arg(long)]
    pub skip_gpu: bool,
    /// Output path; package requires an explicit output directory.
    #[arg(long)]
    pub output: Option<String>,
    /// For `rock sim`: instrument compiler-known allocation sites in isolated sims and write frame-aware joined reports to DIR.
    #[arg(long, value_name = "DIR")]
    pub allocation_sites: Option<String>,
    /// For `rock shader`: the GLSL profile; only `330core` is supported.
    #[arg(long)]
    pub profile: Option<String>,
    /// Number of iterations for compilation (used for profiling)
    #[arg(long, default_value_t = 1)]
    pub iters: usize,
    /// Untimed warmup runs before the timed iters (used by `backend-stats`). Set to 0 to disable.
    #[arg(long, default_value_t = 3)]
    pub warmup: usize,
    /// Disable timing output (Phase Timing, IO Breakdown)
    #[arg(long)]
    pub quiet: bool,
    /// Frames to run: `rock frame` process frames (default 1); `rock perf` measured frames (default 1200)
    #[arg(short = 'n', long)]
    pub frames: Option<usize>,
    /// Perf wall-clock safety cap from runtime launch, including readiness and warmup. Incomplete intervals fail.
    #[arg(long)]
    pub seconds: Option<f64>,
    /// Perf warmup frames after scenario readiness (default: scenario declaration). Zero disables warmup.
    #[arg(long)]
    pub warmup_frames: Option<usize>,
    /// `rock perf` only: record N frames of gross Lua allocator request traffic (opening at --gc-window-start), byte flow, and request populations. This is request traffic, not retained heap growth. 0 = disabled.
    #[arg(long, default_value_t = 0)]
    pub gc_window: usize,
    /// `rock perf` only: frame the --gc-window opens on. Window compilation is counted separately; warmup does not guarantee a settled JIT; the allocation window must fit within the declared measured interval.
    #[arg(long, default_value_t = 100)]
    pub gc_window_start: usize,
    /// `rock perf --gc-window` only: `stopped` pauses GC after a collection; gross requests still include JIT compilation; `running` preserves the ordinary collector schedule. The mode is recorded in profiler output.
    #[arg(long, default_value = "stopped", value_parser = ["stopped", "running"])]
    pub gc_window_mode: String,
    /// Output format for reports
    #[arg(long = "format", default_value = "pretty", value_parser = ["pretty", "json"])]
    pub perf_format: String,
    /// Number of threads to use for running tests (default: number of CPU cores, use 1 for sequential)
    #[arg(long, value_name = "NUM")]
    pub test_threads: Option<usize>,
    /// Release channel for [build.vars] resolution (must be declared in the project's [build] channels)
    #[arg(long)]
    pub channel: Option<String>,
    /// Artifact target for the `platform` builtin (linux, osx, windows). Cross-builds package on one OS for another; defaults to the host.
    #[arg(long, value_name = "OS")]
    pub target_os: Option<String>,
    /// For `rock sim`: rewrite snapshot baselines instead of diffing.
    #[arg(long)]
    pub update_snapshots: bool,
    /// For `rock sim`: run up to N sim children concurrently (default: CPU count, bounded by total RAM at 3.6 GiB per lane and free GPU memory less a 6 GiB reserve at 2 GiB per lane; requests above the RAM budget are rejected, while requests above only the free-GPU budget run at that budget with a warning; at most the core count; a named run narrows N to its name count first; 1 = serial). Applies to the whole-suite and multi-name forms.
    #[arg(short = 'j', long, value_name = "N")]
    pub jobs: Option<usize>,
    /// For `rock sim`: allow at most R of the concurrent children to hold full-size assets (STONE_ASSET_STUB=0 with STONE_IMAGE_MAX_SIDE=0, 2-4 GB of GPU memory each); the remaining slots up to --jobs run the other entries. Default 2. Bounded by --jobs.
    #[arg(long, value_name = "R")]
    pub full_asset_jobs: Option<usize>,
    /// For `rock sim`: list every `@sim` wire name in the project, one per line, then exit.
    #[arg(long)]
    pub list: bool,
    /// Run the game's luajit with -jv (verbose JIT trace log to stderr). Same as STONE_JV=1; works for stone/frame/run/perf/sim.
    #[arg(long)]
    pub jv: bool,
    /// Drop lint warnings/advice from build output. Error-severity lints still run and still fail the build.
    #[arg(long)]
    pub no_lints: bool,
    /// Report build lint warnings/advice as a one-line count instead of rendering each. Error-severity lints always render in full.
    #[arg(long)]
    pub lint_summary: bool,
    /// For `rock lint <path>`: run ONLY the named lints (comma-separated), including opt-in ones a build never runs.
    #[arg(long)]
    pub lints: Option<String>,
}

impl Args {
    pub fn parse() -> Self {
        Self::from_matches(Self::cli().get_matches())
    }

    /// The parser: every option is global, and every command takes paths,
    /// `--var` and the app arguments after `--`.
    pub fn cli() -> clap::Command {
        let var = clap::Arg::new("var")
            .long("var")
            .value_name("NAME=VALUE")
            .action(clap::ArgAction::Append)
            .help("Override a declared [build.vars] entry for this invocation (repeatable)");
        <Self as CommandFactory>::command()
            .mut_args(|arg| {
                if arg.get_id() == "build_var_overrides" {
                    arg
                } else {
                    arg.global(true)
                }
            })
            .mut_subcommands(|command| {
                let fmt = command.get_name() == "fmt";
                let command =
                    command
                        .arg(clap::Arg::new("files").num_args(0..))
                        .arg(clap::Arg::new("app_args").last(true).num_args(0..).help(
                            "Arguments to pass through to the Rock app (everything after --)",
                        ))
                        .arg(var.clone());
                if fmt {
                    command.arg(
                        clap::Arg::new("check")
                            .long("check")
                            .action(clap::ArgAction::SetTrue)
                            .help("Check if files are already formatted (exit 1 if not)."),
                    )
                } else if command.get_name() == "check" {
                    command.arg(
                        clap::Arg::new("alloc_sites")
                            .long("alloc-sites")
                            .action(clap::ArgAction::SetTrue)
                            .help("List Lua allocation sites for [project-or-file] [path-or-function-filter]"),
                    )
                } else if command.get_name() == "lsp-survey" {
                    command.arg(
                        clap::Arg::new("allocation_hints")
                            .long("allocation-hints")
                            .action(clap::ArgAction::SetTrue)
                            .help("Measure Lua allocation diagnostics for the edited file"),
                    )
                } else if command.get_name() == "compile" {
                    command.alias("build")
                } else {
                    command
                }
            })
    }

    pub fn from_matches(matches: clap::ArgMatches) -> Self {
        let mut args = Self::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
        let (_, sub) = matches.subcommand().expect("a command is required");
        let many = |id: &str| -> Vec<String> {
            sub.get_many::<String>(id)
                .map(|values| values.cloned().collect())
                .unwrap_or_default()
        };
        args.files = many("files");
        args.app_args = many("app_args");
        args.build_var_overrides.extend(many("var"));
        args.check = args.command == Command::Fmt && sub.get_flag("check");
        args.alloc_sites = args.command == Command::Check && sub.get_flag("alloc_sites");
        args.allocation_hints =
            args.command == Command::LspSurvey && sub.get_flag("allocation_hints");
        args
    }

    pub fn run(&self) -> Result<()> {
        self.require_command_options()?;
        crate::lint_pass::set_policy(if self.no_lints {
            crate::lint_pass::LintReport::Off
        } else if self.lint_summary {
            crate::lint_pass::LintReport::Summary
        } else {
            crate::lint_pass::LintReport::Full
        });
        if !is_backend_command(self.command) {
            return run_direct(self);
        }
        match backend_handler(self.backend, self.command) {
            Some(handler) => handler(self),
            None => anyhow::bail!("{}", reject_reason(self.backend, self.command)),
        }
    }

    fn require_command_options(&self) -> Result<()> {
        let test = self.command == Command::Test;
        if !test && self.filter.is_some() {
            anyhow::bail!("`--filter` applies only to `rock test`");
        }
        if test && self.backend != Backend::Skull && self.test_filter()?.is_some() {
            anyhow::bail!("`rock test` filtering is supported only by the skull test runner");
        }
        if self.skip_gpu
            && self.command != Command::ShaderValidate
            && !(test && self.backend == Backend::Glsl)
        {
            anyhow::bail!(
                "`--skip-gpu` is supported only by `rock test --backend glsl` and `rock shader-validate`"
            );
        }
        if self.profile.is_some() && self.command != Command::Shader {
            anyhow::bail!(
                "rock {}: --profile applies only to `rock shader` (offline GLSL emission). \
                 In-game shader source is emitted once and adapted to the live GL context by \
                 the engine, so no other command selects a profile. `rock shader-validate` \
                 gates both profiles and takes no flag.",
                self.command.name(),
            );
        }
        Ok(())
    }

    fn json(&self) -> bool {
        self.perf_format == "json"
    }

    /// `--filter`, else `ROCK_TEST_FILTER`.
    pub(crate) fn test_filter(&self) -> Result<Option<crate::TestFilter>> {
        let text = self
            .filter
            .clone()
            .or_else(|| std::env::var("ROCK_TEST_FILTER").ok());
        crate::test::TestFilter::parse(text.as_deref())
    }

    pub(crate) fn get_required_file(&self, command_name: &str) -> Result<&str> {
        let Some(file) = self.files.first() else {
            anyhow::bail!(
                "Missing file path for `{command_name}` command.\n\
                 Usage: rock {command_name} <file.rock>",
            );
        };
        reject_device_path(file)?;
        Ok(file)
    }

    pub(crate) fn get_compile_output(&self) -> Result<Option<String>> {
        let output = match (&self.output, self.files.len()) {
            (Some(_), n) if n >= 2 => {
                anyhow::bail!(
                    "Cannot specify both --output flag and positional output argument.\n\
                     Use either: 'rock compile input.rock --output output.lua' or 'rock compile input.rock output.lua'"
                );
            }
            (None, n) if n > 2 => {
                anyhow::bail!(
                    "Too many arguments provided. Expected: 'rock compile input.rock [output.lua]'\n\
                     Use --output flag for explicit output file specification."
                );
            }
            (Some(output), _) => Some(output.clone()),
            (None, 2) => Some(self.files[1].clone()),
            (None, _) => None,
        };
        if let Some(out) = &output {
            reject_device_output(out)?;
        }
        Ok(output)
    }

    /// Resolve a project directory using this build's project conventions.
    pub(crate) fn get_project_dir_or_current(&self) -> Result<String> {
        let arg = self.get_path_or_current()?;
        Ok(rock_engine_host::project::resolve_project_dir(&arg)?
            .to_string_lossy()
            .into_owned())
    }

    pub(crate) fn get_path_or_current(&self) -> Result<String> {
        let path = self.files.first().cloned().unwrap_or_else(|| ".".into());
        reject_device_path(&path)?;
        Ok(path)
    }

    /// Every path argument, for the commands that act on a list rather than on
    /// one entry point.
    pub(crate) fn get_paths_or_current(&self) -> Result<Vec<String>> {
        self.paths_or_default(".")
    }

    pub(crate) fn get_test_files(&self) -> Result<Vec<String>> {
        self.paths_or_default("tests")
    }

    fn paths_or_default(&self, default: &str) -> Result<Vec<String>> {
        let files = if self.files.is_empty() {
            vec![default.to_owned()]
        } else {
            self.files.clone()
        };
        for file in &files {
            reject_device_path(file)?;
        }
        Ok(files)
    }
}

/// Special device files cannot serve as an input path: the compiler reads them
/// more than once and a stream cannot be rewound.
fn reject_device_path(path: &str) -> Result<()> {
    if path == "/dev/stdin" || path == "/dev/stdout" || path == "/dev/stderr" {
        anyhow::bail!(
            "Cannot use {} as input path. Special device files are not supported.\n\
             Please provide a regular file or directory path.",
            path
        );
    }
    Ok(())
}

fn reject_device_output(path: &str) -> Result<()> {
    if path == "/dev/stdout" {
        anyhow::bail!(
            "Cannot use /dev/stdout as output path.\n\
             The compiler automatically prints to stdout when no output is specified.\n\
             Usage: 'rock compile input.rock'"
        );
    }
    if path == "/dev/stdin" || path == "/dev/stderr" {
        anyhow::bail!(
            "Cannot use {} as output path. Special device files are not supported.",
            path
        );
    }
    Ok(())
}

/// `rock lsp` owns its logger, so it runs before the process installs the CLI
/// logger. `args` follow the `lsp` argument.
pub(crate) fn run_lsp(args: impl Iterator<Item = String>) -> i32 {
    let mut forwarded = vec!["rock lsp".to_string()];
    forwarded.extend(args);
    match rock_lsp::server_cli::run_cli(forwarded) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
