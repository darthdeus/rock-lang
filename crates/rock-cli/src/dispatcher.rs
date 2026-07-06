use crate::*;

pub struct CommandContext {
    pub args: Args,
}

impl CommandContext {
    pub fn new(args: Args) -> Self {
        Self { args }
    }

    /// Run the command directly without server
    pub fn run(&self) -> Result<()> {
        if self.args.verbose {
            log::debug!(target: "CLI", "using direct compilation");
        }
        self.run_command()
    }

    pub fn get_required_file(&self, command_name: &str) -> Result<&str> {
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
    pub fn get_compile_output(&self) -> Result<Option<String>> {
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

    pub fn get_game_file(&self) -> String {
        let path = self.args
            .files
            .first()
            .cloned()
            .unwrap_or_else(|| "game".to_string());

        // If using default and it doesn't exist, provide helpful error
        if self.args.files.is_empty() && !std::path::Path::new(&path).exists() {
            self.suggest_available_directories(&path);
        }

        path
    }

    fn suggest_available_directories(&self, missing_dir: &str) {
        use fs_err as fs;

        log::error!(target: "FRAME", "directory '{}' not found", missing_dir);

        // Try to find directories that look like game projects
        let mut found_dirs = Vec::new();

        if let Ok(entries) = fs::read_dir(".") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

                    // Skip common non-game directories
                    if matches!(name.as_str(), "target" | "rock" | "stone" | "src" | "docs" | "examples" | "libs" | ".git" | ".github" | "node_modules") {
                        continue;
                    }

                    // Check if it looks like a game directory (has .rock files)
                    if let Ok(sub_entries) = fs::read_dir(&path) {
                        let has_rock_files = sub_entries.flatten().any(|e| {
                            e.path().extension().map_or(false, |ext| ext == "rock")
                        });

                        if has_rock_files {
                            found_dirs.push(name);
                        }
                    }
                }
            }
        }

        // Same level as the "not found" error above: these are the
        // actionable recovery hints for that fatal error and emit
        // immediately before exit(1) — at info they'd be suppressed
        // under STONE_LOG=warn|error, leaving a bare error with no
        // guidance (the original unconditional eprintln! always showed
        // them).
        if !found_dirs.is_empty() {
            found_dirs.sort();
            log::error!(target: "FRAME", "available game directories:");
            for dir in found_dirs {
                log::error!(target: "FRAME", "  rock frame {}", dir);
            }
        } else {
            log::error!(target: "FRAME", "no game directories found with .rock files");
        }

