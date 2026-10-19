//! Public compiler registrations. Private hosts extend these static tables.
use crate as rock_cli;
use crate::*;
use crate::registry::{self, OmittedBackend, OptionRule, Route};

pub static COMMANDS: &[&CliCommand] = &[
    &CliCommand { name: "check", route: Route::Direct(bone_compile) },
    &CliCommand { name: "compile", route: Route::Backend(Operation::Compile) },
    &CliCommand { name: "compile-single", route: Route::Backend(Operation::CompileSingle) },
    &CliCommand { name: "test", route: Route::Backend(Operation::Test) },
    &CliCommand { name: "test-compile", route: Route::Backend(Operation::TestCompile) },
    &CliCommand { name: "run", route: Route::Backend(Operation::Run) },
    &CliCommand { name: "watch", route: Route::Backend(Operation::Watch) },
    &CliCommand { name: "fmt", route: Route::Direct(|cx| {
        let paths = cx.get_paths_or_current()?;
        match do_fmt(&paths, !cx.args.stdout, cx.args.check) {
            Ok(_) => Ok(()),
            Err(e) => {
                log::error!(target: "FMT", "{}", e);
                rock_cli::ablation_report::exit_with_census(1);
            }
        }
    }) },
    &CliCommand { name: "completions", route: Route::Direct(|cx| {
        let shell_name = cx.args.files.first().map(|s| s.as_str()).ok_or_else(|| {
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
        let mut cmd = rock_cli::Args::command();
        clap_complete::generate(shell, &mut cmd, "rock", &mut std::io::stdout());
        Ok(())
    }) },
    &CliCommand { name: "lint", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        if cx.args.union {
            return rock_cli::lint_union::do_lint_union(&path, cx.args.consumers.as_deref());
        }
        rock_cli::do_lint(
            &path,
            cx.args.lints.as_deref(),
            cx.args.only_types.as_deref(),
            cx.args.ignore_types.as_deref(),
        )
    }) },
    &CliCommand { name: "iter-compile", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::bone_backend::bone_iter_inference(&path, cx.args.iters, cx.args.warmup)
    }) },
    &CliCommand { name: "iter-parse", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::iter_parse_skull(&path, cx.args.iters)
    }) },
    &CliCommand { name: "iter-tokenize", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::iter_tokenize(&path, cx.args.iters)
    }) },
    &CliCommand { name: "iter-codegen", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::skull_backend::skull_iter_codegen(&path, cx.args.iters)
    }) },
    &CliCommand { name: "iter-cook", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::skull_backend::skull_iter_cook(&path, cx.args.iters)
    }) },
    &CliCommand { name: "skull-inline-stats", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::skull_backend::skull_inline_stats(&path)
    }) },
    &CliCommand { name: "skull-mem2reg-stats", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::skull_backend::skull_mem2reg_stats(&path)
    }) },
    &CliCommand { name: "skull-incremental-profile", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::skull_backend::skull_incremental_profile(&path)
    }) },
    &CliCommand { name: "iter-incremental", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::skull_backend::skull_iter_incremental(&path, cx.args.iters, cx.args.warmup)
    }) },
    &CliCommand { name: "mono-ty-census", route: Route::Direct(|cx| {
        let root = cx.get_path_or_current()?;
        rock_cli::do_monoty_census(Some(&root))
    }) },
    &CliCommand { name: "identity-storage-census", route: Route::Direct(|cx| {
        let root = cx.get_path_or_current()?;
        rock_cli::do_identity_storage_census(&root)
    }) },
    &CliCommand { name: "allocation-join", route: Route::Direct(|cx| {
        rock_cli::allocation_join_run(&cx.args.files)
    }) },
    &CliCommand { name: "iter-lsp", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::iter_lsp(&path, cx.args.iters, cx.args.warmup)
    }) },
    &CliCommand { name: "lsp-survey", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        let json = cx.args.perf_format == "json";
        rock_cli::lsp_survey(&path, cx.args.iters, cx.args.warmup, json)
    }) },
    &CliCommand { name: "iter-reinfer", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::bone_backend::bone_iter_reinfer(&path, cx.args.iters, cx.args.warmup)
    }) },
    &CliCommand { name: "iter-completion", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::iter_completion(&path, cx.args.iters, cx.args.warmup, cx.args.budget_ms)
    }) },
    &CliCommand { name: "completion-sweep", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::completion_sweep(&path, cx.args.stride, cx.args.check)
    }) },
    &CliCommand { name: "completion-episodes", route: Route::Direct(|cx| {
        rock_cli::completion_episodes(&cx.args.files)
    }) },
    &CliCommand { name: "iter-cascade", route: Route::Direct(|cx| {
        let path = cx.get_path_or_current()?;
        rock_cli::bone_backend::bone_iter_cascade(&path, cx.args.iters, cx.args.warmup)
    }) },
    &CliCommand { name: "backend-stats", route: Route::Direct(|cx| {
        let input = cx.get_path_or_current()?;
        rock_cli::backend_stats::do_backend_stats(&input, cx.args.iters, cx.args.warmup)
    }) },
    &SHADER,
    &CliCommand { name: "shader-validate", route: Route::Direct(|cx| {
        rock_cli::glsl_test::shader_validate(&cx.args.files)
    }) },
    &CliCommand { name: "docs-index", route: Route::Direct(|cx| {
        rock_cli::docs_index::run(&cx.args.files, cx.args.output.as_deref())
    }) },
    &CliCommand { name: "modules", route: Route::Direct(|cx| {
        let usage = "usage: rock modules audit <project> [--closed] [--format pretty|json]\n       rock modules migrate <package> --check|--apply [--consumer <dir>]... [--format pretty|json]";
        let format = match cx.args.perf_format.as_str() {
            "pretty" => rock_cli::ModulesFormat::Pretty,
            "json" => rock_cli::ModulesFormat::Json,
            other => anyhow::bail!("rock modules: unknown --format `{other}` (pretty|json)\n{usage}"),
        };
        let code = match cx.args.files.as_slice() {
            [sub, project] if sub == "audit" => {
                rock_cli::modules_audit_run(std::path::Path::new(project), cx.args.closed, format)?
            }
            [sub, package] if sub == "migrate" => {
                let mode = if cx.args.apply {
                    rock_cli::ModulesMode::Apply
                } else {
                    rock_cli::ModulesMode::Check
                };
                let consumers: Vec<std::path::PathBuf> =
                    cx.args.consumer.iter().map(std::path::PathBuf::from).collect();
                rock_cli::modules_migrate_run(std::path::Path::new(package), &consumers, mode, format)?
            }
            [sub, ..] if sub != "audit" && sub != "migrate" => {
                anyhow::bail!("rock modules: unknown subcommand `{sub}`; `audit` and `migrate` exist\n{usage}")
            }
            _ => anyhow::bail!("{usage}"),
        };
        if code != 0 {
            rock_cli::invocation_log::exit(code);
        }
        Ok(())
    }) },
    &CliCommand { name: "fix", route: Route::Direct(|cx| {
        let mut usage = "usage: rock fix --imports <package|file> [--check|--apply] [--format pretty|json]".to_string();
        let mut choices = vec!["`--imports`".to_string()];
        for fix in registry::registered().fixes {
            usage.push_str("\n       ");
            usage.push_str(fix.usage);
            choices.push(format!("`--{}`", fix.flag));
        }
        if let Some(fix) = cx.args.fix {
            return (fix.run)(cx);
        }
        if !cx.args.imports {
            anyhow::bail!("rock fix: name the fix to run ({})\n{usage}", choices.join(" or "));
        }
        let format = match cx.args.perf_format.as_str() {
            "pretty" => rock_cli::ModulesFormat::Pretty,
            "json" => rock_cli::ModulesFormat::Json,
            other => anyhow::bail!("rock fix: unknown --format `{other}` (pretty|json)\n{usage}"),
        };
        let [target] = cx.args.files.as_slice() else { anyhow::bail!("{usage}") };
        let mode = if cx.args.apply { rock_cli::ModulesMode::Apply } else { rock_cli::ModulesMode::Check };
        let code = rock_cli::modules_fix_run(std::path::Path::new(target), mode, format)?;
        if code != 0 {
            rock_cli::invocation_log::exit(code);
        }
        Ok(())
    }) }
];

