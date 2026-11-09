//! Static CLI registrations. Library callers use the plain public default;
//! a process may install its host table once, before parsing or dispatch.
//!
//! Cross-crate composition uses `extend` to concatenate static commands and
//! backend operations at compile time. Backends and host tables use Rust struct
//! updates to inherit public entries, including option rules. The private host
//! replaces runtime policy as one service table. No copied public handlers or
//! runtime table allocation are needed. Reading the default does not install it or block a later host.

use crate::{Args, Backend, CommandContext};
use anyhow::Result;

/// Static host tables; uninstalled library callers use `plain::REGISTRATIONS`.
pub struct Registrations {
    pub commands: &'static [&'static CliCommand],
    pub backends: &'static [BackendSpec],
    /// Extend the public parser with host-owned definitions.
    pub arguments: fn(clap::Command) -> clap::Command,
    /// Decode only the options installed by this host's argument definitions.
    pub decode_arguments: fn(&clap::ArgMatches, &mut Args),
    /// Host exclusions for automatic bin discovery; never applied to the root.
    pub scan_excluded: fn(&std::path::Path) -> bool,
    pub command_metadata: fn(&CliCommand) -> CommandMetadata,
    pub option_rules: &'static [OptionRule],
    /// Optional recipes exposed under the public `fix` command.
    pub fixes: &'static [FixHandler],
    pub early_entries: &'static [EarlyEntry],
    /// Process-only environment policy, called after parsing and before worker threads.
    pub initialize_process: fn(&Args),
    pub runtime_services: &'static RuntimeServices,
}

/// Host argument groups retain their place among shared options in help output.
pub struct ArgumentGroup {
    pub before: &'static str,
    pub arguments: Vec<clap::Arg>,
}

/// Add registered arguments without duplicating the shared argument catalog.
pub fn extend_arguments(mut command: clap::Command, groups: Vec<ArgumentGroup>) -> clap::Command {
    let mut order = Vec::new();
    for arg in command.get_arguments() {
        for group in groups.iter().filter(|group| arg.get_id() == group.before) {
            order.extend(group.arguments.iter().map(|arg| arg.get_id().clone()));
        }
        order.push(arg.get_id().clone());
    }
    for group in groups {
        assert!(
            command
                .get_arguments()
                .any(|arg| arg.get_id() == group.before),
            "host argument anchor {} is absent",
            group.before
        );
        command = command.args(group.arguments);
    }
    command.mut_args(|arg| {
        let index = order
            .iter()
            .position(|id| id == arg.get_id())
            .expect("argument ordered");
        arg.display_order(index)
    })
}

#[derive(Clone, Copy)]
pub enum CompilerProcess {
    Never,
    Always,
    WhenListed,
}

/// Process policy resolved before allocator admission and host initialization.
pub struct CommandMetadata {
    pub log_invocation: bool,
    pub compiler_process: CompilerProcess,
    pub build_command: Option<&'static str>,
    pub implied_channel: Option<&'static str>,
}

static REGISTRATIONS: std::sync::OnceLock<&'static Registrations> = std::sync::OnceLock::new();

/// Install one explicit host table before parsing or dispatching its commands.
pub fn install(registrations: &'static Registrations) {
    assert!(
        REGISTRATIONS.set(registrations).is_ok(),
        "CLI registrations already installed"
    );
}

pub fn registered() -> &'static Registrations {
    REGISTRATIONS
        .get()
        .copied()
        .unwrap_or(&crate::plain::REGISTRATIONS)
}

/// Checked native FFI libraries, independent of package names and aliases.
pub struct TestProgramFacts {
    pub ffi_libraries: Vec<String>,
}

pub enum TestUnit<'a> {
    File(&'a std::path::Path),
    Package {
        dir: &'a str,
        facts: &'a TestProgramFacts,
    },
}

/// Runtime text surrounding the emitted definitions. Native staging is done
/// by `program_host` before resolving the preamble, preserving failure order.
pub struct ProgramHost {
    pub preamble: String,
    pub setup: &'static str,
}

#[derive(Debug)]
pub struct RuntimeFailure {
    pub reason: &'static str,
    pub rendered: String,
}

/// Ordinary compile omits runtime globals for non-games. Game-only builders
/// (frame, sim and watch) always retain them, including their cache identity.
pub enum ProgramKind {
    Project,
    Game,
}

