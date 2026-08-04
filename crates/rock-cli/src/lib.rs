pub use colored::*;
pub use fs_extra::{copy_items, dir::CopyOptions};
pub use notify_debouncer_full::DebouncedEvent;
pub use std::path::PathBuf;
pub use walkdir::WalkDir;

#[cfg(windows)]
pub use std::os::windows::io::AsRawHandle;

// rock-cli's prelude: symbols the kept code reaches through `crate::*`. Core
// types come straight from rock_core + rock_tokenizer, host helpers from
// rock_host, and the rest from std / common deps.
pub use anyhow::{bail, Result};
pub use atomic_refcell::AtomicRefCell;
pub use ustr::Ustr;
pub use std::path::Path;
pub use std::sync::Arc;
pub use rock_core::*;
pub use rock_tokenizer::*;
pub use rock_host::process_management::*;
pub use rock_host::source_files::{
    source_file_from_code, source_file_new_preprocessed, SourceFilesExt,
};
pub use rock_host::stick::StickConfig;
pub use std::collections::{HashMap, HashSet};
pub use std::sync::mpsc::channel;
pub use std::time::Duration;
#[allow(unused_imports)]
pub use log::{debug, error, info, trace, warn};

pub mod backend_stats;
pub mod bone_backend;
pub mod skull_backend;
pub mod common;
mod compile_cmd;
mod dispatcher;
mod error_test;
mod file_watcher;
pub mod game_runtime;
mod formatting;
mod completion_episodes;
mod completion_replay;
mod completion_sweep;
mod iter_completion;
mod iter_lsp;
mod iter_parse;
pub mod glsl_test;
pub mod js_test;
pub mod live_project;
pub mod web;
mod test_protocol;
mod lint_cmd;
pub(crate) mod lint_union;
pub mod lint_pass;
pub mod ablation_report;
mod mono_manifest;
mod monoty_census;
mod lsp_survey;
mod lsp_test;
mod parallel_runner;
mod image_video;
mod run_sim;
mod stone;
pub mod stone_test;
mod test;
mod text_utils;
pub mod unified_compile;
mod watch;

