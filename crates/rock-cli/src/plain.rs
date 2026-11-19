//! Public compiler registrations. Private hosts extend these static tables.
use crate as rock_cli;
use crate::registry::{self, OmittedBackend, OptionRule, Route};
use crate::*;

pub static COMMANDS: &[&CliCommand] = &[
    &CliCommand {
        name: "native-build",
        route: Route::Direct(|cx| {
            anyhow::ensure!(
                !cx.args.files.is_empty(),
                "usage: rock native-build <project>"
            );
            for project in &cx.args.files {
                for lib in rock_engine_host::engine_build::game_engine_libs(project)? {
                    rock_engine_host::engine_build::build_optional_engine_lib(&lib)?;
                }
            }
            Ok(())
        }),
    },
    &CliCommand {
        name: "engine-libs",
        route: Route::Direct(|cx| {
            let mut libs = Vec::new();
            for project in &cx.args.files {
                libs.extend(rock_engine_host::engine_build::game_engine_libs(project)?);
            }
            println!(
                "{}",
                rock_engine_host::native_libraries::registry_json(&libs)?
            );
            Ok(())
        }),
    },
    &CliCommand {
        name: "engine-build-receipt",
        route: Route::Direct(|cx| {
            let output = cx.args.output.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: rock engine-build-receipt [<game>...] --output <receipt.json>"
                )
            })?;
            rock_engine_host::engine_build::prepare_engine_build_receipt(
                &cx.args.files,
                std::path::Path::new(output),
            )
        }),
    },
    &CliCommand {
        name: "sdk-check",
        route: Route::Direct(|cx| {
            if cx.args.files.len() != 1 {
                anyhow::bail!("usage: rock sdk-check <project>");
            }
            crate::sdk_check::check(
                &cx.args.files[0],
                std::path::Path::new(&rock_host::rock_dir()),
            )?;
            println!("SDK manifest compatibility: OK");
            Ok(())
        }),
    },
    &CliCommand {
        name: "sim",
        route: Route::Backend(Operation::Sim),
    },
    &CliCommand {
        name: "frame",
        route: Route::Backend(Operation::Frame),
    },
    &CliCommand {
        name: "perf",
        route: Route::Backend(Operation::Perf),
    },
    &CliCommand {
        name: "stone",
        route: Route::Backend(Operation::Stone),
    },
    &CliCommand {
        name: "stone-test",
        route: Route::Direct(|cx| {
            let path = cx.get_path_or_current()?;
            crate::stone_test::run_stone_test(std::path::Path::new(&path))
        }),
    },
    &CliCommand {
        name: "package",
        route: Route::Direct(|cx| {
            let output = cx
                .args
                .output
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("package requires --output <directory>"))?;
            let project = cx.get_project_dir_or_current()?;
            crate::package::run_package(&project, std::path::Path::new(output))
        }),
    },
    &CliCommand {
        name: "asset-check",
        route: Route::Direct(|cx| {
            let project = cx.get_project_dir_or_current()?;
            crate::package::check_project_assets(std::path::Path::new(&project))
        }),
    },
    &CliCommand {
        name: "check",
        route: Route::Direct(bone_compile),
    },
    &CliCommand {
        name: "compile",
        route: Route::Backend(Operation::Compile),
    },
    &CliCommand {
        name: "compile-single",
        route: Route::Backend(Operation::CompileSingle),
    },
    &CliCommand {
        name: "test",
        route: Route::Backend(Operation::Test),
    },
    &CliCommand {
        name: "test-compile",
        route: Route::Backend(Operation::TestCompile),
    },
    &CliCommand {
        name: "run",
        route: Route::Backend(Operation::Run),
    },
    &CliCommand {
        name: "watch",
        route: Route::Backend(Operation::Watch),
    },
    &CliCommand {
        name: "fmt",
        route: Route::Direct(|cx| {
            let paths = cx.get_paths_or_current()?;
            match do_fmt(&paths, cx.args.check) {
                Ok(_) => Ok(()),
                Err(e) => {
                    log::error!(target: "FMT", "{}", e);
                    rock_cli::invocation_log::exit(1);
                }
            }
        }),
    },
    &CliCommand {
        name: "completions",
        route: Route::Direct(|cx| {
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
        }),
    },
    &CliCommand {
        name: "lint",
        route: Route::Direct(|cx| {
            let path = cx.get_path_or_current()?;
            if cx.args.union {
                return rock_cli::lint_union::do_lint_union(&path, cx.args.consumers.as_deref());
            }
            rock_cli::do_lint(&path, cx.args.lints.as_deref())
        }),
    },
    &CliCommand {
        name: "allocation-join",
        route: Route::Direct(|cx| crate::allocation_join::run(&cx.args.files)),
    },
    &CliCommand {
        name: "lsp-survey",
        route: Route::Direct(|cx| {
            let path = cx.get_path_or_current()?;
            let json = cx.args.perf_format == "json";
            rock_cli::lsp_survey(&path, cx.args.iters, cx.args.warmup, json)
        }),
    },
    &CliCommand {
        name: "backend-stats",
        route: Route::Direct(|cx| {
            let input = cx.get_path_or_current()?;
            let json = cx.args.perf_format == "json";
            rock_cli::backend_stats::do_backend_stats(&input, cx.args.iters, cx.args.warmup, json)
        }),
    },
    &CliCommand {
        name: "incremental-stats",
        route: Route::Direct(|cx| {
            let path = cx.get_path_or_current()?;
            let json = cx.args.perf_format == "json";
            rock_cli::incremental_stats::incremental_stats(
                &path,
                cx.args.iters,
                cx.args.warmup,
                json,
            )
        }),
    },
    &SHADER,
    &CliCommand {
        name: "shader-validate",
        route: Route::Direct(|cx| {
            rock_cli::glsl_test::shader_validate(&cx.args.files, cx.args.skip_gpu)
        }),
    },
    &CliCommand {
        name: "docs-index",
        route: Route::Direct(|cx| {
            rock_cli::docs_index::run(&cx.args.files, cx.args.output.as_deref())
        }),
    },
];

