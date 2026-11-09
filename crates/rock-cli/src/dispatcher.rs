use crate::registry::{OptionRule, Route};
use crate::*;

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

pub struct CommandContext {
    pub args: Args,
}

impl CommandContext {
    pub fn new(args: Args) -> Self {
        Self { args }
    }

    /// Run the command directly without server
    pub fn run(&self) -> Result<()> {
        self.require_backend_options()?;
        if self.args.verbose {
            log::debug!(target: "CLI", "using direct compilation");
        }
        self.require_optional_features()?;
        // `--profile` is global in the clap definition but only `rock shader`
        // acts on it: the profile is a property of the GL context, and the
        // engine adapts the header per context at shader-compile time. Every
        // other command would accept-and-ignore it, which reads as "ES was
        // selected" while emitting desktop headers.
        // The build lint pass runs deep inside the backends, far from `Args`, so
        // the reporting policy is installed once here.
        crate::lint_pass::set_policy(if self.args.no_lints {
            crate::lint_pass::LintReport::Off
        } else if self.args.lint_summary {
            crate::lint_pass::LintReport::Summary
        } else {
            crate::lint_pass::LintReport::Full
        });
        if self.args.profile.is_some() && self.args.command.cli_name() != "shader" {
            anyhow::bail!(
                "rock {}: --profile applies only to `rock shader` (offline GLSL emission). \
                 In-game shader source is emitted once and adapted to the live GL context by \
                 the engine, so no other command selects a profile. `rock shader-validate` \
                 gates both profiles and takes no flag.",
                self.args.command.cli_name(),
            );
        }
        self.run_command()
    }

    fn require_optional_features(&self) -> Result<()> {
        if let Route::Omitted(message) = self.args.command.route {
            anyhow::bail!("{}", message);
        }
        Ok(())
    }

    pub(crate) fn get_required_file(&self, command_name: &str) -> Result<&str> {
        let Some(file) = self.args.files.first() else {
            anyhow::bail!(
                "Missing file path for `{cmd}` command.\n\
                 Usage: rock {cmd} <file.rock>",
                cmd = command_name,
            );
        };

        if file == "/dev/stdin" || file == "/dev/stdout" || file == "/dev/stderr" {
            anyhow::bail!(
                "Cannot use {} as input file. Special device files are not supported.\n\
                 Please provide a regular file path.",
                file
            );
        }

        Ok(file)
    }

    /// Get output file for compile command, handling both --output flag and positional argument
    pub(crate) fn get_compile_output(&self) -> Result<Option<String>> {
        let output = match (&self.args.output, self.args.files.len()) {
            // Both --output flag and second positional argument provided - error
            (Some(_), n) if n >= 2 => {
                anyhow::bail!(
                    "Cannot specify both --output flag and positional output argument.\n\
                     Use either: 'rock compile input.rock --output output.lua' or 'rock compile input.rock output.lua'"
                );
            }
            // Too many arguments provided
            (None, n) if n > 2 => {
                anyhow::bail!(
                    "Too many arguments provided. Expected: 'rock compile input.rock [output.lua]'\n\
                     Use --output flag for explicit output file specification."
                );
            }
            // --output flag provided
            (Some(output), _) => Some(output.clone()),
            // Second positional argument as output
            (None, 2) => Some(self.args.files[1].clone()),
            // No output specified
            (None, _) => None,
        };

        // Validate that output is not a special device file
        if let Some(ref out) = output {
            if out == "/dev/stdout" {
                anyhow::bail!(
                    "Cannot use /dev/stdout as output file.\n\
                     The compiler automatically prints to stdout when no output is specified.\n\
                     Usage: 'rock compile input.rock'"
                );
            } else if out == "/dev/stdin" || out == "/dev/stderr" {
                anyhow::bail!(
                    "Cannot use {} as output file. Special device files are not supported.\n\
                     To print to stdout, omit the output argument: 'rock compile input.rock'",
                    out
                );
            }
        }

        Ok(output)
    }