pub use common::*;
pub use dispatcher::*;
pub use file_watcher::*;
pub use formatting::*;
pub use run_sim::*;
pub use stone::*;
pub use stone_test::*;
pub use text_utils::*;
pub use unified_compile::*;
pub use watch::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CliCommand {
    Compile,
    CompileSingle,
    Test,
    TestCompile,
    Run,
    Watch,
    Fmt,
    Frame,
    Stone,
    /// `rock replay <game> <file>` — windowed replay playback of a recorded
    /// log (docs/replay-scrub-plan.md P3.5). Unlike `rock frame --replay`
    /// (batch headless, exits at log end), this opens a window, plays the log,
    /// and freezes at the last tick with the timeline bar live so the developer
    /// can scrub. Do NOT overload `rock frame` -- its contract is batch headless.
    Replay,
    /// `rock stone-test <root>` — long-lived headless watch process driven
    /// over JSON-line stdio. Test infrastructure for incremental
    /// compilation / hot-reload assertions. See
    /// `docs/stone-test-harness-plan.md`.
    StoneTest,
    Perf,
    /// `rock sim <project> <name>` — run a Rock-native `@sim`
    /// coroutine. Spawns the game with `ROCK_RUN_SIM=<name>`,
    /// demuxes `__SIM__`-prefixed JSON lines from stderr, prints a
    /// pass/fail summary, exits non-zero on any expect_fail.
    /// `<name>` may be comma-separated for chained multi-start
    /// (`a,b` -> two luajit children sharing one `$OUT` tempdir).
    Sim,
    /// `rock lint <path>` -- run the lint catalog (`crates/bone-lints`) over a
    /// file or project and report the findings. The author-facing surface of the
    /// same engine the LSP and the build pass drive; exits non-zero only on an
    /// `Error`-severity finding, matching the build gate. Dispatched
    /// pre-backend-switch (like `fmt`/`shader`): lints read bone's typechecked
    /// output, no codegen involved.
    Lint,
    /// `rock completions <shell>` — print a shell completion script
    /// to stdout. Shells: bash, zsh, fish, powershell, elvish.
    /// Install (zsh): `rock completions zsh > ~/.zsh/_rock` then add
    /// the dir to `$fpath` and `compinit`. Install (bash):
    /// `eval "$(rock completions bash)"` from `~/.bashrc`.
    Completions,
    /// `rock iter-compile <project> --iters N` — build the bone workspace once,
    /// then re-run only the (parallel) inference phase N times. A loop body for
    /// sampling profilers (`samply record -- rock iter-compile ...`) so nearly
    /// all samples land in inference rather than parsing. Bone-only; ignores
    /// `--backend`. See `bone_backend::bone_iter_inference`.
    IterCompile,
    /// `rock iter-parse <project> --iters N` -- discover sources once, then
    /// re-tokenize + re-parse every user `.rock` file N times. A loop body for
    /// sampling profilers so nearly all samples land in the tokenizer/parser.
    /// Backend-independent (the front-end is shared); ignores `--backend`. See
    /// `iter_parse::iter_parse`.
    IterParse,
    /// `rock iter-tokenize <project> --iters N` -- discover sources once, then
    /// re-tokenize every user `.rock` file N times (the parser is never
    /// constructed). The tokenize-only counterpart to `iter-parse`, for
    /// profiling the lexer in isolation. Ignores `--backend`. See
    /// `iter_parse::iter_tokenize`.
    IterTokenize,
    /// `rock iter-codegen <project> --iters N` — build the bone workspace once
    /// (parse + semantic), then re-run only skull's `emit_workspace` (Lua
    /// codegen: lower MIR -> inline -> regalloc -> stringify) N times. A loop
    /// body for sampling profilers (`samply record -- rock iter-codegen ...`) so
    /// nearly all samples land in codegen. Build with `--features sequential`
    /// for single-threaded attribution. skull-only. See
    /// `skull_backend::skull_iter_codegen`.
    IterCodegen,
    /// `rock iter-cook <project> --iters N` — parse stdlib+project to raw skull
    /// ASTs once, then loop `clone + skull_parser::cook` (macro-expand +
    /// desugar) — exactly what bone's `expand_and_desugar` does per compile.
    /// A tight body for sampling profilers. See `skull_backend::skull_iter_cook`.
    IterCook,
    /// `rock skull-inline-stats <project>` — DIAGNOSTIC: replay skull's function
    /// inliner over the workspace and report what gets inlined and how much code
    /// each splice copies (per-callee fan-out + inst-copies, top callers). Writes
    /// `skull-inline-stats.txt`. See `skull_backend::skull_inline_stats`.
    SkullInlineStats,
    /// `rock skull-incremental-profile <project>` — byte-identical correctness
    /// gate for the full-reassembly `emit_incremental` path: for each project
    /// file force a body re-cook, time `update_file` + `emit_incremental`, and
    /// verify the incremental output is byte-identical to a fresh `emit_full`.
    /// (This is the reassembly path, NOT the two-file overlay the live watcher
    /// runs -- byte-identity only holds for reassembly; the overlay's gate is the
    /// runtime-equivalence oracle.) See `skull_backend::skull_incremental_profile`.
    SkullIncrementalProfile,
    /// `rock iter-incremental <project> [--iters N --warmup W]` — tight-loop
    /// profiler for the REAL stone incremental path: warm a Session once, then
    /// repeatedly `update_file` + `emit_overlay` one body (alternating whitespace
    /// so every pass is a real change) and report the timing distribution
    /// (mean/p50/p99) + overlay size. Mirrors what `SkullStone::recompile` does on
    /// a body edit (no reassembly). See `skull_backend::skull_iter_incremental`.
    IterIncremental,
    /// `rock mono-ty-census [root]` -- DIAGNOSTIC (Stage 0c,
    /// `docs/monomorphization-plan.md`): convert every inferred expr/binding
    /// type of the closed world to the total `MonoTy` and report conversions,
    /// distinct logical types, and failures bucketed by reason. Changes no
    /// emission. See `monoty_census::do_monoty_census`.
    MonoTyCensus,
    /// `rock mono-manifest [root]` -- DIAGNOSTIC (Stage 0d,
    /// `docs/monomorphization-plan.md`): build the SHADOW instance graph over
    /// the closed world (instances keyed by `MonoTy`, not `SpecKey`), and
    /// report instance/edge counts, the edge-kind histogram and the recursion
    /// census. Changes no emission. See `mono_manifest::do_mono_manifest`.
    MonoManifest,
    /// `rock iter-lsp <project> [--iters N --warmup W]` -- tight-loop profiler
    /// for the per-edit LSP query path on the bone backend (the work the editor
    /// does per keystroke through `BoneWorkspace`): `update` (didChange) +
    /// `semantic_tokens` + `inlay_hints` refresh, plus on-demand
    /// `hover`/`completion`/`definition` at the edit cursor. Warms the whole
    /// compilation unit once, then alternates a 1-char body edit and reports the
    /// per-op timing distribution. `ITER_LSP_FN`/`ITER_LSP_FILE` target the edit.
    /// See `iter_lsp::iter_lsp`.
    IterLsp,
    /// `rock lsp-survey <project> [--iters N --format json]` -- systematically
    /// hunt for the slowest LSP edit. Collects per-module/per-function stats
    /// from the bone workspace (body size, call fan-in, module fan-in), then
    /// generates + profiles candidate edits across all tiers (body/signature/
    /// broken) through `BoneWorkspace`, ranking by worst per-edit latency.
    /// `--format json` dumps the full stats + samples. See `lsp_survey::lsp_survey`.
    LspSurvey,
    /// `rock iter-reinfer <project> [--iters N --warmup W]` -- tight loop benching
    /// the Interface-tier re-inference path (a signature edit reinfers every
    /// dependent body). Warms a bone Session, alternates a signature edit on a
    /// high-fan-out fn, times `update_file`, and reports the reinferred-body count
    /// + timing distribution. `ITER_REINFER_FN` targets the fn (else auto-picked
    /// by dependent-module count). A samply body. See `bone_backend::bone_iter_reinfer`.
    IterReinfer,
    /// `rock iter-completion <project> [--iters N --warmup W]` -- tight-loop
    /// profiler for the per-keystroke `bone.completion()` path (the slow part is
    /// `call_through_param_completions`). Warms the workspace, places a cursor at
    /// a call-through-triggering prefix in a fn body, then loops `completion()`
    /// reporting the latency distribution. `ITER_COMPL_PREFIX` overrides the
    /// synthetic prefix, `ITER_COMPL_FILE` the target file. Built to run under
    /// `perf record`. See `iter_completion::iter_completion`.
    IterCompletion,
    /// `rock completion-sweep <project> [--stride N]` -- deterministic
    /// completion-output dump over a derived corpus of cursor positions
    /// (identifier ends + after-dot member slots), printed to stdout for
    /// byte-identity diffing across a refactor. The M0 gate of
    /// `docs/completion-v2-design.md`. See `completion_sweep::completion_sweep`.
    CompletionSweep,
    /// `rock completion-replay <corpus.jsonl>...` -- re-rank recorded
    /// completion accepts (nvim completion_log / ROCK_LSP_RECORD rows carrying
    /// the full buffer) against the CURRENT ranker and report per-case ranks +
    /// aggregate MRR. The M6 tuning oracle of `docs/completion-v2-design.md`.
    /// See `completion_replay::completion_replay`.
    CompletionReplay,
    /// `rock completion-episodes <log.jsonl>...` -- stitch completion-log rows
    /// into episodes of user struggle (accept / typed_out / abandoned; the
    /// final word at the anchor is ground truth) and print the tuning
    /// metrics: chars-to-top3, never-surfaced leaderboard, hunted accepts,
    /// repeat-struggle words. `docs/completion-episodes.md`. See
    /// `completion_episodes::completion_episodes`.
    CompletionEpisodes,
    /// `rock iter-cascade <project> [--iters N --warmup W]` -- tight loop benching
    /// the Interface-tier RE-LOWER cascade: a field edit on a high-fan-in TYPE
    /// re-lowers every module whose interface references it (vs `iter-reinfer`,
    /// which edits a fn signature and only re-infers). Warms a bone Session,
    /// alternates a struct-field edit on the type with the most interface
    /// dependents, times `update_file`, and reports the re-lowered module count +
    /// timing. `ITER_CASCADE_TYPE` targets the type. See
    /// `bone_backend::bone_iter_cascade`.
    IterCascade,
    /// `rock backend-stats <X>` — bone-vs-skull performance comparison on one
    /// input (file / test file / project): bone's semantic (typecheck floor) vs
    /// skull's semantic + emit (codegen delta), whole workspace, mean of
    /// `--iters` timed runs after `--warmup`. Runs BOTH backends itself, so it
    /// ignores `--backend`. See `backend_stats::do_backend_stats`.
    BackendStats,
    /// `rock shader <file.rock> <fn> [fn...] --output <file.glsl>` -- GLSL
    /// function-library emission (route B, `docs/shaders.md`). Type-checks
    /// the file against the stdlib, then emits GLSL fn definitions for the named
    /// root fns + their reachable tier-0 callees, topologically ordered. Strict
    /// profile (rejects anything above tier 0). Backend-agnostic: dispatched before
    /// the backend switch, like `fmt`/`completions`.
    Shader,
    /// `rock shader-validate <project> [project...]` -- compile+link every
    /// `rock_shader` pair the project(s) register, on REAL GL contexts in both
    /// profiles (330core + 300es), via `stone-glsl-runner`'s compile-only mode
    /// (`docs/shaders.md` § Testing). A driver rejection is a FAIL
    /// carrying the compile log; a no-display environment skips-with-log
    /// (runner exit 2, the one sanctioned skip); zero verdicts exits non-zero.
    /// See `glsl_test::shader_validate`.
    ShaderValidate,
    /// `rock web <project> [--out <dir>]` -- emit a static browser build
    /// (`index.html` + `game.js`) for a Rock project via the JS backend. A
    /// `CliCommand` dispatched pre-backend-switch (like `fmt`/`shader`): it runs
    /// project-mode JS emission and assembles the browser bundle (runtime
    /// preludes + `JS_WEB_RUNTIME` + emitted program + a boot line). Serve the
    /// out dir with any static file server. See `docs/stone-web-plan.md` Arc 2.
    Web,
}

