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
    let pos_subcmd = |name: &'static str,
                      about: &'static str,
                      pos_name: &'static str,
                      pos_help: &'static str| {
        Command::new(name)
            .about(about)
            .arg(Arg::new(pos_name).required(true).help(pos_help))
            .arg(trace_flag())
    };
    let file_subcmd = |name: &'static str, about: &'static str| {
        pos_subcmd(name, about, "file", "The Rock file to analyze")
    };
    let position_subcmd = |name: &'static str, about: &'static str| {
        pos_subcmd(
            name,
            about,
            "position",
            "The position in format file:line:column (1-indexed)",
        )
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
        .subcommand(file_subcmd("lint", "Run lints (unused_var, unused_fn) over a Rock file or directory"))
        .subcommand(
            Command::new("inline-lua-census")
                .about("Census of inline(lua) fn bodies (meaningful Lua lines) per file, to track and ratchet down raw Lua")
                .arg(Arg::new("file").required(true).num_args(1..).help("Rock file(s) or director(ies) to census (all merged into one report)"))
                .arg(
                    Arg::new("ignore")
                        .long("ignore")
                        .action(ArgAction::Append)
                        .help(
                            "Plain substring (repeatable, not a glob): a match against a file's \
                             displayed path drops the whole file (e.g. --ignore stdlib/), a match \
                             against a fn name drops that fn.",
                        ),
                )
                .arg(
                    Arg::new("summary")
                        .long("summary")
                        .action(ArgAction::SetTrue)
                        .help("Print only the one-line TOTAL (for tracking the number over time)."),
                )
                .arg(trace_flag()),
        )
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
        .subcommand(
            position_subcmd("completion", "Get completions at a specific position")
                .arg(
                    Arg::new("explain")
                        .long("explain")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Human-readable ranking breakdown: one line per item with the \
                             decoded RankKey rungs (exact/type/band/case/kind/dist/score) \
                             instead of JSON",
                        ),
                )
                .arg(
                    Arg::new("prefixes")
                        .num_args(0..)
                        .help(
                            "Optional 'what if I typed this' prefixes. When given, splice each \
                             in place of the identifier at the cursor and print a printable \
                             matrix (rank x match-policy). watch-friendly for matcher tuning.",
                        ),
                )
                .arg(
                    Arg::new("policies")
                        .long("policies")
                        .help("Comma-separated admission policies to compare (clean,subseq,subseq3). Default: all three."),
                )
                .arg(
                    Arg::new("top")
                        .long("top")
                        .value_parser(clap::value_parser!(usize))
                        .help("Rows per policy column (default 10)."),
                )
                .arg(
                    Arg::new("repl")
                        .long("repl")
                        .action(ArgAction::SetTrue)
                        .help("Interactive: load once, then type a prefix per line to reprint the policy matrix (blank/`q` quits)."),
                ),
        )
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

fn open_debug_log(log_path: &str) -> Result<fs_err::File> {
    fs_err::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| anyhow::anyhow!("ROCK_LSP_DEBUG: cannot open {log_path}: {e}"))
}

fn top_arg(m: &ArgMatches) -> usize {
    m.get_one::<usize>("top").copied().unwrap_or(10)
}

/// Stderr filter at a warn floor, refined by `RUST_LOG` directives
/// (`info`, `rock_lsp=debug,warn`, ...). An unparsable `RUST_LOG` is an error.
fn stderr_log_filter(rust_log: Option<&str>, timing: bool) -> Result<rock_log::Filter> {
    // ROCK_LSP_TIMING=1: surface the per-request handle timer (info on
    // the `lsp_timing` target) without lifting the default warn floor
    // for everything else, so `:LspLog` shows timing and nothing else.
    let timing_override = [("lsp_timing", log::LevelFilter::Info)];
    let overrides: &[_] = if timing { &timing_override } else { &[] };
    rock_log::Filter::parse(log::LevelFilter::Warn, rust_log, overrides).map_err(|e| {
        anyhow::anyhow!(
            "RUST_LOG={:?} is not a valid log filter: {e}",
            rust_log.unwrap_or_default()
        )
    })
}

/// Install the LSP logger and run the requested `rock lsp` subcommand.
///
/// `args` is the full argv for the LSP CLI with `argv[0]` a display name
/// (`rock-cli` passes `"rock lsp"`); clap reads the subcommand from `args[1]`.
/// Owns its own logger install — callers must NOT have installed a global
/// logger first (`rock-cli` intercepts `lsp` before `rock_log::init()`).
/// It is the shared rock-log backend with a per-target filter.
pub fn run_cli(args: Vec<String>) -> Result<()> {
    run_cli_with_policy(args, crate::CompilerPolicy::default())
}

