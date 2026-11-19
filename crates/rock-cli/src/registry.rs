//! Command, backend and option tables and the runtime types they share.

use crate::{Backend, CommandContext};
use anyhow::Result;

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

pub type Handler = fn(&CommandContext) -> Result<()>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Test,
    Compile,
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
        CliCommand::all()
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
}

/// Commands are Clap subcommands in registration order.
pub struct CliCommand {
    pub name: &'static str,
    pub route: Route,
}

impl CliCommand {
    pub(crate) fn cli_name(&self) -> &'static str {
        self.name
    }

    pub fn operation(&self) -> Option<Operation> {
        match self.route {
            Route::Backend(op) => Some(op),
            Route::Direct(_) => None,
        }
    }

    pub fn named(name: &str) -> Option<&'static CliCommand> {
        Self::all().iter().copied().find(|c| c.name == name)
    }

    pub fn all() -> &'static [&'static CliCommand] {
        crate::plain::COMMANDS
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
        crate::plain::BACKENDS
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
        let OmittedBackend::Feature { feature, hint } = omitted;
        Some(format!(
            "the `{}` backend was omitted from this build; enable the `{}` feature ({})",
            spec.name, feature, hint,
        ))
    }

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
    /// `--filter` / ROCK_TEST_FILTER: only `rock test`, and only on a backend
    /// without a `filter_rejection`.
    TestFilter,
    /// `--skip-gpu`: GLSL tests and shader validation only.
    SkipGpu,
}

impl OptionRule {
    pub fn all() -> &'static [OptionRule] {
        crate::plain::OPTION_RULES
    }
}