impl clap::ValueEnum for CliCommand {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self::Compile, Self::CompileSingle, Self::Test,
            Self::TestCompile, Self::Run, Self::Watch, Self::Fmt, Self::Frame,
            Self::Stone, Self::Replay, Self::Perf,
            Self::Sim,
            Self::Completions,
            Self::Lint,
            Self::StoneTest,
            Self::IterCompile,
            Self::IterParse,
            Self::IterTokenize,
            Self::IterCodegen,
            Self::IterCook,
            Self::SkullInlineStats,
            Self::SkullIncrementalProfile,
            Self::IterIncremental,
            Self::MonoTyCensus,
            Self::MonoManifest,
            Self::IterLsp,
            Self::LspSurvey,
            Self::IterReinfer,
            Self::IterCompletion,
            Self::CompletionSweep,
            Self::CompletionReplay,
            Self::CompletionEpisodes,
            Self::IterCascade,
            Self::BackendStats,
            Self::Shader,
            Self::ShaderValidate,
            Self::Web,
        ]
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.cli_name()))
    }
}

impl CliCommand {
    /// User-facing kebab-case name (what the user types and what error
    /// messages echo). Single source for the clap `to_possible_value`
    /// surface and the capability-reject message.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::CompileSingle => "compile-single",
            Self::Test => "test",
            Self::TestCompile => "test-compile",
            Self::Run => "run",
            Self::Watch => "watch",
            Self::Fmt => "fmt",
            Self::Frame => "frame",
            Self::Stone => "stone",
            Self::Replay => "replay",
            Self::StoneTest => "stone-test",
            Self::Perf => "perf",
            Self::Sim => "sim",
            Self::Completions => "completions",
            Self::Lint => "lint",
            Self::IterCompile => "iter-compile",
            Self::IterParse => "iter-parse",
            Self::IterTokenize => "iter-tokenize",
            Self::IterCodegen => "iter-codegen",
            Self::IterCook => "iter-cook",
            Self::SkullInlineStats => "skull-inline-stats",
            Self::SkullIncrementalProfile => "skull-incremental-profile",
            Self::IterIncremental => "iter-incremental",
            Self::MonoTyCensus => "mono-ty-census",
            Self::MonoManifest => "mono-manifest",
            Self::IterLsp => "iter-lsp",
            Self::LspSurvey => "lsp-survey",
            Self::IterReinfer => "iter-reinfer",
            Self::IterCompletion => "iter-completion",
            Self::CompletionSweep => "completion-sweep",
            Self::CompletionReplay => "completion-replay",
            Self::CompletionEpisodes => "completion-episodes",
            Self::IterCascade => "iter-cascade",
            Self::BackendStats => "backend-stats",
            Self::Shader => "shader",
            Self::ShaderValidate => "shader-validate",
            Self::Web => "web",
        }
    }
}