pub static SHADER: CliCommand = CliCommand {
    name: "shader",
    route: Route::Direct(|cx| {
        rock_cli::skull_backend::skull_shader(&cx.args.files, cx.args.output.clone(), cx.args.profile.clone())
    }),
};

pub static BONE: BackendSpec = BackendSpec {
        backend: Backend::Bone,
        name: "bone",
        tagline: "bone is a typechecker with no codegen/runtime",
        omitted: None,
        operations: &[
            // skull subsumes bone for tests (it type-checks through the same
            // front end, then runs), so both backends share skull_test.
            (Operation::Test, |cx| {
                let files = cx.get_test_files()?;
                let filter = cx.test_filter()?;
                rock_cli::skull_backend::skull_test(&files, filter.as_ref())
            }),
            (Operation::Compile, bone_compile),
            (Operation::CompileSingle, bone_compile),
            (Operation::TestCompile, bone_compile),
            (Operation::Run, |cx| {
                let file = cx.get_required_file("run")?;
                rock_cli::bone_backend::bone_check_single(file, "run", cx.args.iters)
            }),
            (Operation::Watch, |cx| {
                let path = cx.get_project_dir_or_current()?;
                rock_cli::bone_backend::bone_watch(&path)
            }),
        ],
        filter_rejection: None,
    };