pub fn run_cli_with_policy(args: Vec<String>, policy: crate::CompilerPolicy) -> Result<()> {
    // ROCK_LSP_DEBUG=1 -> lsp_debug.log in /tmp (Windows: system temp directory).
    // ROCK_LSP_DEBUG=/some/path -> that path at trace level
    // Unset -> stderr at warn (or RUST_LOG level)
    match std::env::var("ROCK_LSP_DEBUG") {
        Ok(val) => {
            let log_path = if val == "1" || val.is_empty() {
                crate::default_debug_file("lsp_debug.log")
                    .to_string_lossy()
                    .into_owned()
            } else {
                val
            };
            let file = open_debug_log(&log_path)?;
            let filter = rock_log::Filter::parse(log::LevelFilter::Trace, None, &[])
                .map_err(|e| anyhow::anyhow!("ROCK_LSP_DEBUG filter: {e}"))?;
            rock_log::init_filtered(filter, rock_log::Sink::File(file.into_parts().0));
            log::trace!("LSP_MAIN_STARTED (logging to {log_path})");
        }
        Err(_) => {
            let rust_log = std::env::var("RUST_LOG").ok();
            let filter = stderr_log_filter(
                rust_log.as_deref(),
                std::env::var_os("ROCK_LSP_TIMING").is_some(),
            )?;
            rock_log::init_filtered(filter, rock_log::Sink::Stderr);
        }
    }

    let matches = build_cli().get_matches_from(args);

    match matches.subcommand() {
        None | Some(("serve", _)) => crate::run_lsp_server_with_policy(policy),
        Some(("diagnostics", m)) => {
            cli::bone_lsp::diagnostics_with_policy(arg_str(m, "file"), policy)
        }
        Some(("lint", m)) => cli::bone_lsp::lint_with_policy(arg_str(m, "file"), policy),
        Some(("inline-lua-census", m)) => {
            let paths: Vec<String> = m
                .get_many::<String>("file")
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            let ignores: Vec<String> = m
                .get_many::<String>("ignore")
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            cli::inline_lua_census::inline_lua_census(&paths, &ignores, m.get_flag("summary"))
        }
        Some(("semantic-tokens", m)) => cli::bone_lsp::semantic_tokens(arg_str(m, "file"), policy),
        Some(("hover", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::hover(&file, line, col, policy)
        }
        Some(("completion", m)) => {
            let (file, line, col) = bone_position(m)?;
            let prefixes: Vec<String> = m
                .get_many::<String>("prefixes")
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            let repl = m.get_flag("repl");
            if prefixes.is_empty() && !repl {
                cli::bone_lsp::completion(&file, line, col, m.get_flag("explain"), policy)
            } else {
                let policies = match m.get_one::<String>("policies") {
                    Some(s) => s
                        .split(',')
                        .map(|p| {
                            bone_query::MatchPolicy::parse(p).ok_or_else(|| {
                                anyhow::anyhow!("unknown policy `{p}` (clean|subseq|subseq3)")
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                    None => vec![
                        bone_query::MatchPolicy::Clean,
                        bone_query::MatchPolicy::Subseq,
                        bone_query::MatchPolicy::Subseq3,
                    ],
                };
                let top = top_arg(m);
                if repl {
                    cli::bone_lsp::completion_repl(&file, line, col, &policies, top, policy)
                } else {
                    cli::bone_lsp::completion_matrix(
                        &file, line, col, &prefixes, &policies, top, policy,
                    )
                }
            }
        }
        Some(("definition", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::definition(&file, line, col, policy)
        }
        Some(("type-definition", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::type_definition(&file, line, col, policy)
        }
        Some(("references", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::references(&file, line, col, policy)
        }
        Some(("hole-info", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::hole_info(&file, line, col, policy)
        }
        Some(("inlay-hints", m)) => cli::bone_lsp::inlay_hints(arg_str(m, "file"), policy),
        Some(("prepare-rename", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::prepare_rename(&file, line, col, policy)
        }
        Some(("rename", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::rename(&file, line, col, arg_str(m, "new_name"), policy)
        }
        Some(("signature-help", m)) => {
            let (file, line, col) = bone_position(m)?;
            cli::bone_lsp::signature_help(&file, line, col, policy)
        }
        Some(("document-symbols", m)) => {
            cli::bone_lsp::document_symbols(arg_str(m, "file"), policy)
        }
        Some(("workspace-symbols", m)) => {
            cli::bone_lsp::workspace_symbols(arg_str(m, "file"), arg_str(m, "query"), policy)
        }

        Some((name, _)) => unreachable!("unhandled subcommand: {}", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    /// Which of `(target, level)` the filter built from these settings enables.
    fn enabled(rust_log: Option<&str>, timing: bool, probes: &[(&str, Level)]) -> Vec<bool> {
        let filter = stderr_log_filter(rust_log, timing).unwrap();
        probes
            .iter()
            .map(|(target, level)| filter.enabled(target, *level))
            .collect()
    }

    #[test]
    fn rust_log_accepts_standard_directives() {
        use Level::*;
        assert_eq!(
            enabled(
                Some("rock_lsp=debug"),
                false,
                &[("rock_lsp", Debug), ("other", Warn), ("other", Info)]
            ),
            [true, true, false]
        );
        assert_eq!(
            enabled(
                Some("info,bone_query=trace"),
                false,
                &[("x", Info), ("x", Debug), ("bone_query", Trace)]
            ),
            [true, false, true]
        );
        assert_eq!(
            enabled(
                None,
                true,
                &[("x", Warn), ("x", Info), ("lsp_timing", Info)]
            ),
            [true, false, true]
        );
    }

    #[test]
    fn rust_log_rejects_unparsable_directives() {
        for bad in ["rock_lsp=loud", "a=b=c"] {
            let err = stderr_log_filter(Some(bad), false)
                .err()
                .expect(bad)
                .to_string();
            assert!(err.contains("RUST_LOG") && err.contains(bad), "{err}");
        }
    }

    #[test]
    fn top_must_be_a_number() {
        fn args(top: &str) -> [&str; 6] {
            ["rock lsp", "completion", "a.rock:1:1", "x", "--top", top]
        }
        assert!(build_cli().try_get_matches_from(args("abc")).is_err());
        let m = build_cli().try_get_matches_from(args("3")).unwrap();
        let (_, m) = m.subcommand().unwrap();
        assert_eq!(top_arg(m), 3);
    }

    #[test]
    fn unopenable_debug_log_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-dir").join("lsp.log");
        let err = open_debug_log(path.to_str().unwrap())
            .expect_err("open must fail")
            .to_string();
        assert!(
            err.contains("ROCK_LSP_DEBUG") && err.contains("missing-dir"),
            "{err}"
        );
    }
}