pub static SHADER: CliCommand = CliCommand {
    name: "shader",
    route: Route::Direct(|cx| {
        rock_cli::skull_backend::skull_shader(
            &cx.args.files,
            cx.args.output.clone(),
            cx.args.profile.clone(),
        )
    }),
};

pub(crate) static SKULL: BackendSpec = BackendSpec {
    backend: Backend::Skull,
    name: "skull",
    tagline: "skull is a bone-HIR codegen",
    omitted: None,
    operations: &[
        (Operation::Frame, crate::frame_commands::handle_frame),
        (Operation::Perf, crate::frame_commands::handle_perf),
        (Operation::Sim, crate::sim_command::handle_sim),
        (Operation::Stone, |cx| {
            let path = cx.get_project_dir_or_current()?;
            crate::stone_watch::do_stone_watch(&path, cx.args.quiet, &cx.args.app_args)
        }),
        (Operation::Test, |cx| {
            let files = cx.get_test_files()?;
            if files.iter().any(|path| path.ends_with(".rock.lsp")) {
                anyhow::bail!(
                    ".rock.lsp fixtures have been retired. Run the editor tests with `cargo test -p rock-cli editor::`."
                );
            }
            let filter = cx.test_filter()?;
            rock_cli::skull_backend::skull_test(&files, filter.as_ref(), cx.args.test_threads)
        }),
        (Operation::Compile, skull_compile),
        (Operation::CompileSingle, skull_compile),
        (Operation::TestCompile, skull_compile),
        (Operation::Run, |cx| {
            let file = cx.get_required_file("run")?;
            rock_cli::skull_backend::skull_run(file, &cx.args.app_args)
        }),
        (Operation::Watch, |cx| {
            let path = cx.get_project_dir_or_current()?;
            rock_cli::skull_backend::skull_watch(&path, cx.args.output.clone())
        }),
    ],
    filter_rejection: None,
};

pub static JS: BackendSpec = BackendSpec {
    backend: Backend::Js,
    name: "js",
    tagline: "js is a JS-subset executor (node)",
    omitted: if cfg!(feature = "backend-js") {
        None
    } else {
        Some(OmittedBackend::Feature {
            feature: "backend-js",
            hint: "rebuild with default features or `--features backend-js`",
        })
    },
    operations: &[(Operation::Test, js_test), (Operation::Run, js_run)],
    filter_rejection: Some("`rock test` filtering is supported only by the skull test runner"),
};

pub static GLSL: BackendSpec = BackendSpec {
    backend: Backend::Glsl,
    name: "glsl",
    tagline: "glsl is a GLSL value-test executor (GPU), not a program codegen",
    omitted: None,
    operations: &[(Operation::Test, |cx| {
        let files = cx.get_test_files()?;
        rock_cli::glsl_test::glsl_test(&files, cx.args.skip_gpu)
    })],
    filter_rejection: Some("`rock test` filtering is supported only by the skull test runner"),
};