    /// Resolve a project directory using this build's project conventions.
    pub(crate) fn get_project_dir_or_current(&self) -> Result<String> {
        let arg = self.get_path_or_current()?;
        Ok(
            (crate::registry::RuntimeServices::registered().resolve_project_dir)(&arg)?
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub(crate) fn get_path_or_current(&self) -> Result<String> {
        let path = self
            .args
            .files
            .first()
            .cloned()
            .unwrap_or_else(|| ".".to_string());

        reject_device_path(&path)?;

        Ok(path)
    }

    /// Every path argument, for the commands that act on a list rather than on
    /// one entry point. `get_path_or_current` drops everything after the first.
    pub(crate) fn get_paths_or_current(&self) -> Result<Vec<String>> {
        let paths = if self.args.files.is_empty() {
            vec![".".to_string()]
        } else {
            self.args.files.clone()
        };

        for path in &paths {
            reject_device_path(path)?;
        }

        Ok(paths)
    }

    pub fn get_output_or_default(&self, default: &str) -> Result<String> {
        let output = self
            .args
            .output
            .clone()
            .unwrap_or_else(|| default.to_string());

        if output == "/dev/stdout" {
            anyhow::bail!(
                "Cannot use /dev/stdout as output path.\n\
                 The compiler automatically prints to stdout when no output is specified.\n\
                 Usage: 'rock compile input.rock'"
            );
        } else if output == "/dev/stdin" || output == "/dev/stderr" {
            anyhow::bail!(
                "Cannot use {} as output path. Special device files are not supported.",
                output
            );
        }

        Ok(output)
    }

    pub(crate) fn get_test_files(&self) -> Result<Vec<String>> {
        let files = if self.args.files.is_empty() {
            vec!["tests".to_string()]
        } else {
            self.args.files.clone()
        };

        for file in &files {
            if file == "/dev/stdin" || file == "/dev/stdout" || file == "/dev/stderr" {
                anyhow::bail!(
                    "Cannot use {} as input file. Special device files are not supported.\n\
                     Please provide regular file paths.",
                    file
                );
            }
        }

        Ok(files)
    }

    /// The one capability gate: reject a `--backend`/command combination the
    /// selected backend's registration does not serve, with a uniform error
    /// whose "Supported" list is generated from the registration.
    fn require_backend_support(&self) -> Result<()> {
        if let Some(msg) = self.args.backend.reject_reason(self.args.command) {
            anyhow::bail!("{}", msg);
        }
        Ok(())
    }

    fn run_command(&self) -> Result<()> {
        match self.args.command.route {
            Route::Direct(handler) => handler(self),
            Route::Backend(op) => {
                self.require_backend_support()?;
                let handler = self.args.backend.spec().handler(op).unwrap_or_else(|| {
                    unreachable!(
                        "the capability gate admitted {:?} on the `{}` backend without a registered handler",
                        op,
                        self.args.backend.name()
                    )
                });
                handler(self)
            }
            Route::Omitted(_) => unreachable!(
                "`{}` is omitted from this build; require_optional_features rejects it first",
                self.args.command.cli_name()
            ),
        }
    }

    fn require_backend_options(&self) -> Result<()> {
        for rule in OptionRule::all() {
            match rule {
                OptionRule::Owned {
                    flag,
                    given,
                    backend,
                    operation,
                } => {
                    if given(&self.args)
                        && (self.args.command.operation() != Some(*operation)
                            || self.args.backend != *backend)
                    {
                        anyhow::bail!(
                            "`{}` is supported only by `rock {} --backend {}`",
                            flag,
                            operation.command().cli_name(),
                            backend.name()
                        );
                    }
                }
                OptionRule::TestFilter => self.require_test_filter_support()?,
                OptionRule::SkipGpu => {
                    if self.args.skip_gpu
                        && self.args.command.cli_name() != "shader-validate"
                        && !(self.args.command.operation() == Some(Operation::Test)
                            && self.args.backend == Backend::Glsl)
                    {
                        anyhow::bail!(
                            "`--skip-gpu` is supported only by `rock test --backend glsl` and `rock shader-validate`"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn test_filter(&self) -> Result<Option<crate::TestFilter>> {
        crate::test::TestFilter::parse(self.args.test_filter.as_deref())
    }

    fn require_test_filter_support(&self) -> Result<()> {
        if self.args.command.operation() != Some(Operation::Test) {
            if self.args.test_filter_explicit {
                anyhow::bail!("`--filter` applies only to `rock test`");
            }
            return Ok(());
        }
        let Some(_) = self.test_filter()? else {
            return Ok(());
        };
        if let Some(message) = self.args.backend.spec().filter_rejection {
            anyhow::bail!("{}", message);
        }
        Ok(())
    }
}
