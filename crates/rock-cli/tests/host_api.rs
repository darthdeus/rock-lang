//! The public surface that out-of-tree embedding binaries build against.
//!
//! Several of these items have no caller inside this workspace, so a dead-code
//! sweep sees them as unused; deleting one breaks embedders silently (see the
//! commit that restored the host APIs). Naming them here turns that deletion
//! into a compile error. Remove an entry only together with its embedders.

use std::path::Path;

use rock_cli::registry;

#[test]
fn host_api_items_stay_public() {
    // Process entry and host registration tables.
    let _ = rock_cli::main_with;
    let _ = rock_cli::compiler_process;
    let _ = registry::install;
    let _ = registry::registered;
    let _ = registry::extend_arguments;
    let _: &registry::Registrations = &rock_cli::plain::REGISTRATIONS;
    let _: &[&registry::CliCommand] = rock_cli::plain::COMMANDS;
    let _: &[registry::BackendSpec] = rock_cli::plain::BACKENDS;
    let _: &[registry::OptionRule] = rock_cli::plain::OPTION_RULES;
    let _: &[registry::EarlyEntry] = rock_cli::plain::EARLY_ENTRIES;
    let _: registry::RuntimeServices = rock_cli::plain::RUNTIME_SERVICES;
    let _ = rock_cli::plain::command_metadata;
    let _ = std::any::type_name::<registry::ArgumentGroup>();
    let _ = std::any::type_name::<registry::CompilerProcess>();
    let _ = std::any::type_name::<registry::CommandMetadata>();
    let _ = std::any::type_name::<registry::ProgramHost>();
    let _ = std::any::type_name::<registry::FixHandler>();
    let _ = std::any::type_name::<registry::Handler>();
    let _ = std::any::type_name::<rock_cli::Args>();

    // Command helpers.
    let _ = rock_cli::CommandContext::get_output_or_default;
    let _ = rock_cli::comptime::invocation_selection_args;
    let _ = rock_cli::total_system_memory_mib;
    let _ = rock_cli::headless_scope::inherits_headless;
    let _ = rock_cli::headless_scope::outside_kill_note;
    let _ = rock_cli::image_snapshot::image_diff;
    let _ = rock_cli::file_watcher::FileWatcher::with_ignored_roots;
    let _ = rock_cli::live_project::LiveProject::entry_module;

    // Packaging.
    let _ = rock_cli::package::run_package;

    // Incremental skull projects and sims.
    let _ = rock_cli::skull_backend::collect_sim_descriptors;
    let _ = std::any::type_name::<rock_cli::skull_backend::SimDescriptor>();
    let _ = rock_cli::skull_backend::IncrementalProject::init_with_tests;
    let _ = rock_cli::skull_backend::IncrementalProject::init_with_tests_and_allocation_report;
    let _ = rock_cli::skull_backend::IncrementalProject::take_last_route_profile;
    let _ = init_for_sim_selection;

    // Host paths.
    let _ = rock_host::install_source_root;
    let _ = rock_host::project::find_source_root;
    let _ = rock_host::project::ensure_workspace_root;
    let _: &str = rock_host::project::MAIN_BYTECODE_FILENAME;

    // Language server.
    let _ = rock_lsp::run_lsp_server;
    let _ = rock_lsp::completion_record::init_recorder_at;
}

// Generic over its selector, so it is named through a call instead of a path.
fn init_for_sim_selection(
    dir: &str,
    main: &Path,
    incremental: &Path,
) -> anyhow::Result<rock_cli::skull_backend::IncrementalProject> {
    rock_cli::skull_backend::IncrementalProject::init_for_sim_selection(
        dir,
        main,
        incremental,
        None,
        |_| Ok(None),
    )
}
