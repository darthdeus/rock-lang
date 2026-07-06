//! The `rock lsp <subcommand>` entry point. Drives the persistent LSP server
//! (`serve`) and every one-shot CLI handler (hover, completion, diagnostics,
//! ...). Invoked from `rock-cli`'s `main` after it strips the leading `lsp`
//! token; there is no standalone `rock-lsp` binary (removed so a single `rock`
//! artifact can never go stale against the stdlib it embeds).

use crate::cli;
use anyhow::Result;
use clap::{Arg, ArgAction, ArgMatches, Command};

fn build_cli() -> Command {
    // Most subcommands take a single positional + --trace. A few take
    // (file, query) or (position, new_name). Builders for both shapes.
    let trace_flag = || {
        Arg::new("trace")
            .long("trace")
            .action(ArgAction::SetTrue)
            .help("Enable trace file generation (trace.folded and trace.perfetto) for profiling")
    };
    let pos_subcmd = |name: &'static str, about: &'static str, pos_name: &'static str, pos_help: &'static str| {
        Command::new(name)
            .about(about)
            .arg(Arg::new(pos_name).required(true).help(pos_help))
            .arg(trace_flag())
    };
    let file_subcmd = |name: &'static str, about: &'static str| {
        pos_subcmd(name, about, "file", "The Rock file to analyze")
    };
    let position_subcmd = |name: &'static str, about: &'static str| {
        pos_subcmd(name, about, "position", "The position in format file:line:column (1-indexed)")
    };

    Command::new("rock lsp")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Rock LSP server and CLI tools")
        .subcommand(
            Command::new("serve")
                .about("Start the LSP server (default if no command specified)")
                .arg(trace_flag()),
        )
        .subcommand(file_subcmd("diagnostics", "Get diagnostics for a Rock file"))
        .subcommand(
            Command::new("semantic-tokens")
                .about("Get semantic tokens for a Rock file")
                .arg(Arg::new("file").required(true).help("The Rock file to analyze"))
                .arg(
                    Arg::new("range")
                        .long("range")
                        .help("Line range to display tokens for (e.g., \"10-20\", \"10\", \"10-\")"),
                )
                .arg(trace_flag()),
        )
        .subcommand(position_subcmd("hover", "Get hover information at a specific position"))
        .subcommand(position_subcmd("completion", "Get completions at a specific position"))
        .subcommand(position_subcmd("definition", "Go to definition at a specific position"))
        .subcommand(position_subcmd("type-definition", "Go to the declaration of the type of the value at a specific position"))
        .subcommand(position_subcmd("references", "Find references at a specific position"))
        .subcommand(position_subcmd(
            "hole-info",
            "Print typed-hole context (expected type + in-scope bindings) for the hole under the cursor.",
        ))
        .subcommand(file_subcmd("inlay-hints", "Get inlay hints for a Rock file"))
        .subcommand(position_subcmd("prepare-rename", "Prepare rename at a specific position"))
        .subcommand(
            Command::new("rename")
                .about("Rename symbol at a specific position")
                .arg(Arg::new("position").required(true).help("The position in format file:line:column (1-indexed)"))
                .arg(Arg::new("new_name").required(true).help("The new name for the symbol"))
                .arg(trace_flag()),
        )
        .subcommand(position_subcmd("signature-help", "Get signature help at a specific position"))
        .subcommand(file_subcmd("document-symbols", "Get document symbols for a Rock file"))
        .subcommand(
            Command::new("workspace-symbols")
                .about("Search for symbols in the workspace")
                .arg(Arg::new("file").required(true).help("The Rock file to analyze (acts as workspace root)"))
                .arg(Arg::new("query").required(true).help("The search query"))
                .arg(trace_flag()),
        )
}

fn arg_str<'a>(m: &'a ArgMatches, name: &str) -> &'a str {
    m.get_one::<String>(name).map(|s| s.as_str()).unwrap_or("")
}

/// Parse a `file:line:column` position the same way the Lua CLI handlers do.
fn bone_position(m: &ArgMatches) -> Result<(String, u32, u32)> {
    cli::parse_position(arg_str(m, "position"))
}

/// Install the LSP logger and run the requested `rock lsp` subcommand.
///
/// `args` is the full argv for the LSP CLI with `argv[0]` a display name
/// (`rock-cli` passes `"rock lsp"`); clap reads the subcommand from `args[1]`.
/// Owns its own logger install — callers must NOT have installed a global
/// logger first (`rock-cli` intercepts `lsp` before `rock_log::init()`).
pub fn run_cli(args: Vec<String>) -> Result<()> {
    // ROCK_LSP_DEBUG=1 -> /tmp/lsp_debug.log at trace level (tail -f friendly)
    // ROCK_LSP_DEBUG=/some/path -> that path at trace level
    // Unset -> stderr at warn (or RUST_LOG level)
    match std::env::var("ROCK_LSP_DEBUG") {
        Ok(val) => {
            let log_path = if val == "1" || val.is_empty() {
                "/tmp/lsp_debug.log".to_string()
            } else {
                val
            };
            let file = fs_err::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .unwrap_or_else(|e| panic!("ROCK_LSP_DEBUG: cannot open {log_path}: {e}"));
            env_logger::Builder::new()
                .filter_level(log::LevelFilter::Trace)
                .target(env_logger::Target::Pipe(Box::new(file)))
                .format_timestamp_secs()
                .init();
            log::trace!("LSP_MAIN_STARTED (logging to {log_path})");
        }
        Err(_) => {
            let log_level = match std::env::var("RUST_LOG") {
                Ok(level) => level.parse().unwrap_or(log::LevelFilter::Warn),
                Err(_) => log::LevelFilter::Warn,
            };
            let mut builder = env_logger::builder();
            builder.filter_level(log_level).format_timestamp(None);
            // ROCK_LSP_TIMING=1: surface the per-request handle timer (info on
            // the `lsp_timing` target) without lifting the default warn floor
            // for everything else, so `:LspLog` shows timing and nothing else.
            if std::env::var_os("ROCK_LSP_TIMING").is_some() {
                builder.filter(Some("lsp_timing"), log::LevelFilter::Info);
            }
            builder.init();
        }
    }

    let matches = build_cli().get_matches_from(args);

    match matches.subcommand() {
        None | Some(("serve", _)) => crate::run_lsp_server(),
        Some(("diagnostics", m)) => cli::bone_lsp::diagnostics(arg_str(m, "file")),
        Some(("semantic-tokens", m)) => cli::bone_lsp::semantic_tokens(arg_str(m, "file")),
        Some(("hover", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::hover(&file, line, col)
        }
        Some(("completion", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::completion(&file, line, col)
        }
        Some(("definition", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::definition(&file, line, col)
        }
        Some(("type-definition", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::type_definition(&file, line, col)
        }
        Some(("references", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::references(&file, line, col)
        }
        Some(("hole-info", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::hole_info(&file, line, col)
        }
        Some(("inlay-hints", m)) => cli::bone_lsp::inlay_hints(arg_str(m, "file")),
        Some(("prepare-rename", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::prepare_rename(&file, line, col)
        }
        Some(("rename", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::rename(&file, line, col, arg_str(m, "new_name"))
        }
        Some(("signature-help", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::signature_help(&file, line, col)
        }
        Some(("document-symbols", m)) => cli::bone_lsp::document_symbols(arg_str(m, "file")),
        Some(("workspace-symbols", m)) => {
            cli::bone_lsp::workspace_symbols(arg_str(m, "file"), arg_str(m, "query"))
        }

        Some((name, _)) => unreachable!("unhandled subcommand: {}", name),
    }
}