/// Compiler backend selected via `--backend`. The default is `skull` (bone
/// type-check + skull codegen on bone's HIR, emits + runs Lua) for every command
/// skull serves. The backend-agnostic kept commands (fmt, completions,
/// stone-test) are dispatched before the backend switch and never reach a
/// backend. The `bone` backend routes the type-check-capable subset of commands
/// (compile, test, run, watch) through `bone/`'s from-scratch bidirectional
/// typechecker only; it has no codegen, so commands that need to execute a
/// program are rejected with a clear error. See `bone_backend.rs` and
/// `skull_backend.rs`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Bone,
    /// `skull` — the Claude-managed codegen on bone's HIR (emits + runs Lua).
    Skull,
    /// `js` — the JS-subset test executor (`docs/multi-backend-testing-plan.md`
    /// T1): bone type-checks, then each `@test` fn + its callee closure is
    /// emitted via `skull-codegen-js` and run under node; tests outside the
    /// subset report SKIP(reason). Serves `test` only.
    Js,
    /// `glsl` — the GLSL value-test executor (`docs/multi-backend-testing-plan.md`
    /// G1): bone type-checks, then each `@test(returns = ...)` value test is
    /// emitted as a fragment shader (both 330 core + 300 es profiles) and run on
    /// the GPU headlessly, the returned scalar read back and compared host-side.
    /// Tests with no `returns=` are outside the value class; tests whose GLSL
    /// emission rejects report SKIP(reason). Serves `test` only.
    Glsl,
}

impl clap::ValueEnum for Backend {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Bone, Self::Skull, Self::Js, Self::Glsl]
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.name()))
    }
}

impl Backend {
    /// `--backend` value name, used in messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bone => "bone",
            Self::Skull => "skull",
            Self::Js => "js",
            Self::Glsl => "glsl",
        }
    }

    /// One-clause description of what the backend is, for the reject message.
    fn tagline(self) -> &'static str {
        match self {
            Self::Bone => "bone is a typechecker with no codegen/runtime",
            Self::Skull => "skull is a bone-HIR codegen",
            Self::Js => "js is a JS-subset executor (node); no game/engine runtime",
            Self::Glsl => "glsl is a GLSL value-test executor (GPU), not a program codegen",
        }
    }

    /// The commands a backend serves through the per-backend dispatch switch.
    /// Both remaining backends are restricted (the unrestricted legacy `rock`
    /// backend is gone); this is the exhaustive set for each.
    ///
    /// Single source of truth for the capability gate: the reject message's
    /// "Supported" list is generated from this, and each dispatch match's
    /// fallthrough is `unreachable!` because `serves` gated it first.
    ///
    /// Self-routing commands (`iter-*`, the skull diagnostics) and the
    /// backend-agnostic kept commands (fmt, completions, stone-test) are
    /// dispatched *before* the backend switch and bypass this table entirely —
    /// they are not listed here for any backend.
    pub fn supported_commands(self) -> &'static [CliCommand] {
        use CliCommand::*;
        match self {
            Self::Bone => &[
                Test, Compile, CompileSingle, TestCompile, Run, Watch,
            ],
            Self::Skull => &[
                Test, Compile, CompileSingle, TestCompile, Run, Watch, Stone, Frame, Replay, Perf, Sim,
            ],
            Self::Js => &[Test, Run],
            Self::Glsl => &[Test],
        }
    }

    /// Whether this backend can serve `cmd` through the dispatch switch.
    pub fn serves(self, cmd: CliCommand) -> bool {
        self.supported_commands().contains(&cmd)
    }

    /// `Some(message)` if this backend cannot serve `cmd`; `None` if it can.
    /// The uniform reject message, with its "Supported" list generated from
    /// `supported_commands`. The capability gate (`require_backend_support`)
    /// is the only caller; split out so it is pure and unit-testable.
    pub fn reject_reason(self, cmd: CliCommand) -> Option<String> {
        if self.serves(cmd) {
            return None;
        }
        let names: Vec<&str> = self
            .supported_commands()
            .iter()
            .map(|c| c.cli_name())
            .collect();
        Some(format!(
            "the `{}` backend cannot serve `{}` — {}. Supported commands: {}.",
            self.name(),
            cmd.cli_name(),
            self.tagline(),
            names.join(", "),
        ))
    }
}

/// Resolve the effective backend for an invocation. An explicit choice (the
/// `--backend` flag or `ROCK_BACKEND` env, pre-parsed into `explicit`) is
/// honored verbatim -- the capability gate may then reject an unsupported
/// command. With no explicit choice, the default is `skull`. The
/// backend-agnostic kept commands (fmt, completions, stone-test) are dispatched
/// before the backend switch, so a `skull` default never rejects them.
fn resolve_backend(explicit: Option<Backend>, _command: CliCommand) -> Backend {
    explicit.unwrap_or(Backend::Skull)
}