/// Runtime preparation and execution shared by compiler and game commands.
pub struct RuntimeServices {
    /// Resolve a project argument against the configured project roots.
    pub resolve_project_dir: fn(&str) -> Result<std::path::PathBuf>,
    /// Host-owned project recognition, shared by watch and production emission.
    pub is_game_project: fn(&std::path::Path) -> Result<bool>,
    pub configured_runtime_dir: fn(&std::path::Path) -> Result<Option<std::path::PathBuf>>,
    /// Stage native libraries requested by game compilation.
    pub build_engine_libs: fn(&str) -> Result<()>,
    pub prepare_unit: fn(&str) -> Result<()>,
    pub prepare_sound_bank: fn(&str) -> Result<Option<std::path::PathBuf>>,
    pub sound_project_path: fn(&str) -> Result<Option<std::path::PathBuf>>,
    /// Prepare runtime inputs before public emission, preserving build-time asset reads.
    pub prepare_run: fn(&str) -> Result<()>,
    /// Own project runtime selection, native staging, file layout and execution.
    pub launch: fn(&str, &crate::skull_backend::GameProgram, bool) -> Result<()>,
    pub program_preamble: fn(&str, ProgramKind) -> Result<String>,
    pub program_host: fn(TestUnit<'_>) -> std::result::Result<ProgramHost, RuntimeFailure>,
    /// Selection belongs to the private runner; compiler code requests it through this seam.
    pub resolve_sim_specs: fn(&[String], &[String]) -> Result<Vec<String>>,
}

impl RuntimeServices {
    /// Read the installed host services, or the public plain default.
    pub fn registered() -> &'static Self {
        registered().runtime_services
    }
}

pub type Handler = fn(&CommandContext) -> Result<()>;

/// A build's optional fix flag, help and implementation; the public imports
/// fixer does not depend on any host recipe.
pub struct FixHandler {
    pub flag: &'static str,
    pub help: &'static str,
    pub usage: &'static str,
    pub run: Handler,
}

/// The operations a backend serves. A command whose route is
/// `Route::Backend(op)` is dispatched to the selected backend's handler for
/// `op`; the command registration supplies the user-facing name.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Test,
    Compile,
    CompileSingle,
    TestCompile,
    Run,
    Watch,
    Stone,
    Frame,
    Perf,
    Sim,
}

impl Operation {
    /// The command registered for this operation.
    pub fn command(self) -> &'static CliCommand {
        registered()
            .commands
            .iter()
            .copied()
            .find(|c| c.operation() == Some(self))
            .unwrap_or_else(|| panic!("no command is registered for backend operation {self:?}"))
    }
}

#[derive(Copy, Clone)]
pub enum Route {
    /// Runs its own pipeline and ignores `--backend`.
    Direct(Handler),
    /// Served by the selected backend through the capability gate.
    Backend(Operation),
    /// Compiled out of this build; the message names the feature to enable.
    Omitted(&'static str),
}

/// One `rock <command>` registration. Commands are positional clap values,
/// not clap subcommands; the accepted names are the registered names in
/// registration order.
pub struct CliCommand {
    pub name: &'static str,
    pub route: Route,
}

impl CliCommand {
    /// User-facing kebab-case name (what the user types and what error
    /// messages echo).
    pub(crate) fn cli_name(&self) -> &'static str {
        self.name
    }

    pub fn operation(&self) -> Option<Operation> {
        match self.route {
            Route::Backend(op) => Some(op),
            Route::Direct(_) | Route::Omitted(_) => None,
        }
    }

    pub fn named(name: &str) -> Option<&'static CliCommand> {
        registered()
            .commands
            .iter()
            .copied()
            .find(|c| c.name == name)
    }

    pub fn all() -> &'static [&'static CliCommand] {
        registered().commands
    }

    /// The clap parser for the positional `command` argument.
    pub(crate) fn value_parser() -> impl clap::builder::TypedValueParser<Value = &'static CliCommand>
    {
        use clap::builder::TypedValueParser;
        clap::builder::PossibleValuesParser::new(
            Self::all()
                .iter()
                .map(|c| clap::builder::PossibleValue::new(c.name)),
        )
        .map(|name| {
            Self::named(&name).expect("the possible-values parser admits only registered names")
        })
    }
}

impl PartialEq for CliCommand {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for CliCommand {}

impl std::fmt::Debug for CliCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

/// Why a recognized backend is unavailable in this build.
#[derive(Clone, Copy)]
pub enum OmittedBackend {
    /// Not compiled into this binary at all.
    Build,
    Feature {
        feature: &'static str,
        hint: &'static str,
    },
}

/// One `--backend` registration.
#[derive(Clone, Copy)]
pub struct BackendSpec {
    pub backend: Backend,
    pub name: &'static str,
    /// One-clause description of what the backend is, for the reject message.
    pub tagline: &'static str,
    pub omitted: Option<OmittedBackend>,
    /// Served operations in the order the reject message lists them.
    pub operations: &'static [(Operation, Handler)],
    /// Why `rock test` on this backend rejects `--filter` / ROCK_TEST_FILTER;
    /// `None` when the backend's test runner applies the filter.
    pub filter_rejection: Option<&'static str>,
}

impl BackendSpec {
    pub fn all() -> &'static [BackendSpec] {
        registered().backends
    }

