pub use colored::*;
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
pub mod bone_backend;
pub mod child_lifetime;
mod cli;
pub mod common;
pub mod compile_budget;
pub(crate) mod compile_cmd;
pub(crate) mod compile_slots;
pub mod comptime;
pub(crate) mod docs_index;
mod error_test;
mod file_watcher;
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
mod process;
pub mod runtime_process;
pub mod watch;
pub(crate) use lint_cmd::do_lint;
pub(crate) use lsp_survey::lsp_survey;
pub use process::main;

pub use cli::{Args, Backend, Command};
pub use common::*;
pub(crate) use formatting::*;

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