pub static SKULL: BackendSpec = BackendSpec {
        backend: Backend::Skull,
        name: "skull",
        tagline: "skull is a bone-HIR codegen",
        omitted: None,
        operations: &[
            (Operation::Test, |cx| {
                let files = cx.get_test_files()?;
                let filter = cx.test_filter()?;
                rock_cli::skull_backend::skull_test(&files, filter.as_ref())
            }),
            (Operation::Compile, skull_compile),
            (Operation::CompileSingle, skull_compile),
            (Operation::TestCompile, skull_compile),
            (Operation::Run, |cx| {
                let file = cx.get_required_file("run")?;
                rock_cli::skull_backend::skull_run(file)
            }),
            (Operation::Watch, |cx| {
                let path = cx.get_project_dir_or_current()?;
                rock_cli::skull_backend::skull_watch(&path, cx.args.output.clone())
            }),

            // frame/perf/sim are backend-parameterized: the compile + @sim
            // collection routes through skull, the spawn/profile/run half is shared.



        ],
        filter_rejection: None,
    };

pub static JS: BackendSpec = BackendSpec {
    backend: Backend::Js,
    name: "js",
    tagline: "js is a JS-subset executor (node)",
    omitted: Some(OmittedBackend::Build),
    operations: &[],
    filter_rejection: None,
};

pub static GLSL: BackendSpec = BackendSpec {
        backend: Backend::Glsl,
        name: "glsl",
        tagline: "glsl is a GLSL value-test executor (GPU), not a program codegen",
        omitted: None,
        operations: &[(Operation::Test, |cx| {
            let files = cx.get_test_files()?;
            rock_cli::glsl_test::glsl_test(&files)
        })],
        filter_rejection: Some("`rock test` filtering is supported only by the skull/bone test runner"),
    };

pub static C: BackendSpec = BackendSpec {
    backend: Backend::C,
    name: "c",
    tagline: "c is a native C executor (Clang)",
    omitted: Some(OmittedBackend::Build),
    operations: &[],
    filter_rejection: None,
};