    pub fn named(name: &str) -> Option<&'static BackendSpec> {
        Self::all().iter().find(|b| b.name == name)
    }

    pub fn handler(&self, op: Operation) -> Option<Handler> {
        self.operations
            .iter()
            .find(|(o, _)| *o == op)
            .map(|(_, h)| *h)
    }

    /// The clap parser for `--backend`.
    pub(crate) fn value_parser() -> impl clap::builder::TypedValueParser<Value = Backend> {
        use clap::builder::TypedValueParser;
        clap::builder::PossibleValuesParser::new(
            Self::all()
                .iter()
                .map(|b| clap::builder::PossibleValue::new(b.name)),
        )
        .map(|name| {
            Self::named(&name)
                .expect("the possible-values parser admits only registered names")
                .backend
        })
    }
}

impl Backend {
    pub fn spec(self) -> &'static BackendSpec {
        BackendSpec::all()
            .iter()
            .find(|b| b.backend == self)
            .unwrap_or_else(|| panic!("backend {self:?} has no registration"))
    }

    /// `--backend` value name, used in messages.
    pub fn name(self) -> &'static str {
        self.spec().name
    }

    /// The commands a backend serves through the backend switch, in the order
    /// the reject message lists them.
    pub(crate) fn supported_commands(self) -> Vec<&'static CliCommand> {
        self.spec()
            .operations
            .iter()
            .map(|(op, _)| op.command())
            .collect()
    }

    /// Whether this backend can serve `cmd` through the backend switch.
    pub fn serves(self, cmd: &CliCommand) -> bool {
        cmd.operation()
            .is_some_and(|op| self.spec().handler(op).is_some())
    }

    pub(crate) fn omitted_reason(self) -> Option<String> {
        let spec = self.spec();
        let omitted = spec.omitted.as_ref()?;
        Some(match omitted {
            OmittedBackend::Build => {
                format!("the `{}` backend is not included in this build", spec.name)
            }
            OmittedBackend::Feature { feature, hint } => format!(
                "the `{}` backend was omitted from this build; enable the `{}` feature ({})",
                spec.name, feature, hint,
            ),
        })
    }

    /// `Some(message)` if this backend cannot serve `cmd`; `None` if it can.
    /// The capability gate (`require_backend_support`) is the only caller.
    pub(crate) fn reject_reason(self, cmd: &CliCommand) -> Option<String> {
        if let Some(message) = self.omitted_reason() {
            return Some(message);
        }
        if self.serves(cmd) {
            return None;
        }
        let names: Vec<&str> = self
            .supported_commands()
            .iter()
            .map(|c| c.cli_name())
            .collect();
        Some(format!(
            "the `{}` backend cannot serve `{}` \u{2014} {}. Supported commands: {}.",
            self.name(),
            cmd.cli_name(),
            self.spec().tagline,
            names.join(", "),
        ))
    }
}

/// Validation of an option some backends do not accept, checked in
/// registration order before dispatch.
#[derive(Clone, Copy)]
pub enum OptionRule {
    /// `flag` is accepted only by `rock <operation> --backend <backend>`.
    Owned {
        flag: &'static str,
        given: fn(&Args) -> bool,
        backend: Backend,
        operation: Operation,
    },
    /// `--filter` / ROCK_TEST_FILTER: only `rock test`, and only on a backend
    /// without a `filter_rejection`.
    TestFilter,
    /// `--skip-gpu`: GLSL tests and shader validation only.
    SkipGpu,
}

impl OptionRule {
    pub fn all() -> &'static [OptionRule] {
        registered().option_rules
    }
}

/// A raw argument handler that runs before ordinary process initialization
/// (allocator defaults, the logger, clap). `run` receives the arguments after
/// the entry name and returns the process exit code.
pub struct EarlyEntry {
    pub name: &'static str,
    pub run: fn(Vec<String>) -> i32,
}

/// Run the early entry named by the first argument, if one is registered,
/// calling `prepare` first. `args` excludes argv[0].
pub(crate) fn run_early_entry(
    mut args: impl Iterator<Item = String>,
    prepare: impl FnOnce(),
) -> Option<i32> {
    let first = args.next()?;
    let entry = registered()
        .early_entries
        .iter()
        .find(|e| e.name == first)?;
    prepare();
    Some((entry.run)(args.collect()))
}

/// Concatenate two nonempty-in-total static tables into a fixed-size array.
/// `N` is the sum of both lengths; mismatches fail during const evaluation.
pub const fn extend<T: Copy, const N: usize>(base: &[T], extra: &[T]) -> [T; N] {
    assert!(N == base.len() + extra.len());
    let first = if base.is_empty() { extra[0] } else { base[0] };
    let mut result = [first; N];
    let mut i = 0;
    while i < N {
        result[i] = if i < base.len() {
            base[i]
        } else {
            extra[i - base.len()]
        };
        i += 1;
    }
    result
}