pub struct Args {
    pub command: CliCommand,
    pub files: Vec<String>,
    pub output: Option<String>,
    /// For `rock shader` two-root pipeline form: GLSL profile (`330core` |
    /// `300es`). Default `330core`. See `skull_backend::skull_shader`.
    pub profile: Option<String>,
    pub no_stdlib: bool,
    pub stdout: bool,
    pub check: bool,
    pub iters: usize,
    pub warmup: usize,
    pub verbose: bool,
    pub quiet: bool,
    pub frames: usize,
    pub seconds: Option<f64>,
    pub warmup_seconds: f64,
    pub gc_window: usize,
    pub gc_window_start: usize,
    pub perf_format: String,
    pub type_validation: bool,
    pub headless: bool,
    pub test_threads: Option<usize>,
    pub lib: Option<String>,
    pub clean: bool,
    pub release: bool,
    pub trace: bool,
    pub timeout: u64,
    pub no_timeout: bool,
    pub dump_state: bool,
    pub update_snapshots: bool,
    /// For `rock sim`: how many sim children may run at once. `None` uses the
    /// conservative default in `run_sim::resolve_jobs`.
    pub jobs: Option<usize>,

    /// For `rock frame`: record per-tick input + dt to this replay log
    /// (docs/replay-scrub-plan.md). Mutually exclusive with `--replay`.
    pub record: Option<String>,
    /// For `rock frame`: drive the run from this replay log instead of live
    /// input/wall-clock. Mutually exclusive with `--record`.
    pub replay: Option<String>,

    /// For `rock frame --record`: hash + checkpoint game state every N ticks
    /// for desync detection (docs/replay-scrub-plan.md P2). Default 60; 0
    /// disables checkpointing.
    pub checkpoint_interval: Option<usize>,

    /// For `rock stone`: disable the always-on flight recorder for this
    /// session. Equivalent to STONE_NO_FLIGHT_RECORDER=1.
    pub no_record: bool,

    /// For `rock completion-sweep`: keep every Nth derived probe position
    /// (1 = exhaustive). Controls corpus size vs coverage.
    pub stride: usize,

    /// For `rock iter-completion`: fail (exit nonzero) when the worst
    /// per-keystroke median exceeds this many ms -- the completion perf gate
    /// (docs/completion-v2-design.md M0). None = report only.
    pub budget_ms: Option<f64>,

    /// For `rock sim`: list every `@sim` wire name in the project,
    /// one per line, then exit. Skips the runner. Used by
    /// `just anvil-sim` to avoid hardcoding the sim array.
    pub list: bool,

    pub app_args: Vec<String>,

    /// Launch the game's luajit with `-jv` (verbose trace log to stderr).
    /// Equivalent to setting `STONE_JV=1`; applies to any command that
    /// runs the game (`stone`, `frame`, `run`, `perf`, `sim`).
    pub jv: bool,

    /// For `rock test`: re-run tests on every `.rock` file change in
    /// the given paths. Watches the same paths passed to `rock test`
    /// (defaults to `tests/`). Compilation/test failures don't abort
    /// the loop; Ctrl-C to exit.
    pub watch: bool,
    pub port: u16,

    /// Resolved compiler backend for this invocation (default `skull`; see
    /// `from_matches`). `skull` is bone type-check + skull codegen; `bone` is
    /// type-check only. Override with `--backend` or `ROCK_BACKEND=bone|skull`.
    pub backend: Backend,

    /// Build lint pass: `--no-lints` drops the `Warning`/`Advice` tiers from a
    /// build's output; `--lint-summary` compacts them to one count line.
    /// `Error`-severity findings render and fail the build under both.
    pub no_lints: bool,
    pub lint_summary: bool,

    /// `rock lint --union <lib>`: judge a library's reachability against every
    /// consumer in the repo at once, reporting only what none of them reach.
    pub union: bool,
    /// Where `--union` looks for consumers. Defaults to the lib's parent, which
    /// is right when the lib sits beside its consumers (`stone/`); a nested lib
    /// (`libs/json`) needs the repo root passed explicitly.
    pub consumers: Option<String>,

    /// For `rock test`: force package-aware compilation (the surrounding
    /// stick.toml is compiled as one unit, so @test files see sibling
    /// definitions). Without the flag, `rock test` still auto-detects a
    /// surrounding package; set `ROCK_TEST_NO_PACKAGE=1` to fall back to
    /// the legacy single-file path.
    pub package: bool,

}

impl Args {
    pub fn parse() -> Self {
        Self::from_matches(Self::command().get_matches())
    }