        log::error!(target: "FRAME", "usage: rock frame [GAME_DIR] [OUTPUT_DIR]");
        log::error!(target: "FRAME", "example: rock frame bitgun tmp");
        std::process::exit(1);
    }

    pub fn get_path_or_current(&self) -> Result<String> {
        let path = self.args
            .files
            .first()
            .cloned()
            .unwrap_or_else(|| ".".to_string());

        if path == "/dev/stdin" || path == "/dev/stdout" || path == "/dev/stderr" {
            anyhow::bail!(
                "Cannot use {} as input path. Special device files are not supported.\n\
                 Please provide a regular file or directory path.",
                path
            );
        }

        Ok(path)
    }

    pub fn get_output_or_default(&self, default: &str) -> Result<String> {
        let output = self.args
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

    pub fn get_test_files(&self) -> Result<Vec<String>> {
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

    /// `rock frame` on the skull backend (do_one_frame is backend-parameterized).
    fn handle_frame(&self) -> Result<()> {
        let file = self.get_game_file();
        let output = self.get_output_or_default("tmp")?;
        do_one_frame(
            self.args.backend,
            &file,
            output,
            self.args.frames,
            self.args.headless,
            self.args.dump_state,
            &self.args.app_args,
        )
    }

    /// `rock perf` on the skull backend.
    fn handle_perf(&self) -> Result<()> {
        let file = self.get_game_file();
        let output = self.get_output_or_default("tmp")?;
        // Default perf run: 12 wall-clock seconds. Overridable via --seconds or
        // --frames. (300 frames / ~5s was too short for stable distribution stats.)
        let user_set_frames = self.args.frames != 1;
        let seconds = self
            .args
            .seconds
            .or(if user_set_frames { None } else { Some(12.0) });
        do_perf(
            self.args.backend,
            &file,
            output,
            self.args.frames,
            seconds,
            self.args.warmup_seconds,
            self.args.gc_window,
            &self.args.perf_format,
            &self.args.app_args,
        )
    }

    /// `rock sim` on the active backend (list / run-all / run-one). Single source
    /// for both the rock-default and skull dispatch arms; the backend selects the
    /// compile + `@sim` collection (rock: `unified_compile` + `sim_registry`;
    /// skull: the bone workspace + skull's emitted registry).
    fn handle_sim(&self) -> Result<()> {
        let backend = self.args.backend;
        if self.args.list {
            if self.args.files.is_empty() {
                anyhow::bail!("sim --list needs a project: rock sim --list <project>");
            }
            let project = &self.args.files[0];
            crate::run_sim::do_list_sims(backend, project)
        } else if self.args.files.is_empty() {
            anyhow::bail!("sim needs a project: rock sim <project> [name]");
        } else if self.args.files.len() == 1 {
            // No name: run the whole suite.
            let project = &self.args.files[0];
            crate::run_sim::do_run_all_sims(
                backend,
                project,
                self.args.quiet,
                self.args.update_snapshots,
            )
        } else {
            let project = &self.args.files[0];
            let name = &self.args.files[1];
            crate::run_sim::do_run_sim_opts(
                backend,
                project,
                name,
                self.args.quiet,
                self.args.update_snapshots,
            )
        }
    }

    /// The one capability gate. Reject `--backend`/command combinations a
    /// restricted backend can't serve, with a uniform error whose "Supported"
    /// list is generated from `Backend::supported_commands` — so the policy
    /// lives in one data table, not in three hand-maintained reject arms.
    /// Self-routing commands never reach here (dispatched before the switch);
    /// the rock backend is unrestricted (`serves` is always true).
    fn require_backend_support(&self) -> Result<()> {
        if let Some(msg) = self.args.backend.reject_reason(self.args.command) {
            anyhow::bail!("{}", msg);
        }
        Ok(())
    }

    /// Dispatch the command through the `bone` typechecker backend.
    ///
    /// Bone can parse, lower, and type-check Rock source but has no codegen or
    /// runtime, so only the type-check-capable commands are routed here. Anything
    /// that needs to execute a program (or otherwise has no bone equivalent) is
    /// rejected with a clear error instead of silently falling through to the Lua
    /// pipeline.
    fn run_command_bone(&self) -> Result<()> {
        use crate::bone_backend;
        match self.args.command {
            // Unified test path: skull subsumes bone (it type-checks via the same
            // bone front end, then runs), so `test --backend bone` and `--backend
            // skull` are the SAME -- both go through skull_test (@test execution +
            // LSP + error suites).
            CliCommand::Test => {
                let files = self.get_test_files()?;
                crate::skull_backend::skull_test(&files)
            }
            CliCommand::Compile | CliCommand::CompileSingle | CliCommand::TestCompile => {
                let file = self.get_required_file("compile")?;
                bone_backend::bone_check_single(file, "compile", self.args.iters)
            }
            CliCommand::Run => {
                let file = self.get_required_file("run")?;
                bone_backend::bone_check_single(file, "run", self.args.iters)
            }
            CliCommand::Watch => {
                let path = self.get_path_or_current()?;
                bone_backend::bone_watch(&path)
            }
            // `require_backend_support` (called before this dispatch) has
            // already rejected anything bone can't serve, so the remaining
            // commands are exactly `Backend::Bone.supported_commands()`.
            other => unreachable!(
                "bone dispatch reached unsupported command {:?}; the capability \
                 gate should have rejected it — Backend::supported_commands and \
                 run_command_bone have diverged",
                other
            ),
        }
    }

    /// Dispatch through the `skull` backend (bone type-check + skull codegen ->
    /// Lua, executed via luajit). Only the codegen-capable commands route here;
    /// anything else is rejected, like the bone path.
    fn run_command_skull(&self) -> Result<()> {
        use crate::skull_backend;
        match self.args.command {
            CliCommand::Test => {
                let files = self.get_test_files()?;
                skull_backend::skull_test(&files)
            }
            CliCommand::Compile | CliCommand::CompileSingle | CliCommand::TestCompile => {
                let file = self.get_required_file("compile")?;
                skull_backend::skull_compile_single(file, self.get_compile_output()?)
            }
            CliCommand::Run => {
                let file = self.get_required_file("run")?;
                skull_backend::skull_run(file)
            }
            CliCommand::Watch => {
                let path = self.get_path_or_current()?;
                skull_backend::skull_watch(&path, self.args.output.clone())
            }
            CliCommand::Stone => {
                // Hot-reload game dev loop on skull: the watch/libstone/game-process
                // machinery is shared; only the compile step re-emits via skull.
                let path = self.get_path_or_current()?;
                crate::stone::do_stone_watch(
                    &path,
                    self.args.quiet,
                    &self.args.app_args,
                    crate::Backend::Skull,
                )
            }
            // frame/perf/sim are backend-parameterized: the compile + @sim
            // collection routes through skull, the spawn/profile/run half is shared.
            CliCommand::Frame => self.handle_frame(),
            CliCommand::Perf => self.handle_perf(),
            CliCommand::Sim => self.handle_sim(),
            // See the bone fallthrough: the capability gate already rejected
            // anything skull can't serve.
            other => unreachable!(
                "skull dispatch reached unsupported command {:?}; the capability \
                 gate should have rejected it — Backend::supported_commands and \
                 run_command_skull have diverged",
                other
            ),
        }
    }

    /// Run command directly
    fn run_command(&self) -> Result<()> {
        // iter-compile is a bone-inference profiling loop; it ignores --backend
        // and runs its own pipeline, so dispatch it before the backend switch.
        if self.args.command == CliCommand::IterCompile {
            let path = self.get_path_or_current()?;
            return crate::bone_backend::bone_iter_inference(&path, self.args.iters, self.args.warmup);
        }
        // iter-parse is a front-end (tokenize+parse) profiling loop; the
        // front-end is backend-independent, so dispatch before the backend switch.
        if self.args.command == CliCommand::IterParse {
            let path = self.get_path_or_current()?;
            return crate::iter_parse::iter_parse_skull(&path, self.args.iters);
        }
        if self.args.command == CliCommand::IterTokenize {
            let path = self.get_path_or_current()?;
            return crate::iter_parse::iter_tokenize(&path, self.args.iters);
        }
        // iter-codegen builds the bone workspace once and re-runs skull's emit;
        // skull-specific, dispatched before the backend switch like iter-compile.
        if self.args.command == CliCommand::IterCodegen {
            let path = self.get_path_or_current()?;
            return crate::skull_backend::skull_iter_codegen(&path, self.args.iters);
        }
        // backend-stats runs BOTH backends itself (bone semantic vs skull
        // semantic+emit), so it ignores --backend and dispatches before the
        // backend switch like the iter-* profilers.
        if self.args.command == CliCommand::BackendStats {
            let input = self.get_path_or_current()?;
            return crate::backend_stats::do_backend_stats(&input, self.args.iters, self.args.warmup);
        }
        // skull-inline-stats replays the inliner over the workspace and writes a
        // diagnostic report; skull-specific, dispatched early like iter-codegen.
        if self.args.command == CliCommand::SkullInlineStats {
            let path = self.get_path_or_current()?;
            return crate::skull_backend::skull_inline_stats(&path);
        }
        // skull-incremental-profile builds a bone Session + skull emit cache and
        // profiles per-file incremental recompile; skull-specific, dispatched
        // before the backend switch like the other skull diagnostics.
        if self.args.command == CliCommand::SkullIncrementalProfile {
            let path = self.get_path_or_current()?;
            return crate::skull_backend::skull_incremental_profile(&path);
        }
        // iter-incremental is the tight-loop sampling profiler for skull's
        // incremental recompile; skull-specific, dispatched early.
        if self.args.command == CliCommand::IterIncremental {
            let path = self.get_path_or_current()?;
            return crate::skull_backend::skull_iter_incremental(&path, self.args.iters, self.args.warmup);
        }
        // iter-lsp is the tight-loop profiler for the per-edit LSP query path
        // (BoneWorkspace on the bone backend); dispatched early like the other
        // iter-* profilers.
        if self.args.command == CliCommand::IterLsp {
            let path = self.get_path_or_current()?;
            return crate::iter_lsp::iter_lsp(&path, self.args.iters, self.args.warmup);
        }
        // iter-completion is the tight-loop profiler for the per-keystroke
        // completion path (call_through_param_completions); dispatched early.
        if self.args.command == CliCommand::IterCompletion {
            let path = self.get_path_or_current()?;
            return crate::iter_completion::iter_completion(&path, self.args.iters, self.args.warmup);
        }
        // lsp-survey is the stats-driven slow-edit hunter (BoneWorkspace);
        // dispatched early like the other iter-* / LSP profilers.
        if self.args.command == CliCommand::LspSurvey {
            let path = self.get_path_or_current()?;
            let json = self.args.perf_format == "json";
            return crate::lsp_survey::lsp_survey(&path, self.args.iters, self.args.warmup, json);
        }
        // iter-reinfer benches the Interface-tier re-inference path (bone
        // Session); dispatched early like the other iter-* profilers.
        if self.args.command == CliCommand::IterReinfer {
            let path = self.get_path_or_current()?;
            return crate::bone_backend::bone_iter_reinfer(&path, self.args.iters, self.args.warmup);
        }
        // iter-cascade benches the Interface-tier RE-LOWER cascade (a type edit
        // re-lowers its interface-dependents), the sibling of iter-reinfer.
        if self.args.command == CliCommand::IterCascade {
            let path = self.get_path_or_current()?;
            return crate::bone_backend::bone_iter_cascade(&path, self.args.iters, self.args.warmup);
        }
        // iter-cook loops the front-end cook (macro-expand + desugar), the work
        // bone's `expand_and_desugar` does per compile. skull-specific, early.
        if self.args.command == CliCommand::IterCook {
            let path = self.get_path_or_current()?;
            return crate::skull_backend::skull_iter_cook(&path, self.args.iters);
        }
        // Backend-agnostic kept commands: dispatched before the backend switch so
        // they never depend on a backend (the legacy rock execution arm is gone).
        if self.args.command == CliCommand::Fmt {
            let path = self.get_path_or_current()?;
            return match do_fmt(&path, !self.args.stdout, self.args.check) {
                Ok(_) => Ok(()),
                Err(e) => {
                    log::error!(target: "FMT", "{}", e);
                    std::process::exit(1);
                }
            };
        }
        if self.args.command == CliCommand::StoneTest {
            let path = self.get_path_or_current()?;
            return crate::stone_test::run_stone_test(std::path::Path::new(&path));
        }
        if self.args.command == CliCommand::Completions {
            // `rock completions <shell>` -- print a clap_complete script for the
            // requested shell. Shell name is the first positional after `completions`.
            let shell_name = self.args.files.first().map(|s| s.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "completions: missing shell name. Usage: rock completions <bash|zsh|fish|powershell|elvish>"
                )
            })?;
            let shell: clap_complete::Shell = shell_name.parse().map_err(|_| {
                anyhow::anyhow!(
                    "completions: unknown shell `{}`. Try one of: bash, zsh, fish, powershell, elvish",
                    shell_name
                )
            })?;
            let mut cmd = crate::Args::command();
            clap_complete::generate(shell, &mut cmd, "rock", &mut std::io::stdout());
            return Ok(());
        }

        // Every remaining command is served by a backend. The default backend is
        // skull; bone serves the type-check-capable subset. The capability gate
        // rejects an explicit backend/command mismatch.
        if self.args.backend == crate::Backend::Bone {
            self.require_backend_support()?;
            return self.run_command_bone();
        }
        self.require_backend_support()?;
        self.run_command_skull()
    }
}