pub static BACKENDS: &[BackendSpec] = &[BONE, SKULL, JS, GLSL, C];

fn bone_compile(cx: &CommandContext) -> Result<()> {
    let file = cx.get_required_file("compile")?;
    rock_cli::bone_backend::bone_check_single(file, "compile", cx.args.iters)
}

fn skull_compile(cx: &CommandContext) -> Result<()> {
    let file = cx.get_required_file("compile")?;
    rock_cli::skull_backend::skull_compile_single(file, cx.get_compile_output()?)
}

pub static OPTION_RULES: &[OptionRule] = &[OptionRule::TestFilter];

pub const RUNTIME_SERVICES: registry::RuntimeServices = registry::RuntimeServices {
    resolve_project_dir: |path| Ok(std::path::PathBuf::from(path)),
    is_game_project: |_| Ok(false),
    configured_runtime_dir: |_| Ok(None),
    build_engine_libs: |_| unavailable("engine build"),
    runtime_command: |command, _| command,
    prepare_test,
    prepare_unit: |_| Ok(()),
    prepare_sound_bank: |_| Ok(None),
    sound_project_path: |_| Ok(None),
    prepare_run: |_| Ok(()),
    launch: |_, program, emits_main| crate::compile_cmd::run_lua_program(program, emits_main),
    // Compiler-only artifacts have no host globals, including incremental output.
    program_preamble: |_, _| Ok(String::new()),
    program_host: |_| Ok(registry::ProgramHost { preamble: String::new(), setup: "" }),
    // An unfiltered compiler build needs no private selection policy.
    resolve_sim_specs: |names, selection| {
        if selection.is_empty() { Ok(names.to_vec()) } else { unavailable("simulation selection") }
    },
};

/// Portable scratch belongs to the OS/user-selected temporary directory,
/// independently of project and installation roots.
fn prepare_test(command: &mut std::process::Command) -> Result<tempfile::TempDir> {
    let scratch = tempfile::Builder::new().prefix("rock-test-").tempdir()?;
    command.env("ROCK_TEST_TMP", scratch.path());
    Ok(scratch)
}

fn unavailable<T>(operation: &str) -> Result<T> {
    anyhow::bail!("{operation} is unavailable in the plain compiler build")
}

pub fn command_metadata(command: &CliCommand) -> registry::CommandMetadata {
    registry::CommandMetadata {
        log_invocation: matches!(command.cli_name(),
            "compile" | "compile-single" | "test" | "test-compile" | "run" | "watch" | "lint" | "fmt"),
        compiler_process: if command.cli_name() == "check" || matches!(command.operation(),
            Some(Operation::Compile | Operation::CompileSingle | Operation::Test | Operation::TestCompile)) {
            registry::CompilerProcess::Always
        } else {
            registry::CompilerProcess::Never
        },
        build_command: Some(command.cli_name()),
        implied_channel: None,
    }
}

pub static REGISTRATIONS: registry::Registrations = registry::Registrations {
    identity_storage_exemptions: bone_query::IdentityStorageExemptions {
        packages: &[bone_query::STDLIB_PACKAGE, bone_query::UNPACKAGED_PROGRAMS],
        modules: &[],
    },
    action_layer_policy: None,
    commands: COMMANDS,
    backends: BACKENDS,
    option_rules: OPTION_RULES,
    arguments: |command| command,
    decode_arguments: |_, _| {},
    command_metadata,
    invocation_context_keys: &[],
    compile_slot_policy: || Ok(None),
    scan_excluded: |_| false,
    identity_census_population: |root| crate::identity_storage_census::CensusPopulation {
        packages: crate::identity_storage_census::packages_under(root, |_| false),
        programs: Vec::new(),
    },
    early_entries: &[],
    fixes: &[],
    sim_environment: None,
    runtime_services: &RUNTIME_SERVICES,
    // A public tool does not record working directories and branches unless asked.
    initialize_process: |_| crate::invocation_log::require_explicit_log_path(),
};