    /// Construct the clap `Command` for the rock CLI. Public so
    /// `rock completions` (`dispatcher.rs`) can hand it to
    /// `clap_complete::generate`.
    pub fn command() -> clap::Command {
        use clap::{Arg, ArgAction};
        clap::Command::new("rock")
            .version(env!("CARGO_PKG_VERSION"))
            .about("Rock Compiler")
            .arg(
                Arg::new("command")
                    .required(true)
                    .value_parser(clap::value_parser!(CliCommand)),
            )
            .arg(Arg::new("files").num_args(0..))
            .arg(Arg::new("output").long("output").alias("out"))
            .arg(Arg::new("profile").long("profile"))
            .arg(Arg::new("no-stdlib").long("no-stdlib").action(ArgAction::SetTrue)
                .help("Compile without the standard library"))
            .arg(Arg::new("stdout").long("stdout").action(ArgAction::SetTrue)
                .help("Print formatted output to stdout instead of updating files - for fmt command"))
            .arg(Arg::new("check").long("check").action(ArgAction::SetTrue)
                .help("For `rock fmt`: check if files are already formatted (exit 1 if not). For `rock completion-sweep`: enforce the completion response invariants at every position (exit 1 on violation)."))
            .arg(Arg::new("iters").long("iters").default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Number of iterations for compilation (used for profiling)"))
            .arg(Arg::new("warmup").long("warmup").default_value("3")
                .value_parser(clap::value_parser!(usize))
                .help("Number of untimed warmup runs before the timed iters (used by `backend-stats`). Default 3. Set to 0 to disable."))
            .arg(Arg::new("verbose").long("verbose").action(ArgAction::SetTrue)
                .help("Enable verbose debug output"))
            .arg(Arg::new("quiet").long("quiet").action(ArgAction::SetTrue)
                .help("Disable timing output (Phase Timing, IO Breakdown)"))
            .arg(Arg::new("frames").short('n').long("frames").default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Number of frames to run for frame command"))
            .arg(Arg::new("seconds").long("seconds")
                .value_parser(clap::value_parser!(f64))
                .help("Run `rock perf` for this many wall-clock seconds instead of a fixed frame count. Overrides --frames when set."))
            .arg(Arg::new("warmup-seconds").long("warmup-seconds").default_value("2.0")
                .value_parser(clap::value_parser!(f64))
                .help("Skip the first N wall-clock seconds from perf statistics (warmup). Applies to `rock perf` only. Default 2.0s. Set to 0 to disable."))
            .arg(Arg::new("gc-window").long("gc-window").default_value("0")
                .value_parser(clap::value_parser!(usize))
                .help("`rock perf` only: run a GC-stopped window of N frames (opening at --gc-window-start) and report TRUE bytes/frame allocated in the summary (memory.true_alloc_kb_per_frame). The trustworthy allocation oracle; the per-section alloc_kb is duration-correlated. 0 = disabled."))
            .arg(Arg::new("gc-window-start").long("gc-window-start").default_value("100")
                .value_parser(clap::value_parser!(usize))
                .help("`rock perf` only: frame the --gc-window opens on (default 100). Push it past warmup so the window lands on a warm JIT; `rock perf` sizes its frame budget to cover start + window."))
            .arg(Arg::new("perf-format").long("format").default_value("pretty")
                .help("Output format for `rock perf` summary: \"pretty\" (default) or \"json\"."))
            .arg(Arg::new("type-validation").long("type-validation").action(ArgAction::SetTrue)
                .help("Enable type validation warnings for unresolved types after inference"))
            .arg(Arg::new("headless").long("headless").action(ArgAction::SetTrue)
                .help("Run frame command in headless mode (no display required)"))
            .arg(Arg::new("test-threads").long("test-threads").value_name("NUM")
                .value_parser(clap::value_parser!(usize))
                .help("Number of threads to use for running tests (default: number of CPU cores, use 1 for sequential)"))
            .arg(Arg::new("lib").long("lib")
                .help("Load a project as a library context when running a Rock file. Usage: rock run --lib game/ test.rock"))
            .arg(Arg::new("clean").long("clean").action(ArgAction::SetTrue)
                .help("Force a clean build by clearing the cache before compilation"))
            .arg(Arg::new("release").long("release").action(ArgAction::SetTrue)
                .help("Compile in release mode — strips debug-time runtime checks. Can also be set via ROCK_RELEASE=1."))
            .arg(Arg::new("trace").long("trace").action(ArgAction::SetTrue)
                .help("Enable trace file generation (trace.folded and trace.perfetto) for profiling"))
            .arg(Arg::new("timeout").long("timeout").default_value("60")
                .value_parser(clap::value_parser!(u64))
                .help("Timeout for run command in seconds (default: 60)"))
            .arg(Arg::new("no-timeout").long("no-timeout").action(ArgAction::SetTrue)
                .help("Disable timeout for run command (allow infinite execution)"))
            .arg(Arg::new("dump-state").long("dump-state").action(ArgAction::SetTrue)
                .help("Dump all global state as JSON after the last frame (for AI/human diagnostics)"))
            .arg(Arg::new("update-snapshots").long("update-snapshots").action(ArgAction::SetTrue)
                .help("For `rock sim`: rewrite snapshot baselines instead of diffing."))
            .arg(Arg::new("jobs").long("jobs").short('j').value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock sim`: run up to N sim children concurrently (default 4, 1 = serial)."))
            .arg(Arg::new("record").long("record").value_name("FILE")
                .help("For `rock frame`: record per-tick input + dt to FILE (a replay log). Replay it with --replay."))
            .arg(Arg::new("replay").long("replay").value_name("FILE")
                .help("For `rock frame`: drive the run from replay log FILE instead of live input/wall-clock."))
            .arg(Arg::new("checkpoint-interval").long("checkpoint-interval").value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock frame --record`: hash game state every N ticks for desync detection (default 60; 0 disables)."))
            .arg(Arg::new("no-record").long("no-record").action(ArgAction::SetTrue)
                .help("For `rock stone`: disable the always-on flight recorder for this session (same as STONE_NO_FLIGHT_RECORDER=1)."))
            .arg(Arg::new("stride").long("stride").default_value("10")
                .value_parser(clap::value_parser!(usize))
                .help("For `rock completion-sweep`: keep every Nth probe position (1 = exhaustive)."))
            .arg(Arg::new("budget-ms").long("budget-ms")
                .value_parser(clap::value_parser!(f64))
                .help("For `rock iter-completion`: fail when the worst per-keystroke median exceeds this many ms (the completion perf gate)."))
            .arg(Arg::new("list").long("list").action(ArgAction::SetTrue)
                .help("For `rock sim`: list every `@sim` wire name in the project, one per line, then exit."))
            .arg(Arg::new("watch").long("watch").action(ArgAction::SetTrue)
                .help("For `rock test`: re-run tests on source changes. For `rock web`: serve the build and reload connected browsers after successful changes."))
            .arg(Arg::new("port").long("port").default_value("8000")
                .value_parser(clap::value_parser!(u16))
                .help("For `rock web --watch`: loopback HTTP port. Use 0 to choose an available port."))
            .arg(Arg::new("package").long("package").action(ArgAction::SetTrue)
                .help("For `rock test`: compile the surrounding stick.toml package (and its deps) so @test files can call sibling-file symbols. Auto-detected when the path lives inside a package; pass --package=false (or env ROCK_TEST_NO_PACKAGE=1) to force single-file mode."))
            .arg(Arg::new("no-lints").long("no-lints").action(ArgAction::SetTrue)
                .help("Drop lint warnings/advice from build output. Error-severity lints still run and still fail the build."))
            .arg(Arg::new("lint-summary").long("lint-summary").action(ArgAction::SetTrue)
                .help("Report build lint warnings/advice as a one-line count instead of rendering each. Error-severity lints always render in full."))
            .arg(Arg::new("union").long("union").action(ArgAction::SetTrue)
                .help("For `rock lint <lib>`: judge a library's reachability against every consumer in the repo at once, reporting only functions that NONE of them reach. The only sound way to run unused_fn on a lib."))
            .arg(Arg::new("consumers").long("consumers")
                .help("For `rock lint --union`: directory to search for consumers (default: the lib's parent). Pass the repo root for a nested lib like `libs/json`."))
            .arg(Arg::new("backend").long("backend")
                .value_parser(clap::value_parser!(Backend))
                .help("Compiler backend: `skull` (default -- bone type-check + skull codegen, runs Lua), `bone` (type-check only, no codegen), `js` (test only -- JS-subset @test execution under node, SKIP(reason) outside the subset), or `glsl` (test only -- GLSL value-test execution on the GPU, both 330core+300es profiles). Can also be set via ROCK_BACKEND=bone|skull|js|glsl."))
            .arg(Arg::new("jv").long("jv").action(ArgAction::SetTrue)
                .help("Run the game's luajit with -jv (verbose JIT trace log to stderr). Same as STONE_JV=1; works for stone/frame/run/perf/sim."))
            .arg(Arg::new("app_args").last(true).num_args(0..)
                .help("Arguments to pass through to the Rock app (everything after --)"))
    }

    fn from_matches(m: clap::ArgMatches) -> Self {
        let many_str = |name: &str| -> Vec<String> {
            m.get_many::<String>(name).map(|vs| vs.cloned().collect()).unwrap_or_default()
        };
        let command = *m.get_one::<CliCommand>("command").expect("command required");
        Args {
            command,
            files: many_str("files"),
            output: m.get_one::<String>("output").cloned(),
            profile: m.get_one::<String>("profile").cloned(),
            no_stdlib: m.get_flag("no-stdlib"),
            stdout: m.get_flag("stdout"),
            check: m.get_flag("check"),
            iters: *m.get_one::<usize>("iters").unwrap(),
            warmup: *m.get_one::<usize>("warmup").unwrap(),
            verbose: m.get_flag("verbose"),
            quiet: m.get_flag("quiet"),
            frames: *m.get_one::<usize>("frames").unwrap(),
            seconds: m.get_one::<f64>("seconds").copied(),
            warmup_seconds: *m.get_one::<f64>("warmup-seconds").unwrap(),
            gc_window: *m.get_one::<usize>("gc-window").unwrap(),
            gc_window_start: *m.get_one::<usize>("gc-window-start").unwrap(),
            perf_format: m.get_one::<String>("perf-format").cloned().unwrap_or_else(|| "pretty".to_string()),
            type_validation: m.get_flag("type-validation"),
            headless: m.get_flag("headless"),
            test_threads: m.get_one::<usize>("test-threads").copied(),
            lib: m.get_one::<String>("lib").cloned(),
            clean: m.get_flag("clean"),
            release: m.get_flag("release"),
            trace: m.get_flag("trace"),
            timeout: *m.get_one::<u64>("timeout").unwrap(),
            no_timeout: m.get_flag("no-timeout"),
            dump_state: m.get_flag("dump-state"),
            update_snapshots: m.get_flag("update-snapshots"),
            jobs: m.get_one::<usize>("jobs").copied(),
            record: m.get_one::<String>("record").cloned(),
            replay: m.get_one::<String>("replay").cloned(),
            checkpoint_interval: m.get_one::<usize>("checkpoint-interval").copied(),
            no_record: m.get_flag("no-record"),
            stride: *m.get_one::<usize>("stride").unwrap(),
            budget_ms: m.get_one::<f64>("budget-ms").copied(),
            list: m.get_flag("list"),
            app_args: many_str("app_args"),
            jv: m.get_flag("jv"),
            watch: m.get_flag("watch"),
            port: *m.get_one::<u16>("port").unwrap(),
            package: m.get_flag("package"),
            no_lints: m.get_flag("no-lints"),
            lint_summary: m.get_flag("lint-summary"),
            union: m.get_flag("union"),
            consumers: m.get_one::<String>("consumers").cloned(),
            // Backend selection. An explicit `--backend` flag wins; otherwise an
            // explicit `ROCK_BACKEND` env var; otherwise the implicit default is
            // `skull`. An explicit `--backend skull`/`bone` on a command that
            // backend can't serve still rejects (the capability gate's
            // diagnostic). The backend-agnostic kept commands (fmt, completions,
            // stone-test) are dispatched before the backend switch, so the
            // default never rejects them.
            backend: {
                let explicit = m.get_one::<Backend>("backend").copied().or_else(|| {
                    match std::env::var("ROCK_BACKEND").ok().as_deref() {
                        Some("bone") => Some(Backend::Bone),
                        Some("skull") => Some(Backend::Skull),
                        Some("js") => Some(Backend::Js),
                        Some("glsl") => Some(Backend::Glsl),
                        _ => None,
                    }
                });
                resolve_backend(explicit, command)
            },
        }
    }
}


#[cfg(test)]
mod arg_tests {
    use super::Args;

    #[test]
    fn no_record_flag_parses_and_defaults_off() {
        let parse = |argv: &[&str]| {
            Args::from_matches(Args::command().try_get_matches_from(argv).expect("parse"))
        };
        assert!(parse(&["rock", "stone", "bitgun", "--no-record"]).no_record);
        assert!(!parse(&["rock", "stone", "bitgun"]).no_record);
    }
}

#[cfg(test)]
mod caps_tests {
    use super::{Backend, CliCommand};

    #[test]
    fn bone_serves_typecheck_subset_only() {
        use CliCommand::*;
        for cmd in [Test, Compile, CompileSingle, TestCompile, Run, Watch] {
            assert!(Backend::Bone.serves(cmd), "bone should serve {:?}", cmd);
        }
        // Codegen/runtime/game commands are rejected.
        for cmd in [Frame, Perf, Stone, Sim] {
            assert!(!Backend::Bone.serves(cmd), "bone should reject {:?}", cmd);
        }
    }

    #[test]
    fn skull_serves_codegen_subset_only() {
        use CliCommand::*;
        // skull is a codegen backend: compile/run/test + the game commands
        // (stone/frame/perf/sim) that route their compile through its GameCompiler.
        for cmd in [Test, Compile, CompileSingle, TestCompile, Run, Watch, Stone, Frame, Perf, Sim] {
            assert!(Backend::Skull.serves(cmd), "skull should serve {:?}", cmd);
        }
        // Backend-agnostic kept commands (dispatched before the backend switch)
        // are not served by skull's per-backend dispatch.
        for cmd in [Fmt, Completions, StoneTest] {
            assert!(!Backend::Skull.serves(cmd), "skull should reject {:?}", cmd);
        }
    }

    #[test]
    fn js_serves_test_and_run() {
        use CliCommand::*;
        assert!(Backend::Js.serves(Test), "js serves test");
        assert!(Backend::Js.serves(Run), "js serves run (M1: node program emission)");
        // No game/engine surface: everything past headless program execution
        // stays rejected.
        for cmd in [Compile, CompileSingle, TestCompile, Watch, Stone, Frame, Perf, Sim] {
            assert!(!Backend::Js.serves(cmd), "js should reject {:?}", cmd);
        }
        let msg = Backend::Js.reject_reason(CliCommand::Stone).expect("js rejects stone");
        assert!(msg.contains("`js` backend cannot serve `stone`"), "{msg}");
        assert!(msg.contains("run"), "supported list names run: {msg}");
    }

    #[test]
    fn glsl_serves_test_only() {
        use CliCommand::*;
        assert!(Backend::Glsl.serves(Test), "glsl serves test");
        for cmd in [Compile, CompileSingle, TestCompile, Run, Watch, Stone, Frame, Perf, Sim] {
            assert!(!Backend::Glsl.serves(cmd), "glsl should reject {:?}", cmd);
        }
        let msg = Backend::Glsl.reject_reason(CliCommand::Run).expect("glsl rejects run");
        assert!(msg.contains("`glsl` backend cannot serve `run`"), "{msg}");
        assert!(msg.contains("test"), "supported list names test: {msg}");
    }

    #[test]
    fn reject_reason_message_is_generated_from_caps_table() {
        // The "Supported commands" list must be generated from
        // supported_commands, not hand-typed prose that can drift.
        let msg = Backend::Bone
            .reject_reason(CliCommand::Frame)
            .expect("bone rejects frame");
        assert!(msg.contains("`bone` backend cannot serve `frame`"), "{msg}");
        assert!(msg.contains("typechecker"), "{msg}");
        for cmd in Backend::Bone.supported_commands() {
            assert!(
                msg.contains(cmd.cli_name()),
                "reject message should list supported command {}: {msg}",
                cmd.cli_name()
            );
        }
    }

    #[test]
    fn cli_names_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for cmd in <CliCommand as clap::ValueEnum>::value_variants().iter().copied() {
            let name = cmd.cli_name();
            assert!(!name.is_empty(), "{:?} has empty cli_name", cmd);
            assert!(seen.insert(name), "duplicate cli_name {name}");
        }
    }

    use super::resolve_backend;

    #[test]
    fn default_backend_is_skull_for_served_commands() {
        // With no explicit choice, every command skull serves defaults to skull.
        for cmd in <CliCommand as clap::ValueEnum>::value_variants().iter().copied() {
            if Backend::Skull.serves(cmd) {
                assert_eq!(
                    resolve_backend(None, cmd),
                    Backend::Skull,
                    "{cmd:?} is served by skull -> default should be skull"
                );
            }
        }
    }

    #[test]
    fn default_backend_is_skull_for_backend_agnostic_commands() {
        use CliCommand::*;
        // The kept backend-agnostic commands (fmt, completions, stone-test) are
        // dispatched before the backend switch, so the resolved default is still
        // skull -- it just never reaches the per-backend dispatch for them.
        for cmd in [Fmt, Completions, StoneTest] {
            assert!(!Backend::Skull.serves(cmd));
            assert_eq!(resolve_backend(None, cmd), Backend::Skull);
        }
    }

    #[test]
    fn explicit_backend_is_honored_even_when_unserved() {
        // An explicit choice is verbatim; the capability gate (not this
        // resolver) is what later rejects an unserved command.
        assert_eq!(resolve_backend(Some(Backend::Bone), CliCommand::Frame), Backend::Bone);
        assert_eq!(resolve_backend(Some(Backend::Skull), CliCommand::Test), Backend::Skull);
    }
}