pub static BACKENDS: &[BackendSpec] = &[SKULL, JS, GLSL];

fn bone_compile(cx: &CommandContext) -> Result<()> {
    let file = cx.get_required_file("compile")?;
    rock_cli::bone_backend::bone_check_single(file, cx.args.iters)
}

fn skull_compile(cx: &CommandContext) -> Result<()> {
    let file = cx.get_required_file("compile")?;
    rock_cli::skull_backend::skull_compile_single(file, cx.get_compile_output()?)
}

#[cfg(feature = "backend-js")]
fn js_test(cx: &CommandContext) -> Result<()> {
    let files = cx.get_test_files()?;
    rock_cli::js_test::js_test(&files)
}

#[cfg(feature = "backend-js")]
fn js_run(cx: &CommandContext) -> Result<()> {
    let file = cx.get_required_file("run")?;
    rock_cli::js_test::js_run(file)
}

// Without the feature the capability gate rejects JS with the omitted-feature
// message before dispatch; the operations stay registered so the gate and the
// reject message still know what JS serves.
#[cfg(not(feature = "backend-js"))]
fn js_test(_: &CommandContext) -> Result<()> {
    unreachable!("JS dispatch requires the `backend-js` feature")
}

#[cfg(not(feature = "backend-js"))]
fn js_run(_: &CommandContext) -> Result<()> {
    unreachable!("JS dispatch requires the `backend-js` feature")
}

pub static OPTION_RULES: &[OptionRule] = &[OptionRule::TestFilter, OptionRule::SkipGpu];

pub const RUNTIME_SERVICES: registry::RuntimeServices = registry::RuntimeServices {
    resolve_project_dir: rock_engine_host::project::resolve_project_dir,
    is_game_project: rock_engine_host::project::is_game_project,
    configured_runtime_dir: rock_engine_host::project::configured_stone_dir,
    build_engine_libs: rock_engine_host::engine_build::build_engine_libs,
    prepare_unit: crate::stone_runtime::prepare_unit,
    prepare_sound_bank: crate::package::prepare_sound_bank,
    sound_project_path: crate::package::sound_project_path_for,
    prepare_run: crate::stone_runtime::prepare_run,
    launch: crate::stone_runtime::launch,
    program_preamble: crate::stone_runtime::program_preamble,
    program_host: crate::stone_runtime::program_host,
    resolve_sim_specs: crate::run_sim::resolve_sim_specs,
};

pub fn command_metadata(command: &CliCommand) -> registry::CommandMetadata {
    registry::CommandMetadata {
        log_invocation: matches!(
            command.cli_name(),
            "compile"
                | "compile-single"
                | "test"
                | "test-compile"
                | "run"
                | "watch"
                | "lint"
                | "fmt"
                | "frame"
                | "sim"
                | "stone-test"
                | "stone"
        ),
        compiler_process: if command.cli_name() == "check"
            || matches!(
                command.operation(),
                Some(
                    Operation::Compile
                        | Operation::CompileSingle
                        | Operation::Test
                        | Operation::TestCompile
                )
            ) {
            registry::CompilerProcess::Always
        } else if command.operation() == Some(Operation::Sim) {
            registry::CompilerProcess::WhenListed
        } else {
            registry::CompilerProcess::Never
        },
        build_command: Some(command.cli_name()),
        implied_channel: None,
    }
}

pub static EARLY_ENTRIES: &[registry::EarlyEntry] = &[
    // LSP owns its logger, so dispatch it before the process installs the CLI logger.
    registry::EarlyEntry {
        name: "lsp",
        run: |rest| {
            let mut forwarded = vec!["rock lsp".to_string()];
            forwarded.extend(rest);
            match rock_lsp::server_cli::run_cli(forwarded) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        },
    },
];

pub static REGISTRATIONS: registry::Registrations = registry::Registrations {
    commands: COMMANDS,
    backends: BACKENDS,
    option_rules: OPTION_RULES,
    arguments: crate::sim_arguments::arguments,
    decode_arguments: crate::sim_arguments::decode,
    command_metadata,
    scan_excluded: |_| false,
    early_entries: EARLY_ENTRIES,
    runtime_services: &RUNTIME_SERVICES,
    initialize_process: crate::frame_commands::initialize_process,
};
