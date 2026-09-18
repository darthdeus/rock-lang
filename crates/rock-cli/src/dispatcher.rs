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
        if self.args.profile.is_some() && self.args.command != CliCommand::Shader {
            anyhow::bail!(
                "rock {}: --profile applies only to `rock shader` (offline GLSL emission). \
                 In-game shader source is emitted once and adapted to the live GL context by \
                 the engine, so no other command selects a profile. `rock shader-validate` \
                 gates both profiles and takes no flag.",
                self.args.command.cli_name(),
            );
        }
        // THE callee-binding hot-reload rule
        // (`skull_codegen_lua::force_global_callee_binding`). An
        // `incremental.lua` overlay redefines one global function; a caller that
        // captured it as an immutable upvalue holds the previous closure and
        // would never see the edited body. `upvalue` is the shipped default, so
        // the two commands that drive `emit_overlay` DIRECTLY pin the
        // global-lookup shape for the whole session, whatever
        // `ROCK_SKULL_CALLEE_BINDING` says: `stone-test` is the overlay path's
        // end-to-end harness, and `iter-incremental` is its profiler. Neither
        // has another way to reach that path once the default binds.
        //
        // `rock stone` is NOT here any more: it ships the bound shape and
        // publishes a full `main.lua` per edit instead of an overlay
        // (`SkullStone::route_and_emit`), so the dev loop emits what the game
        // ships. `rock watch` and `skull-incremental-profile` were never
        // affected -- both reassemble the whole chunk (`emit_incremental`), so
        // both emission passes rerun. `assemble_overlay` rejects a bound body
        // rather than trusting this list.
        if matches!(
            self.args.command,
            CliCommand::StoneTest | CliCommand::IterIncremental
        ) {
            skull::force_global_callee_binding();
        }
        let _renderer_override = rock_host::stick::install_renderer_override(self.args.renderer);
        self.run_command()
    }

    fn require_optional_features(&self) -> Result<()> {
        if self.args.command == CliCommand::Web && !cfg!(feature = "backend-js") {
            anyhow::bail!(
                "the `rock web` command was omitted from this build; enable the `backend-js` feature (for example, `just test-js` or `cargo run --features backend-js --bin rock web ...)"
            );
        }
        if self.args.command == CliCommand::MonoManifest && !cfg!(feature = "backend-js") {
            anyhow::bail!(
                "the `mono-manifest` command was omitted from this build; enable the `backend-js` feature (for example, `cargo run --features backend-js --bin rock mono-manifest ...)"
            );
        }
        if matches!(self.args.renderer, Some(RendererBackend::Wgpu))
            && !cfg!(feature = "renderer-wgpu")
        {
            anyhow::bail!(
                "the `wgpu` renderer was omitted from this build; enable the `renderer-wgpu` feature (for example, `just test-wgpu` or `cargo run --features renderer-wgpu --bin rock ... --renderer wgpu)"
            );
        }
        Ok(())
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

    pub fn get_game_file(&self) -> Result<String> {
        let Some(arg) = self.args.files.first() else {
            // No project arg: default to `game/`, with a directory
            // suggestion (then exit) if it's absent -- unchanged behavior.
            let path = "game".to_string();
            if !std::path::Path::new(&path).exists() {
                self.suggest_available_directories(&path);
            }
            return Ok(path);
        };
        // Explicit project arg: bare-name resolution, same as `rock sim`
        // (`match3` -> `games/match3`; a literal path wins; unknown rejects
        // listing the searched parents).
        self.resolve_project_arg(arg)
    }

    /// Route a project-directory argument through the shared bare-name
    /// resolver (`rock sim`'s `resolve_project_dir`): a literal path wins
    /// as-is, a bare name expands against `games/` (`match3` ->
    /// `games/match3`), and an unknown name rejects listing the searched
    /// parents. Runs `ensure_workspace_root` first so resolution is
    /// workspace-root-relative (idempotent -- the backend entry re-runs it).
    fn resolve_project_arg(&self, arg: &str) -> Result<String> {
        rock_host::project::ensure_workspace_root()?;
        Ok(rock_host::project::resolve_project_dir(arg)?
            .to_string_lossy()
            .into_owned())
    }

    /// `get_path_or_current` (default `.`, special device files rejected)
    /// followed by project-dir resolution via `resolve_project_arg`. For
    /// commands that take a PROJECT directory (`stone`, `watch`) so a bare
    /// `match3` resolves exactly like `rock sim`. The `.` default is a
    /// literal directory that always wins, so it stays "current directory"
    /// and is never bare-name-expanded.
    fn get_project_dir_or_current(&self) -> Result<String> {
        let arg = self.get_path_or_current()?;
        self.resolve_project_arg(&arg)
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
        crate::ablation_report::exit_with_census(1);
    }

    pub fn get_path_or_current(&self) -> Result<String> {
        let path = self.args
            .files
            .first()
            .cloned()
            .unwrap_or_else(|| ".".to_string());

        reject_device_path(&path)?;

        Ok(path)
    }

    /// Every path argument, for the commands that act on a list rather than on
    /// one entry point. `get_path_or_current` drops everything after the first.
    pub fn get_paths_or_current(&self) -> Result<Vec<String>> {
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
        let file = self.get_game_file()?;
        let output = self.get_output_or_default("tmp")?;
        if self.args.record.is_some() && self.args.replay.is_some() {
            anyhow::bail!("--record and --replay are mutually exclusive");
        }
        do_one_frame(
            self.args.backend,
            &file,
            output,
            self.args.frames,
            self.args.headless,
            self.args.dump_state,
            self.args.record.as_deref(),
            self.args.replay.as_deref(),
            self.args.checkpoint_interval,
            &self.args.app_args,
            self.args.renderer,
        )
    }

    /// `rock replay <game> <file>` — windowed playback (docs/replay-scrub-plan.md
    /// P3.5). Dedicated command, NOT `rock frame --replay` (batch headless): the
    /// window stays open and freezes at the last tick with the timeline bar live.
    fn handle_replay(&self) -> Result<()> {
        if self.args.files.len() < 2 {
            anyhow::bail!("usage: rock replay <game> <file.replay>");
        }
        let game = self.args.files[0].clone();
        let replay_file = self.args.files[1].clone();
        if !std::path::Path::new(&replay_file).exists() {
            anyhow::bail!("replay log not found: {replay_file}");
        }
        let output = self.get_output_or_default("tmp")?;
        do_replay_window(self.args.backend, &game, output, &replay_file, &self.args.app_args)
    }

    fn handle_replay_profile(&self) -> Result<()> {
        if self.args.files.len() != 2 {
            anyhow::bail!("usage: rock replay-profile <game> <file.replay> --range A-B [--runs N]");
        }
        let range = self.args.replay_range.as_deref().ok_or_else(|| {
            anyhow::anyhow!("rock replay-profile requires an inclusive --range A-B")
        })?;
        let game = self.resolve_project_arg(&self.args.files[0])?;
        let replay_file = std::path::Path::new(&self.args.files[1]);
        if !replay_file.exists() {
            anyhow::bail!("replay log not found: {}", replay_file.display());
        }
        let output = self.get_output_or_default("tmp")?;
        crate::replay_profile::run(
            self.args.backend,
            &game,
            output,
            replay_file,
            range,
            self.args.runs,
            &self.args.app_args,
        )
    }

    /// `rock perf` on the skull backend.
    fn handle_perf(&self) -> Result<()> {
        let file = self.get_game_file()?;
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
            self.args.gc_window_start,
            &self.args.gc_window_mode,
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
        if self.args.allocation_census.is_some() && self.args.allocation_sites.is_some() {
            anyhow::bail!("--allocation-census and --allocation-sites are separate diagnostic arms");
        }
        if let Some(path) = self.args.allocation_sites.as_deref() {
            if self.args.list {
                anyhow::bail!("--allocation-sites cannot be combined with sim --list");
            }
            if self.args.files.is_empty() {
                anyhow::bail!("allocation sites need a project: rock sim --allocation-sites DIR <project> [name]");
            }
            crate::run_sim::do_run_allocation_sites(
                backend,
                &self.args.files[0],
                &self.args.files[1..],
                path,
                self.args.jobs,
            )
        } else if let Some(path) = self.args.allocation_census.as_deref() {
            if self.args.list {
                anyhow::bail!("--allocation-census cannot be combined with sim --list");
            }
            if self.args.files.is_empty() {
                anyhow::bail!("allocation census needs a project: rock sim --allocation-census PATH <project> [name]");
            }
            crate::run_sim::do_run_allocation_census(
                backend,
                &self.args.files[0],
                &self.args.files[1..],
                path,
                self.args.jobs,
            )
        } else if self.args.list {
            if self.args.files.is_empty() {
                anyhow::bail!("sim --list needs a project: rock sim --list <project>");
            }
            let project = &self.args.files[0];
            crate::run_sim::do_list_sims(backend, project)
        } else if self.args.files.is_empty() {
            anyhow::bail!("sim needs a project: rock sim <project> [name]");
        } else if self.args.multi_order {
            // `files[1..]` narrows the CORPUS to those sims and changes nothing
            // else about the gate; empty means the whole registry.
            crate::run_sim::run_multi_order_gate(
                backend,
                &self.args.files[0],
                &self.args.files[1..],
            )
        } else if self.args.files.len() == 1 {
            // No name: run the whole suite.
            let project = &self.args.files[0];
            crate::run_sim::do_run_all_sims(
                backend,
                project,
                self.args.quiet,
                self.args.update_snapshots,
                self.args.jobs,
                self.args.full_asset_jobs,
                crate::run_sim::exec_mode(self.args.isolated, self.args.workers),
            )
        } else {
            // files[0] = project, files[1..] = one or more sim names. One name
            // streams full detail; several run concise (per-sim verdict +
            // final tally). Each name may itself be a `a,b` multi-start chain.
            let project = &self.args.files[0];
            let names = self.args.files[1..].to_vec();
            crate::run_sim::do_run_named_sims(
                backend,
                project,
                &names,
                self.args.quiet,
                self.args.update_snapshots,
                self.args.jobs,
                self.args.full_asset_jobs,
                crate::run_sim::exec_mode(self.args.isolated, self.args.workers),
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
                let path = self.get_project_dir_or_current()?;
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

    /// Dispatch through the `js` test executor (bone type-check + JS-subset
    /// per-test emission, run under node). Serves `test` only -- the `serves`
    /// table gates everything else before this switch.
    fn run_command_js(&self) -> Result<()> {
        #[cfg(not(feature = "backend-js"))]
        unreachable!("JS dispatch requires the `backend-js` feature");
        #[cfg(feature = "backend-js")]
        match self.args.command {
            CliCommand::Test => {
                let files = self.get_test_files()?;
                crate::js_test::js_test(&files)
            }
            CliCommand::Run => {
                let file = self.get_required_file("run")?;
                crate::js_test::js_run(file)
            }
            other => unreachable!(
                "js dispatch reached unsupported command {:?}; the capability \
                 gate should have rejected it — Backend::supported_commands and \
                 run_command_js have diverged",
                other
            ),
        }
    }

    /// Dispatch through the `glsl` value-test executor (bone type-check + GLSL
    /// per-value-test emission, run headlessly on the GPU under both profiles).
    /// Serves `test` only -- the `serves` table gates everything else.
    fn run_command_glsl(&self) -> Result<()> {
        match self.args.command {
            CliCommand::Test => {
                let files = self.get_test_files()?;
                crate::glsl_test::glsl_test(&files)
            }
            other => unreachable!(
                "glsl dispatch reached unsupported command {:?}; the capability \
                 gate should have rejected it — Backend::supported_commands and \
                 run_command_glsl have diverged",
                other
            ),
        }
    }

    fn run_command_c(&self) -> Result<()> {
        match self.args.command {
            CliCommand::Compile => {
                let file = self.get_required_file("compile")?;
                crate::c_test::c_compile(file, self.get_compile_output()?)
            }
            CliCommand::Test => {
                let files = self.get_test_files()?;
                crate::c_test::c_test(&files)
            }
            CliCommand::Run => {
                let file = self.get_required_file("run")?;
                crate::c_test::c_run(file)
            }
            other => unreachable!(
                "c dispatch reached unsupported command {:?}; the capability \
                 gate should have rejected it — Backend::supported_commands and \
                 run_command_c have diverged",
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
                let path = self.get_project_dir_or_current()?;
                skull_backend::skull_watch(&path, self.args.output.clone())
            }
            CliCommand::Stone => {
                // Hot-reload game dev loop on skull: the watch/libstone/game-process
                // machinery is shared; only the compile step re-emits via skull.
                let path = self.get_project_dir_or_current()?;
                crate::stone::do_stone_watch(
                    &path,
                    self.args.quiet,
                    &self.args.app_args,
                    crate::Backend::Skull,
                    self.args.no_record,
                )
            }
            // frame/perf/sim are backend-parameterized: the compile + @sim
            // collection routes through skull, the spawn/profile/run half is shared.
            CliCommand::Frame => self.handle_frame(),
            CliCommand::Replay => self.handle_replay(),
            CliCommand::ReplayProfile => self.handle_replay_profile(),
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
        if self.args.command == CliCommand::SdkCheck {
            if self.args.files.len() != 1 {
                anyhow::bail!("usage: rock sdk-check <project>");
            }
            crate::sdk_check::check(&self.args.files[0], std::path::Path::new(&rock_host::rock_dir()))?;
            println!("SDK manifest compatibility: OK");
            return Ok(());
        }
        if self.args.command == CliCommand::Modules {
            let usage = "usage: rock modules audit <project> [--closed] [--format pretty|json]";
            return match self.args.files.as_slice() {
                [sub, project] if sub == "audit" => {
                    let format = match self.args.perf_format.as_str() {
                        "pretty" => crate::modules_audit::Format::Pretty,
                        "json" => crate::modules_audit::Format::Json,
                        other => anyhow::bail!("rock modules audit: unknown --format `{other}` (pretty|json)\n{usage}"),
                    };
                    let code = crate::modules_audit::run(std::path::Path::new(project), self.args.closed, format)?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                [sub, ..] if sub != "audit" => {
                    anyhow::bail!("rock modules: unknown subcommand `{sub}`; only `audit` exists\n{usage}")
                }
                _ => anyhow::bail!("{usage}"),
            };
        }
        if self.args.command == CliCommand::DocsIndex {
            return crate::docs_index::run(&self.args.files, self.args.output.as_deref());
        }
        if self.args.command == CliCommand::ReplayReport {
            if self.args.files.len() != 1 {
                anyhow::bail!("usage: rock replay-report <file.replay> [--top N] [--context N] [--format pretty|json]");
            }
            return crate::replay_report::run(
                std::path::Path::new(&self.args.files[0]),
                self.args.top,
                self.args.context,
                &self.args.perf_format,
            );
        }
        if self.args.command == CliCommand::SimReport {
            if self.args.files.len() > 1 {
                anyhow::bail!(
                    "usage: rock sim-report [<run-id>] [--sim-log-dir <root>] \
                     [--sim-entry <name>] [--top N] [--format pretty|json]"
                );
            }
            return crate::sim_report::run(
                self.args.sim_log_dir.as_deref(),
                self.args.files.first().map(String::as_str),
                self.args.sim_entry.as_deref(),
                self.args.top,
                &self.args.perf_format,
            );
        }
        if self.args.command == CliCommand::ReplayProfileReport {
            if self.args.files.len() != 1 {
                anyhow::bail!(
                    "usage: rock replay-profile-report <file.jsonl> [--top N] [--format pretty|json]"
                );
            }
            return crate::replay_profile_report::run(
                std::path::Path::new(&self.args.files[0]),
                self.args.top,
                &self.args.perf_format,
            );
        }
        // iter-compile is a bone-inference profiling loop; it ignores --backend
        // and runs its own pipeline, so dispatch it before the backend switch.
        if self.args.command == CliCommand::IterCompile {
            let path = self.get_path_or_current()?;
            return crate::bone_backend::bone_iter_inference(&path, self.args.iters, self.args.warmup);
        }
        // web emits a static browser build (index.html + game.js) via the JS
        // backend; it runs project-mode JS emission itself and ignores
        // --backend, so it dispatches before the backend switch like shader/fmt.
        #[cfg(feature = "backend-js")]
        if self.args.command == CliCommand::Web {
            let project = self.get_project_dir_or_current()?;
            return crate::web::run_web(
                &project,
                self.args.output.as_deref(),
                self.args.watch,
                self.args.port,
                if self.args.single_file {
                    crate::web::WebPackage::SingleFile
                } else {
                    crate::web::WebPackage::Directory
                },
                self.args.itch,
            );
        }
        if self.args.command == CliCommand::Package {
            let project = self.get_project_dir_or_current()?;
            return crate::package::run_package(&project, self.args.output.as_deref());
        }
        if self.args.command == CliCommand::SoundInspect {
            let project = self.get_project_dir_or_current()?;
            return crate::sound_inspect::run(&project);
        }
        // shader is GLSL fn-library emission (route B); it type-checks a single
        // file against the stdlib and emits GLSL, so it's backend-agnostic and
        // dispatches before the backend switch like fmt/completions.
        if self.args.command == CliCommand::Shader {
            return crate::skull_backend::skull_shader(
                &self.args.files,
                self.args.output.clone(),
                self.args.profile.clone(),
            );
        }
        // shader-validate compiles+links registered shader pairs on real GL
        // contexts (both profiles); backend-agnostic like shader.
        if self.args.command == CliCommand::ShaderValidate {
            return crate::glsl_test::shader_validate(&self.args.files);
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
        if self.args.command == CliCommand::SkullMem2RegStats {
            let path = self.get_path_or_current()?;
            return crate::skull_backend::skull_mem2reg_stats(&path);
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
        // mono-ty-census converts the closed world's inferred types to MonoTy and
        // reports; a Stage-0 diagnostic that touches no emission, dispatched early
        // like the other skull diagnostics.
        if self.args.command == CliCommand::MonoTyCensus {
            let root = self.get_path_or_current()?;
            return crate::monoty_census::do_monoty_census(Some(&root));
        }
        // mono-manifest builds the Stage-0d shadow instance graph; a diagnostic
        // that touches no emission, dispatched early like its 0c sibling.
        #[cfg(feature = "backend-js")]
        if self.args.command == CliCommand::MonoManifest {
            let root = self.get_path_or_current()?;
            // `ROCK_MONO_DUMP=<dir>` writes the per-project Lua and JS manifests
            // there. A target disagreement is only diagnosable by diffing them.
            let dump = std::env::var("ROCK_MONO_DUMP").ok();
            return crate::mono_manifest::do_mono_manifest(Some(&root), dump.as_deref());
        }
        if self.args.command == CliCommand::AllocationJoin {
            return crate::allocation_join::run(&self.args.files);
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
            return crate::iter_completion::iter_completion(
                &path,
                self.args.iters,
                self.args.warmup,
                self.args.budget_ms,
            );
        }
        // completion-sweep is the byte-identity output dump for completion
        // refactors (docs/completion-v2-design.md M0); dispatched early like
        // the other completion harnesses.
        if self.args.command == CliCommand::CompletionSweep {
            let path = self.get_path_or_current()?;
            return crate::completion_sweep::completion_sweep(
                &path,
                self.args.stride,
                self.args.check,
            );
        }
        // completion-replay re-ranks recorded accepts against the current
        // ranker and reports MRR (docs/completion-v2-design.md M6);
        // dispatched early like the other completion harnesses.
        if self.args.command == CliCommand::CompletionReplay {
            return crate::completion_replay::completion_replay(&self.args.files);
        }
        // completion-episodes stitches log rows into episodes of user
        // struggle and prints the tuning metrics (docs/completion-episodes.md).
        if self.args.command == CliCommand::CompletionEpisodes {
            return crate::completion_episodes::completion_episodes(&self.args.files);
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
            let paths = self.get_paths_or_current()?;
            return match do_fmt(&paths, !self.args.stdout, self.args.check) {
                Ok(_) => Ok(()),
                Err(e) => {
                    log::error!(target: "FMT", "{}", e);
                    crate::ablation_report::exit_with_census(1);
                }
            };
        }
        // `rock lint` is a pure consumer of bone's typechecked output -- no
        // codegen, so it dispatches before the backend switch like `fmt`.
        if self.args.command == CliCommand::Lint {
            let path = self.get_path_or_current()?;
            if self.args.union {
                return crate::lint_union::do_lint_union(
                    &path,
                    self.args.consumers.as_deref(),
                );
            }
            return crate::lint_cmd::do_lint(
                &path,
                self.args.lints.as_deref(),
                self.args.only_types.as_deref(),
                self.args.ignore_types.as_deref(),
            );
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
        if self.args.backend == crate::Backend::Js {
            self.require_backend_support()?;
            return self.run_command_js();
        }
        if self.args.backend == crate::Backend::Glsl {
            self.require_backend_support()?;
            return self.run_command_glsl();
        }
        if self.args.backend == crate::Backend::C {
            self.require_backend_support()?;
            return self.run_command_c();
        }
        self.require_backend_support()?;
        self.run_command_skull()
    }
}
