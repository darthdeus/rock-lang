//! skull backend integration for `rock-cli` (`--backend skull`, the default).
//!
//! skull is the code generator over `bone`'s HIR (see the `skull` crate).
//! Where `--backend bone` stops at type-checking, `--backend skull` emits Lua
//! from bone's HIR and, for `test`/`run`, executes it with LuaJIT.

use crate::Result;
use crate::bone_backend::{categorize, short_path};
use crate::registry::{ProgramKind, RuntimeServices};
use bone_core::prelude::Instant;
use colored::Colorize;
use rock_core::CompilerError;
use rock_core::SourceFiles;
use rock_core::source_file_from_path_and_contents;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Parse `src`, type-check it through bone WITH the stdlib injected (so
/// operators/`print`/`assert` resolve, exactly like `--backend bone`), then emit
/// Lua for the user module via skull. The stdlib module isn't emitted — its
/// symbols are referenced by name (resolved at runtime by a real stdlib
/// prelude, or simply not called under the load-only validity gate).
/// Returns the emitted Lua and the emitter's own `emits_main`, which decides
/// whether the caller appends the entry call. Not recovered from the text: the
/// emitted spelling of a definition is the emitter's business.
fn skull_emit(file: &str, src: &str) -> std::result::Result<(String, bool), Vec<CompilerError>> {
    let (lua, _, emits_main) = skull_emit_full(file, src).map_err(EmitFullErr::into_errs)?;
    Ok((lua, emits_main))
}

/// A [`skull_emit_full`] failure. `Discovery` (a malformed `@test` annotation on
/// a file bone ACCEPTED) is split from `Compile` so the expect-compile-failure
/// escape in [`skull_build`] never absorbs it -- `@test(expect_failure = true,
/// returns = 42)` must reject, not count as an expected compile failure.
enum EmitFullErr {
    Compile(Vec<CompilerError>),
    Discovery(CompilerError),
}

impl EmitFullErr {
    fn into_errs(self) -> Vec<CompilerError> {
        match self {
            EmitFullErr::Compile(errs) => errs,
            EmitFullErr::Discovery(e) => vec![e],
        }
    }
}

/// The command line's [`skull::pipeline::CompileHost`]: alias tables from
/// manifests on disk, the invocation's lint policy and compile measurements.
struct CliCompileHost<'a> {
    sources: &'a SourceFiles,
    lint_root: &'a std::path::Path,
    lint_scope: crate::lint_pass::Scope,
    comptime: CliComptime<'a>,
    prepare_timings: Vec<(&'static str, std::time::Duration)>,
}

enum CliComptime<'a> {
    /// Resolve from the entry path's enclosing project once the program
    /// checks (single-file emission walks up, matching the JS single-file
    /// path: the same source must agree across backends).
    FromEntry(&'a str),
    /// Resolved before discovery (a project build).
    Resolved(Option<bone_semantic::inference::ComptimeInputs>),
}

impl skull::pipeline::CompileHost for CliCompileHost<'_> {
    fn aliases(
        &mut self,
        modules: &skull_parser::stdlib::ModuleSet,
    ) -> std::result::Result<bone_core::modules::NsAliases, CompilerError> {
        crate::bone_backend::aliases_for_modules(modules.iter())
    }

    fn phase(&mut self, phase: skull::pipeline::Phase<'_>) {
        use skull::pipeline::Phase;
        match phase {
            Phase::Parsed => crate::compile_budget::phase("parse+stdlib"),
            Phase::Checked(ws) => {
                crate::compile_budget::phase("semantic");
                crate::compile_budget::detail(&ws.phase_timings);
            }
            Phase::Prepared => {
                crate::compile_budget::phase("prepare");
                crate::compile_budget::detail(&self.prepare_timings);
            }
        }
    }

    fn comptime_inputs(
        &mut self,
    ) -> std::result::Result<bone_semantic::inference::ComptimeInputs, CompilerError> {
        match &mut self.comptime {
            CliComptime::FromEntry(file) => {
                crate::comptime::inputs_for_entry_path(std::path::Path::new(file)).map_err(|e| {
                    CompilerError::new_no_location(format!("build var resolution failed: {e:#}"))
                })
            }
            CliComptime::Resolved(inputs) => Ok(inputs.take().unwrap_or_default()),
        }
    }

    fn checked_comptime_inputs(
        &mut self,
        ws: &bone_semantic::inference::Workspace,
    ) -> std::result::Result<bone_semantic::inference::ComptimeInputs, CompilerError> {
        let timer = Instant::now();
        let inputs = self.comptime_inputs()?;
        // Every fingerprint owner looks its text up; one index over the
        // sources instead of a scan per owner.
        let texts: std::collections::HashMap<String, usize> = self
            .sources
            .files
            .iter()
            .enumerate()
            .map(|(index, source)| (source.borrow().file.to_id_path(), index))
            .collect();
        let resolved = crate::comptime::resolve_checked_inputs(ws, inputs, |file| {
            texts
                .get(file)
                .map(|&index| self.sources.files[index].borrow().contents.clone())
        })
        .map_err(|error| {
            CompilerError::new_no_location(format!("build var resolution failed: {error:#}"))
        });
        self.prepare_timings
            .push(("comptime inputs", timer.elapsed()));
        resolved
    }

    fn lint_findings(
        &mut self,
        ws: &bone_semantic::inference::Workspace,
    ) -> Vec<bone_lints::Finding> {
        let timer = Instant::now();
        let found = crate::lint_pass::findings(ws, self.sources, self.lint_root, self.lint_scope);
        self.prepare_timings.push(("lints", timer.elapsed()));
        found
    }

    fn report_soft_findings(&mut self, soft: Vec<bone_lints::Finding>) {
        crate::lint_pass::report_soft(&soft, self.sources, self.lint_root);
    }
}

/// A package test build's [`skull::pipeline::CompileHost`]: the command-line
/// host with test-rooted emission. The lint scope stays whole-project: the
/// sources still include the package's own tests.
struct PackageTestHost<'a> {
    inner: CliCompileHost<'a>,
    test_dir: &'a str,
}

impl skull::pipeline::CompileHost for PackageTestHost<'_> {
    fn aliases(
        &mut self,
        modules: &skull_parser::stdlib::ModuleSet,
    ) -> std::result::Result<bone_core::modules::NsAliases, CompilerError> {
        self.inner.aliases(modules)
    }

    fn comptime_inputs(
        &mut self,
    ) -> std::result::Result<bone_semantic::inference::ComptimeInputs, CompilerError> {
        self.inner.comptime_inputs()
    }

    fn checked_comptime_inputs(
        &mut self,
        ws: &bone_semantic::inference::Workspace,
    ) -> std::result::Result<bone_semantic::inference::ComptimeInputs, CompilerError> {
        self.inner.checked_comptime_inputs(ws)
    }

    fn lint_findings(
        &mut self,
        ws: &bone_semantic::inference::Workspace,
    ) -> Vec<bone_lints::Finding> {
        self.inner.lint_findings(ws)
    }

    fn report_soft_findings(&mut self, soft: Vec<bone_lints::Finding>) {
        self.inner.report_soft_findings(soft);
    }

    fn emit(
        &mut self,
        ws: &bone_semantic::inference::Workspace,
        entry: bone_semantic::prelude::ModuleId,
        _target: skull::pipeline::EmitTarget,
    ) -> std::result::Result<skull::mir_emit::MirEmit, CompilerError> {
        let test_fns = collect_package_test_fns(ws, self.test_dir)?;
        let roots = test_fns.iter().map(|(func, _)| *func).collect::<Vec<_>>();
        skull::mir_emit::FullEmit::package_test(&roots)
            .run(ws, entry)
            .map(|(emit, _cache)| emit)
    }
}

/// Emit the workspace and also discover the user module's `@test` functions
/// (for execution). Returns `(lua, tests, emits_main)`. `file` is stamped as
/// the source-file id so spans (and `__SITE__`) carry the real path.
fn skull_emit_full(
    file: &str,
    src: &str,
) -> std::result::Result<(String, Vec<skull::SkullTest>, bool), EmitFullErr> {
    let (sources, lint_root) = crate::lint_pass::single_file_scope(file, src);
    let mut host = CliCompileHost {
        sources: &sources,
        lint_root: &lint_root,
        lint_scope: crate::lint_pass::Scope::WholeProject,
        comptime: CliComptime::FromEntry(file),
        prepare_timings: Vec::new(),
    };
    let skull::pipeline::Compiled {
        ws,
        entry: user_mid,
        emission: emit,
    } = skull::pipeline::compile_source(
        file,
        src,
        skull::pipeline::EmitTarget::SINGLE_SOURCE,
        &mut host,
    )
    .map_err(|failure| EmitFullErr::Compile(failure.errors))?;
    crate::compile_budget::emission(&emit);
    let emits_main = emit.emits_main;
    let lua = emit.lua;
    let tests = skull::module_tests(&ws, user_mid).map_err(EmitFullErr::Discovery)?;
    Ok((lua, tests, emits_main))
}

/// Per-phase wall-clock for one skull run over a single source. `parse` and
/// `semantic` mirror `BonePhaseTimes` (skull shares bone's front half: same
/// `parse_rock` + `semantic_pass`), so they're directly comparable; `emit` is
/// the codegen skull adds on top (`skull::emit_workspace`). Consumed by
/// `backend-stats` for the bone-vs-skull comparison.
#[derive(Default, Clone)]
pub(crate) struct SkullPhaseTimes {
    pub parse: Duration,
    pub semantic: Duration,
    /// Comptime/build-variable preparation after semantic checking and before
    /// code generation. This is setup work, not semantic or emit self-time.
    pub workspace_setup: Duration,
    pub emit: Duration,
    /// The last front-end/setup/emit failure, retained so measurement callers
    /// cannot report a contradictory successful-looking zero population.
    pub failure: Option<CompilerError>,
    pub setup_failed: bool,
    pub emit_failed: bool,
    /// skull's emit broken into coarse stages (lower / cleanup / emit-lua /
    /// scaffold+misc), from `MirEmit::phases`. Empty if bone rejected the
    /// workspace (emit never ran). Summed element-wise across timed runs.
    pub emit_phases: Vec<(&'static str, Duration)>,
    /// skull's ABI-plan memo counters, from `MirEmit::abi_plan_stats`.
    pub abi_plan_stats: skull_mir::lower::AbiPlanMemoStats,
    /// Shared-arena traffic split at the setup/fanout boundary, from
    /// `MirEmit::arena_traffic`.
    pub arena_traffic: skull::mir_emit::ArenaTraffic,
    /// Body-oriented lowering and transform attribution from `MirEmit`.
    pub attribution: skull::mir_emit::EmitAttribution,
    /// Setup-lane populations and observed execution overlap from `MirEmit`.
    pub setup_lanes: skull::mir_emit::SetupLaneAttribution,
    /// Graph operation census from `MirEmit`.
    pub graph_census: skull::instance_graph::RunStats,
    /// Concrete-plan ownership census from `MirEmit`.
    pub concrete_stats: skull::concrete_body::ConcreteBuildStats,
}

/// Whole-project skull timing: parse all user files, type-check them together
/// with the stdlib as ONE workspace (bone's front half), then emit Lua for the
/// entry module. Returns whether Lua was emitted (the skull "pass" condition,
/// same gate as `skull_emit`) and the per-phase timing. `parse`/`semantic`
/// mirror `bone_workspace_timed`; `emit` is skull's codegen on top. Used by
/// `backend-stats` — a per-file loop would never reach emit because bone rejects
/// any file that references a sibling (so emit would always read ~0).
pub(crate) fn skull_workspace_timed(
    sources: &SourceFiles,
    project_path: &std::path::Path,
) -> (bool, SkullPhaseTimes) {
    let mut times = SkullPhaseTimes::default();
    let (user_modules, entry_file, parse) = match crate::bone_backend::parse_user_modules(sources) {
        Ok(v) => v,
        Err(mut errors) => {
            times.failure = errors.drain(..).next();
            return (false, times);
        }
    };
    times.parse = parse;

    let t = Instant::now();
    // Timed path: assemble via the shared helper but keep the `semantic_pass`
    // call explicit here (the verdict is a bool, not a gate, and the timer must
    // bracket exactly this call).
    let mut modules = match skull::pipeline::with_stdlib(user_modules) {
        Ok(modules) => modules,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    let aliases = match crate::bone_backend::aliases_for_modules(modules.iter()) {
        Ok(aliases) => aliases,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    let mut ws = match crate::bone_backend::semantic_pass_with_aliases(&mut modules, &aliases) {
        Ok(ws) => ws,
        Err(error) => {
            times.semantic = t.elapsed();
            times.failure = Some(error);
            return (false, times);
        }
    };
    times.semantic = t.elapsed();
    let body_errors = crate::bone_backend::body_errors(&ws);
    if let Some(error) = body_errors.into_iter().next() {
        times.failure = Some(error);
        return (false, times);
    }

    // Production emission stamps declared [build.vars] once after the checked
    // workspace exists. Keep that preparation visible as its own timed lane;
    // the emit stopwatch remains codegen-only.
    let t = Instant::now();
    // Source discovery loads dependencies first; the parser's entry fallback
    // must not choose the package that owns invocation build settings.
    if let Err(error) = crate::comptime::stamp_workspace(&mut ws, project_path) {
        times.workspace_setup = t.elapsed();
        times.setup_failed = true;
        times.failure = Some(CompilerError::new_no_location(format!(
            "build var resolution failed: {error:#}"
        )));
        return (false, times);
    }
    times.workspace_setup = t.elapsed();

    let entry_mid = crate::bone_backend::entry_module_id(&ws, entry_file);

    let t = Instant::now();
    let emit = skull::mir_emit::emit_workspace_mir(&ws, entry_mid);
    times.emit = t.elapsed();
    let emit_ok = match emit {
        Ok(e) => {
            times.emit_phases = e.phases;
            times.abi_plan_stats = e.abi_plan_stats;
            times.arena_traffic = e.arena_traffic;
            times.attribution = e.attribution;
            times.setup_lanes = e.setup_lanes;
            times.graph_census = e.graph_census;
            times.concrete_stats = e.concrete_stats;
            true
        }
        Err(e) => {
            times.failure = Some(e);
            times.emit_failed = true;
            false
        }
    };
    (emit_ok, times)
}

/// Assemble a whole *project* directory (a dir with a `stick.toml`) into bone
/// modules: every project + dependency `.rock` file (via the normal compiler's
/// dependency resolver) plus the bone stdlib. Returns the parsed modules and the
/// entry-module file path (`main.rock`, else the first project file); resolve it
/// to a `ModuleId` via `bone_backend::entry_module_id` once the workspace exists.
///
/// Unlike the single-file path, the embedded compiler stdlib is NOT pulled from
/// `discover_sources` — bone supplies its own, shared ([`skull::pipeline::with_stdlib`]).
pub(crate) fn skull_assemble_project(
    sources: &SourceFiles,
) -> std::result::Result<(skull_parser::stdlib::ModuleSet, ustr::Ustr), Vec<CompilerError>> {
    let (user_modules, entry_file, _parse) = crate::bone_backend::parse_user_modules(sources)?;
    let modules = skull::pipeline::with_stdlib(user_modules).map_err(|e| vec![e])?;
    Ok((modules, entry_file))
}

/// A Lua harness appended after the emitted module: it runs each `@test` fn with
/// its `print` AND `io.write` captured (both restored before the markers are
/// written), and writes one `__SKULL__<TAB>PASS|FAIL<TAB>detail` line and one
/// wall-time `TIME` line per test. Before each test it writes a
/// `ROCK_TEST_START` marker with this run's `token` to stderr; the token and
/// ordinal also trail the TIME line so the host can count completed tests as
/// progress. `ROCK_TEST_SHARD_TESTS` (1-based indices)
/// restricts a process to one shard's tests; start ordinals count within it.
/// Capturing `io.write` too is required: tests that emit via `io.write` (raw,
/// no trailing newline) would otherwise bypass the buffer and read as empty.
fn build_test_harness(tests: &[skull::SkullTest], token: &str) -> String {
    fn lua_str(s: &str) -> String {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                _ => o.push(c),
            }
        }
        o.push('"');
        o
    }

    let mut h = format!(
        "\nlocal __skull_token = {}\nlocal __skull_T = {{\n",
        lua_str(token)
    );
    for t in tests {
        let out = match &t.output {
            Some(s) => lua_str(s),
            None => "nil".to_string(),
        };
        // `f` is the global test fn by its EMITTED symbol; `out` is the expected
        // stdout (or nil). `name` labels the FAIL line and stays the logical
        // DefKey, which is what a human reading the report wants (a package unit
        // runs many tests together, and two modules may declare the same name).
        // `val` marks a `@test(returns = ...)` test: the harness emits a VALUE
        // line carrying the serialized return value; the HOST compares it.
        h.push_str(&format!(
            "  {{ f = {}, out = {}, name = {}, val = {} }},\n",
            t.symbol,
            out,
            lua_str(&t.name),
            t.returns.is_some()
        ));
    }
    h.push_str("}\n");
    h.push_str(
        r#"local function __esc(s) return (s:gsub("\\", "\\\\"):gsub("\n", "\\n"):gsub("\t", "\\t")) end
-- os.clock is CPU time on POSIX and misses sleeps and waits, so tests are timed
-- with the monotonic clock; MSVC's clock() is already wall time.
local __skull_now = function() return os.clock() * 1e3 end
do
  local ffi = require("ffi")
  if ffi.os ~= "Windows" then
    ffi.cdef[[struct __skull_timespec { long tv_sec; long tv_nsec; };
      int __skull_clock_gettime(int, struct __skull_timespec *) __asm__("clock_gettime");]]
    local ts = ffi.new("struct __skull_timespec")
    local monotonic = ffi.os == "OSX" and 6 or 1
    __skull_now = function()
      assert(ffi.C.__skull_clock_gettime(monotonic, ts) == 0, "clock_gettime failed")
      return tonumber(ts.tv_sec) * 1e3 + tonumber(ts.tv_nsec) / 1e6
    end
  end
end
io.stdout:setvbuf("no")
local __skull_gc_mb = tonumber(os.getenv("ROCK_TEST_GC_COLLECT_MB") or "512")
assert(__skull_gc_mb and __skull_gc_mb >= 0 and __skull_gc_mb < math.huge,
  "ROCK_TEST_GC_COLLECT_MB must be a finite non-negative number (0 disables)")
local __skull_gc_limit_kb = __skull_gc_mb * 1024
local __skull_run = {}
local __skull_shard = os.getenv("ROCK_TEST_SHARD_TESTS")
if __skull_shard then
  for i in __skull_shard:gmatch("%d+") do __skull_run[#__skull_run + 1] = __skull_T[tonumber(i)] end
else
  __skull_run = __skull_T
end
for n, t in ipairs(__skull_run) do
  io.stderr:write("\nROCK_TEST_START\t", __skull_token, "\t", n - 1, "\n")
  io.stderr:flush()
  local buf = {}
  local rp = print
  local rw = io.write
  -- Model real stdout byte-faithfully: `print` joins args with TAB and adds a
  -- trailing newline; `io.write` appends raw fragments. One accumulator; only
  -- TRAILING whitespace stripped (eats print's `\n` + trailing-space separators
  -- like `{i} ` loops). Leading whitespace is significant so a right-aligned /
  -- space-padded `{x:5}` -> `   42` is testable via @test(output=..).
  print = function(...)
    local p = {}
    for i = 1, select('#', ...) do p[i] = tostring((select(i, ...))) end
    buf[#buf + 1] = table.concat(p, "\t") .. "\n"
  end
  io.write = function(...)
    for i = 1, select('#', ...) do buf[#buf + 1] = tostring((select(i, ...))) end
  end
  local start = __skull_now()
  local ok, ret = pcall(t.f)
  local ms = __skull_now() - start
  print = rp
  io.write = rw
  local got = table.concat(buf, ""):gsub("%s+$", "")
  local result = "fail"
  if not ok then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": error: ", __esc(tostring(ret)), "\n")
  elseif t.out ~= nil and got ~= t.out then
    io.write("\n__SKULL__\tFAIL\t", t.name, ": output mismatch: expected <", __esc(t.out), "> got <", __esc(got), ">\n")
  elseif t.val then
    result = "value"
    -- %.17g round-trips any f64; booleans via tostring. The host does the
    -- comparison (test_protocol::compare_value), never this harness.
    local s
    if type(ret) == "number" then s = string.format("%.17g", ret) else s = tostring(ret) end
    io.write("\n__SKULL__\tVALUE\t", t.name, "\t", s, "\n")
  else
    result = "pass"
    io.write("\n__SKULL__\tPASS\t\n")
  end
  io.write("\n__SKULL__\tTIME\t", t.name, "\t", string.format("%.3f", ms), "\t", result,
    "\t", __skull_token, "\t", n - 1, "\n")
  -- Reclaim floating test garbage without changing the collector's pacing.
  if __skull_gc_limit_kb > 0 and collectgarbage("count") > __skull_gc_limit_kb then
    collectgarbage("collect")
  end
end
"#,
    );
    h
}

/// Render single-file errors against `src`. `skull_emit_full` parses with the
/// real `file` path as the span file id, so the source must be registered under
/// that same id -- registering under a placeholder makes every span lookup fail
/// ("Unable to fetch source") and silently drops the snippet + labels.
pub(crate) fn render_file_errs(file: &str, src: &str, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles::empty();
    sf.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    crate::bone_backend::render_errs(&sf, errs)
}

/// `map_err` for the `skull::user_module_id` lookup — the same message at every
/// call site.
fn no_user_module(e: CompilerError) -> anyhow::Error {
    anyhow::anyhow!("no user module: {}", e.message)
}

/// A skull failure: a short bucketed reason + the full rendered detail.
#[derive(Debug)]
struct SkullFailure {
    reason: String,
    rendered: String,
}

impl From<crate::registry::RuntimeFailure> for SkullFailure {
    fn from(error: crate::registry::RuntimeFailure) -> Self {
        Self {
            reason: format!("{}: {}", error.reason, error.rendered),
            rendered: error.rendered,
        }
    }
}

/// Outcome of building + running one file. `Pass` means it emitted valid Lua
/// AND every `@test` passed (or it had none). `TestFail` means it emitted valid
/// Lua but some `@test` produced the wrong output or errored — these still count
/// toward "emit valid Lua" but not toward execution. Both variants carry the
/// number of individual `@test` functions involved so the runner can report
/// per-`@test` totals, not just per-file ones.
#[derive(Debug)]
enum SkullOutcome {
    /// All `@test`s passed (or the file had none). `tests` = number that ran
    /// green (0 for a load-only file with no `@test`s).
    Pass {
        tests: usize,
        skipped: usize,
        timings: Vec<TestTiming>,
    },
    /// Some `@test`s failed. `passed` ran green; `fails` holds the rest's detail.
    /// Abnormal shard stops also carry a concise diagnostic for the default CLI
    /// summary, independent of `ROCK_TEST_VERBOSE`.
    TestFail {
        passed: usize,
        fails: Vec<String>,
        skipped: usize,
        timings: Vec<TestTiming>,
        diagnostics: Vec<String>,
    },
}

/// Wall time of one `@test` that finished, for the slowest-tests report and
/// the `ROCK_TEST_TIMINGS` file.
#[derive(Debug, Clone)]
struct TestTiming {
    name: String,
    ms: f64,
    status: &'static str,
}

fn child_exit_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                libc::SIGABRT => "SIGABRT",
                libc::SIGBUS => "SIGBUS",
                libc::SIGFPE => "SIGFPE",
                libc::SIGILL => "SIGILL",
                libc::SIGINT => "SIGINT",
                libc::SIGKILL => "SIGKILL",
                libc::SIGPIPE => "SIGPIPE",
                libc::SIGSEGV => "SIGSEGV",
                libc::SIGTERM => "SIGTERM",
                _ => "unknown signal",
            };
            return format!("signal {signal} ({name})");
        }
    }
    "unknown exit status".to_string()
}

fn stderr_excerpt(stderr: &[u8]) -> String {
    const MAX_BYTES: usize = 4096;
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "<empty>".to_string();
    }
    if text.len() <= MAX_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - MAX_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier stderr omitted]\n{}", &text[start..])
}

fn timeout_cause(run: &crate::common::ProcessResult, limit: Duration) -> String {
    match run.timeout_phase {
        Some(crate::common::TimeoutPhase::TestCpu) => format!(
            "timed out: child CPU-time budget reached ({:.3}s)",
            limit.as_secs_f64()
        ),
        Some(crate::common::TimeoutPhase::StartupCpu) => format!(
            "timed out: emitted-program loading used the child CPU-time budget ({:.3}s)",
            limit.as_secs_f64()
        ),
        Some(crate::common::TimeoutPhase::TestWall) => format!(
            "timed out: wall-clock backstop reached ({}s)",
            crate::common::test_watchdog_wall_backstop(limit).as_secs_f64()
        ),
        Some(crate::common::TimeoutPhase::StartupWall) => format!(
            "timed out: emitted-program loading reached its wall-clock backstop ({}s)",
            crate::common::test_watchdog_wall_backstop(limit).as_secs_f64()
        ),
        _ => format!(
            "timed out: watchdog stopped the child ({:.3}s configured)",
            limit.as_secs_f64()
        ),
    }
}

/// Run a source string through the full skull pipeline (bone type-check + skull
/// codegen), confirm the emitted Lua loads, and execute its `@test` functions
/// against the real (emitted) stdlib. `Err` = emit/load failure; `Ok` carries
/// the pass/test-fail outcome. `filter` selects which `@test`s run.
fn skull_build(
    file: &str,
    src: &str,
    filter: Option<&crate::test::TestFilter>,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    (crate::registry::RuntimeServices::registered().prepare_unit)(file).map_err(|e| {
        SkullFailure {
            reason: "sound bank preparation failed".to_string(),
            rendered: format!("sound bank preparation failed for {file}: {e:#}"),
        }
    })?;
    // bone type-check + skull emit. Errors here are bone's (most files) or a
    // skull codegen gap; both carry a `CompilerError`.
    let (lua, tests, _emits_main) = match skull_emit_full(file, src) {
        Ok(v) => v,
        Err(EmitFullErr::Compile(errs)) => {
            // A file whose `@test`s ALL expect failure is supposed to fail to
            // compile — a bone rejection here is the expected (passing) outcome.
            // (Mirrors the real rock runner's compile-Err branch.)
            if let Some(tests) = all_tests_expect_compile_failure(file, src) {
                let selected = if let Some(filter) = filter {
                    let mut sources = SourceFiles::empty();
                    sources.add_or_update_file(source_file_from_path_and_contents(
                        file.to_string(),
                        file.to_string(),
                        src.to_string(),
                    ));
                    crate::test::find_test_functions_from_sources(&sources)
                        .iter()
                        .filter(|test| filter.matches(&test.name, file))
                        .count()
                } else {
                    tests
                };
                return Ok(SkullOutcome::Pass {
                    tests: selected,
                    skipped: tests - selected,
                    timings: Vec::new(),
                });
            }
            return Err(SkullFailure {
                reason: categorize(&errs[0].message),
                rendered: render_file_errs(file, src, &errs),
            });
        }
        Err(EmitFullErr::Discovery(e)) => {
            return Err(SkullFailure {
                reason: categorize(&e.message),
                rendered: render_file_errs(file, src, &[e]),
            });
        }
    };

    let host = (crate::registry::RuntimeServices::registered().program_host)(
        crate::registry::TestUnit::File(std::path::Path::new(file)),
    )
    .map_err(SkullFailure::from)?;
    let lua = TestProgram {
        preamble: host.preamble,
        lua,
        setup: host.setup,
    };
    let files = vec![file.to_owned(); tests.len()];
    run_emitted_tests(lua, &tests, &files, TEST_CPU_LIMIT, 1, filter, false)
}

/// `Some(count)` when `file` declares `@test` fns and EVERY one carries
/// `expect_failure`/`@ignore` -- a compile rejection is then the expected
/// (passing) outcome and each counts as a passing test. Shared by the luajit
/// and JS executors (both bone-type-check first, so the rule is identical).
pub(crate) fn all_tests_expect_compile_failure(file: &str, src: &str) -> Option<usize> {
    let mut sf = SourceFiles::empty();
    sf.add_or_update_file(source_file_from_path_and_contents(
        file.to_string(),
        file.to_string(),
        src.to_string(),
    ));
    let test_fns = crate::test::find_test_functions_from_sources(&sf);
    if !test_fns.is_empty() && test_fns.iter().all(|t| t.expect_failure || t.ignored) {
        Some(test_fns.len())
    } else {
        None
    }
}

/// Given emitted workspace Lua and the `@test` fns discovered in it, run them
/// via the harness (or the load-only validity gate when there are none) and
/// classify the outcome. Shared by the per-file path ([`skull_build`]) and the
/// whole-package path ([`skull_build_package`]). `filter` selects the tests
/// that run; `shards` LuaJIT processes split the selected tests and their
/// results merge into one outcome. `release_memory` returns the freed program
/// text to the OS before the tests start, for a host that then only waits.
fn run_emitted_tests(
    lua: impl Into<TestProgram>,
    tests: &[skull::SkullTest],
    files: &[String],
    limit: std::time::Duration,
    shards: usize,
    filter: Option<&crate::test::TestFilter>,
    release_memory: bool,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    let lua = lua.into();
    assert_eq!(tests.len(), files.len());
    // `@test(backends = ...)` without lua: not a test of this backend.
    let (lua_tests, lua_files): (Vec<skull::SkullTest>, Vec<String>) = tests
        .iter()
        .zip(files)
        .filter(|(test, _)| test.runs_on(bone_core::annotations::TestBackend::Lua))
        .map(|(test, file)| (test.clone(), file.clone()))
        .unzip();
    let (tests, files) = (lua_tests.as_slice(), lua_files.as_slice());

    let mut selected_tests = Vec::with_capacity(tests.len());
    let mut selected_files = Vec::with_capacity(files.len());
    for (test, file) in tests.iter().zip(files) {
        if filter.map_or(true, |filter| filter.matches(&test.name, file)) {
            selected_tests.push(test.clone());
            selected_files.push(file.clone());
        }
    }
    let skipped = tests.len() - selected_tests.len();

    // Failure reports quote the program's head; the whole text is not kept
    // while the test process runs (project's is about 170 MB).
    // Files without @tests still have a bounded load-only validity gate.
    if tests.is_empty() {
        let (script, excerpt) = write_test_program(lua, "").map_err(|e| SkullFailure {
            reason: "invalid lua emitted (skull bug)".to_string(),
            rendered: e.to_string(),
        })?;
        let run = run_test_program(script.path(), limit, run_token(), 0, None).map_err(|e| {
            SkullFailure {
                reason: "invalid lua emitted (skull bug)".to_string(),
                rendered: e.to_string(),
            }
        })?;
        if run.timed_out || !run.output.status.success() {
            return Err(SkullFailure {
                reason: "invalid lua emitted (skull bug)".to_string(),
                rendered: if run.timed_out {
                    format!(
                        "loading emitted program: timed out after {}s",
                        limit.as_secs_f64()
                    )
                } else {
                    format!(
                        "{}{}\n\n--- emitted lua (truncated) ---\n{}",
                        String::from_utf8_lossy(&run.output.stdout),
                        String::from_utf8_lossy(&run.output.stderr),
                        excerpt
                    )
                },
            });
        }
        return Ok(SkullOutcome::Pass {
            tests: 0,
            skipped: 0,
            timings: Vec::new(),
        });
    }
    if selected_tests.is_empty() {
        return Ok(SkullOutcome::Pass {
            tests: 0,
            skipped,
            timings: Vec::new(),
        });
    }
    // Only the selected tests exist from here on: the harness, the shard
    // indices and the never-reported accounting all count them alone.
    let (tests, files) = (selected_tests.as_slice(), selected_files.as_slice());

    // Run the @tests: emit + harness, capture each test's PASS/FAIL line.
    // A stable key keeps existing tests in place when declarations are inserted.
    let token = run_token();
    let shards = resolve_test_shards(
        shards,
        tests.len(),
        lua.len(),
        std::env::var_os("ROCK_TEST_SHARDS").is_some(),
    );
    let harness = build_test_harness(tests, &token);
    let (script, excerpt) = write_test_program(lua, &harness).map_err(|e| SkullFailure {
        reason: "test run errored (skull bug)".to_string(),
        rendered: e.to_string(),
    })?;
    drop(harness);
    // Per-file runs keep freed pages for the next compile; package runs only wait.
    if release_memory {
        crate::allocator::release_freed_memory();
    }
    let requested_shards = shards.clamp(1, tests.len());
    let mut assignment: Vec<Vec<usize>> = (0..requested_shards).map(|_| Vec::new()).collect();
    for (index, test) in tests.iter().enumerate() {
        assignment[stable_test_shard(&test.name, requested_shards)].push(index);
    }
    assignment.retain(|indices| !indices.is_empty());
    let shards = assignment.len();
    let runs: Vec<Result<Vec<ShardRun>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = assignment
            .iter()
            .map(|indices| {
                let (script, token) = (script.path(), token.clone());
                scope.spawn(move || run_shard(script, limit, token, indices, shards > 1))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("test shard thread panicked")))
            })
            .collect()
    });
    drop(script);

    let expects: HashMap<&str, &skull::ValueExpect> = tests
        .iter()
        .filter_map(|t| t.returns.as_ref().map(|v| (t.name.as_str(), v)))
        .collect();
    let mut passed = 0;
    let mut fails = Vec::new();
    let mut diagnostics = Vec::new();
    let mut timings = Vec::new();
    let mut segments = Vec::new();
    for (shard, run) in runs.into_iter().enumerate() {
        let run = run.map_err(|e| SkullFailure {
            reason: "test run errored (skull bug)".to_string(),
            rendered: e.to_string(),
        })?;
        for (process, segment) in run.into_iter().enumerate() {
            let label = if process == 0 {
                format!("shard {}/{shards}: ", shard + 1)
            } else {
                format!("shard {}/{shards} process {}: ", shard + 1, process + 1)
            };
            segments.push((label, segment));
        }
    }
    for (label, ShardRun { indices, run }) in segments {
        let indices = indices.as_slice();
        // The harness `pcall`s each test, so an ordinary assertion failure is a
        // FAIL marker and the process still exits 0. A non-zero status means
        // the run DIED -- `os.exit`, a C-level abort, an OOM -- and every test
        // after that point never ran in this process. A truncated run still
        // reports what it got; `run_shard` reruns the tests after a stopped one.
        let stdout = String::from_utf8_lossy(&run.output.stdout);
        let stderr = String::from_utf8_lossy(&run.output.stderr);
        let died = !run.output.status.success();

        // No result lines and no test started => the module didn't even load
        // (a skull bug), not a test failure.
        let markers = crate::test_protocol::parse_skull_markers(&stdout);
        if markers.is_none() && !run.timed_out && !died && run.running_test.is_none() {
            return Err(SkullFailure {
                reason: "invalid lua emitted (skull bug)".to_string(),
                rendered: format!(
                    "{label}{stdout}\n{stderr}\n\n--- emitted lua (truncated) ---\n{excerpt}"
                ),
            });
        }
        let markers = markers.unwrap_or(crate::test_protocol::SkullMarkers {
            passed: 0,
            fails: Vec::new(),
            values: Vec::new(),
            times: Vec::new(),
        });
        let reported = markers.passed + markers.fails.len() + markers.values.len();
        passed += markers.passed;
        fails.extend(markers.fails);

        // VALUE lines: compare the serialized return value HOST-side against
        // the pinned `@test(returns = ...)` literal.
        let mut value_fails = HashSet::new();
        for (name, val) in &markers.values {
            let Some(expect) = expects.get(name.as_str()) else {
                return Err(SkullFailure {
                    reason: "harness invariant breach (skull bug)".to_string(),
                    rendered: format!("VALUE line for `{name}` without a returns expectation"),
                });
            };
            match crate::test_protocol::compare_value(
                expect,
                val,
                crate::test_protocol::FLOAT_REL_TOL,
            ) {
                Ok(()) => passed += 1,
                Err(detail) => {
                    value_fails.insert(name.as_str());
                    fails.push(format!("{name}: {detail}"));
                }
            }
        }
        // A shard's TIME lines follow its run order, so the j-th is test
        // `indices[j]`; keying by it restores declaration order after merging.
        timings.extend(markers.times.iter().zip(indices).map(|(time, &index)| {
            (
                index,
                TestTiming {
                    name: time.name.clone(),
                    ms: time.ms,
                    status: match time.result.as_str() {
                        "pass" => "pass",
                        "value" if !value_fails.contains(time.name.as_str()) => "pass",
                        _ => "fail",
                    },
                },
            )
        }));

        // Every selected test owes exactly one marker. Counting only the
        // markers that ARRIVED makes a truncated run self-consistent -- three
        // tests, one marker, `1/1 passed` -- so each test that never reported
        // is a named failure. This is the only signal for a premature
        // `os.exit(0)`, which exits success and emits no marker at all.
        let stop = if run.timed_out {
            timeout_cause(&run, limit)
        } else if died {
            "child process stopped".to_string()
        } else {
            "process exited 0 before all tests reported".to_string()
        };
        let missing = indices.iter().copied().skip(reported).collect::<Vec<_>>();
        if run.timed_out || died || !missing.is_empty() {
            let state = if let Some(position) = run.running_test {
                indices
                    .get(position)
                    .map(|&index| {
                        format!("running test `{}` ({})", tests[index].name, files[index],)
                    })
                    .unwrap_or_else(|| format!("running shard test ordinal {position}"))
            } else if let Some(position) = run.last_completed_test {
                indices
                    .get(position)
                    .map(|&index| {
                        format!(
                            "between tests after `{}` ({})",
                            tests[index].name, files[index],
                        )
                    })
                    .unwrap_or_else(|| "between tests".to_string())
            } else {
                "before the first test started".to_string()
            };
            let missing = missing
                .iter()
                .map(|&index| format!("{}: never reported ({})", tests[index].name, files[index],))
                .collect::<Vec<_>>()
                .join(", ");
            diagnostics.push(format!(
                "{label}stopped: {stop}; exit status: {}; {state}; never reported: {}; stderr:\n{}",
                child_exit_status(&run.output.status),
                if missing.is_empty() {
                    "<none>"
                } else {
                    &missing
                },
                stderr_excerpt(&run.output.stderr),
            ));
        }
        for (position, &index) in indices.iter().enumerate().skip(reported) {
            let reason = if run.running_test == Some(position) {
                format!("{stop} while it ran")
            } else if run.running_test.is_none() && run.last_completed_test.is_some() {
                format!("{stop} between tests")
            } else {
                "its process stopped before it ran".to_string()
            };
            fails.push(format!(
                "{}: never reported ({}; {label}{reason})",
                tests[index].name, files[index]
            ));
        }
        if reported >= indices.len() && died {
            fails.push(format!(
                "{label}every @test reported, but the harness process exited non-zero -- \
                 something after the tests failed"
            ));
        }
    }
    timings.sort_by_key(|(index, _)| *index);
    let timings = timings.into_iter().map(|(_, timing)| timing).collect();

    if fails.is_empty() {
        Ok(SkullOutcome::Pass {
            tests: passed,
            skipped,
            timings,
        })
    } else {
        Ok(SkullOutcome::TestFail {
            passed,
            fails,
            skipped,
            timings,
            diagnostics,
        })
    }
}

/// Child CPU time one `@test` (or program loading) may use: a hang backstop,
/// not a speed bound. A loaded host inflates a test's CPU time (memory
/// reclaim, E-cores, busy SMT siblings): the slowest `project` test used 21 s on
/// the build host at load 70 with little free memory, and 16 s under an all-core
/// busy-loop burner.
const TEST_CPU_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// One test process of a shard and the tests it was given.
struct ShardRun {
    indices: Vec<usize>,
    run: crate::common::ProcessResult,
}

/// Processes a shard may start after the first. A stopped test is charged by
/// name and the tests after it run in a fresh process; the cap bounds a
/// defect that stops every test to a few process starts and budgets.
const SHARD_RESTARTS: usize = 3;

/// Run one shard's tests. When the process stops while a test runs (watchdog,
/// crash, early exit), that test keeps the stopped run and the tests after it
/// rerun in a fresh process, so one stopped test never takes the rest of the
/// shard with it. `select` passes the indices even for a whole-program run.
fn run_shard(
    script: &std::path::Path,
    limit: std::time::Duration,
    token: String,
    indices: &[usize],
    select: bool,
) -> Result<Vec<ShardRun>> {
    let mut runs = Vec::new();
    let mut rest = indices;
    loop {
        let selection = (select || !runs.is_empty()).then(|| {
            rest.iter()
                .map(|i| (i + 1).to_string())
                .collect::<Vec<_>>()
                .join(",")
        });
        let run = run_test_program(
            script,
            limit,
            token.clone(),
            rest.len(),
            selection.as_deref(),
        )?;
        // A test still running when the process ended is the one that stopped it.
        let resume_at = run
            .running_test
            .filter(|_| runs.len() < SHARD_RESTARTS)
            .map(|position| position + 1)
            .filter(|&next| next < rest.len());
        let Some(next) = resume_at else {
            runs.push(ShardRun {
                indices: rest.to_vec(),
                run,
            });
            return Ok(runs);
        };
        runs.push(ShardRun {
            indices: rest[..next].to_vec(),
            run,
        });
        rest = &rest[next..];
    }
}

/// A per-run token for the harness's start markers, only distinct from
/// accidental test output. Lead decision: the per-test limit is a hang guard
/// for this repo's own tests, not a security boundary, so deliberate
/// in-process forgery by Lua code (debug locals, readable harness source,
/// intercepted harness writes) is out of scope.
fn run_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = || {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        hasher.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

struct TestProgram {
    preamble: String,
    lua: String,
    setup: &'static str,
}

impl TestProgram {
    fn len(&self) -> usize {
        self.preamble.len() + self.lua.len() + self.setup.len()
    }
}

impl From<String> for TestProgram {
    fn from(lua: String) -> Self {
        Self {
            preamble: String::new(),
            lua,
            setup: "",
        }
    }
}

fn program_excerpt(parts: &[&str]) -> String {
    let bytes: usize = parts.iter().map(|part| part.len()).sum();
    let mut head = String::with_capacity(bytes.min(4000));
    for part in parts {
        let end = part.floor_char_boundary(4000 - head.len());
        head.push_str(&part[..end]);
        if end < part.len() {
            break;
        }
    }
    if bytes > 4000 {
        head.push_str(&format!("\n... ({} more bytes)", bytes - head.len()));
    }
    head
}

/// A file avoids blocking on stdin before the startup watchdog is running, and
/// lets every shard load the same program.
fn write_test_program(
    program: TestProgram,
    harness: &str,
) -> Result<(tempfile::NamedTempFile, String)> {
    use std::io::Write;
    let parts = [&*program.preamble, &*program.lua, program.setup, harness];
    let excerpt = program_excerpt(&parts);
    let mut script = tempfile::NamedTempFile::new()?;
    for part in parts {
        script.write_all(part.as_bytes())?;
    }
    Ok((script, excerpt))
}

/// Run `script` under the per-test watchdog. `selection` restricts the run to
/// one shard's tests (`ROCK_TEST_SHARD_TESTS`); `tests` counts the tests run.
fn run_test_program(
    script: &std::path::Path,
    limit: std::time::Duration,
    token: String,
    tests: usize,
    selection: Option<&str>,
) -> Result<crate::common::ProcessResult> {
    use rock_host::process_management::{register_child_process, unregister_child_process};
    use std::process::Stdio;

    let mut command = rock_host::project::build_luajit_command(script, &[]);
    // Shards and concurrent runs share one worktree; each process gets its own
    // scratch root, removed when this returns.
    let _scratch = crate::runtime_process::prepare_test(&mut command)?;
    // An inherited selection would silently narrow an unsharded run.
    match selection {
        Some(selection) => command.env("ROCK_TEST_SHARD_TESTS", selection),
        None => command.env_remove("ROCK_TEST_SHARD_TESTS"),
    };
    command =
        crate::runtime_process::configure(command, crate::runtime_process::RuntimeProcess::LuaTest);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    register_child_process(pid);
    let result = crate::common::ProcessManager::wait_with_test_watchdog(
        &mut child,
        limit,
        crate::common::TestWatch { token, tests },
    );
    if result.is_err() {
        if let Err(error) = child.kill() {
            log::error!(target: "CLI", "failed to kill Lua test process: {error}");
        }
        if let Err(error) = child.wait() {
            log::error!(target: "CLI", "failed to reap Lua test process: {error}");
        }
    }
    unregister_child_process(pid);
    result
}

/// A directory that is a `stick.toml` package -- the unit `rock test` builds as
/// ONE whole workspace (all its files + deps + its own `tests/`) rather than
/// file-by-file. A `type=lib` split across files can only be tested this way:
/// per-file, a file referencing a sibling's symbol fails to compile. Root
/// `tests/` (no stick.toml, hundreds of independent colliding programs) is NOT
/// a package and stays on the per-file path; a package NESTED under it is one,
/// and [`walk_test_units`] finds it.
fn is_test_package_dir(path: &str) -> bool {
    let p = std::path::Path::new(path);
    p.is_dir() && p.join("stick.toml").is_file()
}

pub(crate) fn is_package_tests_dir(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let name = p.file_name().and_then(|n| n.to_str());
        if name == Some("tests") || name == Some("tests/") {
            let parent = p.parent().unwrap_or(std::path::Path::new(""));
            let candidate = if parent.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                parent
            };
            if candidate.join("stick.toml").is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn is_rock_test_source(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|e| e == "rock") && !path.to_string_lossy().ends_with('~')
}

/// Split the `.rock` files under `path` into loose-file units and PACKAGE
/// units. A directory holding a `stick.toml` is a package: it becomes one unit
/// compiled through its own manifest, and the walk does not descend into it.
///
/// Building a package's sources as loose files drops the manifest that supplies
/// their dialect and their dependencies, so a v1 source is parsed in the legacy
/// dialect and fails with `unsupported item start` -- an emit failure that says
/// nothing about the package it came from. The ownership rule: a nested
/// manifest starts a different package and is not absorbed as
/// source of the tree above it.
///
/// Single-file and directory inputs use the same source filter. `.lsp` and
/// `.err` fixtures belong only to their dedicated suites.
fn walk_test_units(
    path: &str,
    files: &mut Vec<String>,
    package_dirs: &mut Vec<String>,
) -> Result<()> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        anyhow::bail!("Path does not exist: {}", path);
    }
    if p.is_file() {
        if is_rock_test_source(p) {
            files.push(path.to_string());
        }
        return Ok(());
    }
    let base = fs_err::canonicalize(p)?;
    let mut walk = walkdir::WalkDir::new(&base).sort_by_file_name().into_iter();
    while let Some(entry) = walk.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "rock: skipping path under `{}` while scanning for tests: {e}",
                    base.display()
                );
                continue;
            }
        };
        let entry_path = entry.path();
        if entry.file_type().is_dir() {
            if entry_path != base && entry_path.join("stick.toml").is_file() {
                package_dirs.push(entry_path.to_string_lossy().into_owned());
                walk.skip_current_dir();
            }
            continue;
        }
        if is_rock_test_source(entry_path) {
            files.push(entry_path.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Assemble + type-check + emit a whole package (tests INCLUDED), then run its
/// `@test`s. The package analogue of [`skull_build`]. Cross-file references
/// resolve because every file is in one workspace.
fn skull_build_package(
    dir: &str,
    shards: usize,
    filter: Option<&crate::test::TestFilter>,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    skull_build_package_with_limit(dir, TEST_CPU_LIMIT, shards, filter)
}

fn skull_build_package_with_limit(
    dir: &str,
    limit: std::time::Duration,
    shards: usize,
    filter: Option<&crate::test::TestFilter>,
) -> std::result::Result<SkullOutcome, SkullFailure> {
    // Like a game run, so tests never read a bank an earlier tree compiled.
    (crate::registry::RuntimeServices::registered().prepare_unit)(dir).map_err(|e| {
        SkullFailure {
            reason: "sound bank preparation failed".to_string(),
            rendered: format!("sound bank preparation failed for {dir}: {e:#}"),
        }
    })?;
    let slot = crate::compile_slots::acquire(dir).map_err(|e| SkullFailure {
        reason: "no compile slot".to_string(),
        rendered: format!("{e:#}"),
    })?;
    let spec = crate::unified_compile::SourceSpec::Directory(std::path::PathBuf::from(dir));
    let sources = crate::unified_compile::discover_sources(&spec).map_err(|e| SkullFailure {
        reason: "cannot load project".to_string(),
        rendered: e.to_string(),
    })?;
    let sources = crate::package_test_sources::select(&sources, std::path::Path::new(dir))
        .map_err(|error| SkullFailure {
            reason: "cannot select package test sources".to_string(),
            rendered: format!("{error:#}"),
        })?;
    let (modules, entry_file) = skull_assemble_project(&sources).map_err(|errs| SkullFailure {
        reason: categorize(&errs[0].message),
        rendered: render_project_errs(&sources, &errs),
    })?;
    let mut host = PackageTestHost {
        inner: CliCompileHost {
            sources: &sources,
            lint_root: std::path::Path::new(dir),
            lint_scope: crate::lint_pass::Scope::WholeProject,
            comptime: CliComptime::FromEntry(dir),
            prepare_timings: Vec::new(),
        },
        test_dir: dir,
    };
    let target = skull::pipeline::EmitTarget {
        project_root: None,
        renderer: skull::mir_emit::RendererTarget::Gl,
    };
    let skull::pipeline::Compiled {
        ws, emission: emit, ..
    } = skull::pipeline::compile_modules(modules, entry_file, target, &mut host).map_err(
        |failure| SkullFailure {
            reason: categorize(&failure.errors[0].message),
            rendered: render_project_errs(&sources, &failure.errors),
        },
    )?;
    let lua = emit.lua;
    let test_fns = collect_package_test_fns(&ws, dir).map_err(|e| SkullFailure {
        reason: categorize(&e.message),
        rendered: render_project_err(&sources, &e),
    })?;
    let files = test_fns
        .iter()
        .map(|(func, _)| ws.modules[func.module.index()].file.as_str().to_owned())
        .collect::<Vec<_>>();
    let tests = test_fns
        .into_iter()
        .map(|(_, test)| test)
        .collect::<Vec<_>>();
    let facts = test_program_facts(&ws);
    let lua = install_global_accessors(lua, &ws)
        .map_err(|e| SkullFailure {
            reason: categorize(&e.message),
            rendered: render_project_err(&sources, &e),
        })?
        .parts()
        .concat();
    // The compiler state is dead once the program is emitted. Free it before
    // the program text is copied and the test process runs: project's peaks near
    // 5 GB, and keeping it resident doubled the memory a `rock test` needed
    // while its luajit child ran.
    drop((ws, sources));
    crate::allocator::release_freed_memory();
    drop(slot);
    let host = (crate::registry::RuntimeServices::registered().program_host)(
        crate::registry::TestUnit::Package { dir, facts: &facts },
    )
    .map_err(SkullFailure::from)?;
    let lua = TestProgram {
        preamble: host.preamble,
        lua,
        setup: host.setup,
    };
    run_emitted_tests(lua, &tests, &files, limit, shards, filter, true)
}

/// Package shard limit; the default resolves against the selected tests and
/// program size in `run_emitted_tests`, before the program text is freed.
fn test_shard_count() -> Result<usize> {
    let value = match std::env::var("ROCK_TEST_SHARDS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => anyhow::bail!("ROCK_TEST_SHARDS: {error}"),
    };
    test_shard_count_from(
        value.as_deref(),
        std::thread::available_parallelism().map_or(1, |cores| cores.get()),
    )
}

fn test_shard_count_from(value: Option<&str>, cores: usize) -> Result<usize> {
    match value {
        Some(value) => match value.trim().parse::<usize>() {
            Ok(shards) if shards > 0 => Ok(shards),
            _ => anyhow::bail!("ROCK_TEST_SHARDS must be a positive integer, got `{value}`"),
        },
        None => Ok(cores.clamp(1, 2)),
    }
}

fn stable_test_shard(name: &str, shards: usize) -> usize {
    let hash = name
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    (hash % shards as u64) as usize
}

fn resolve_test_shards(
    shards: usize,
    tests: usize,
    program_bytes: usize,
    overridden: bool,
) -> usize {
    if !overridden && (tests < 60 || program_bytes < 8 * 1024 * 1024) {
        1
    } else {
        shards
    }
}

#[cfg(test)]
#[path = "../tests/test_shards/policy.rs"]
mod test_shard_policy;

/// Read the checked FFI namespace without assigning runtime meaning to names.
fn test_program_facts(
    ws: &bone_semantic::inference::Workspace,
) -> crate::registry::TestProgramFacts {
    let ffi_libraries = ws
        .modules
        .iter()
        .flat_map(|module| {
            module.functions.iter().filter_map(|(_, function)| {
                (function.is_extern && function.backend != bone_semantic::hir::FnBackend::Js)
                    .then_some(function.ffi_lib)
                    .flatten()
                    .map(|library| library.as_str().to_owned())
            })
        })
        .collect();
    crate::registry::TestProgramFacts { ffi_libraries }
}

/// The `@test` fns defined in the package rooted at `dir` -- its own source
/// files and its `tests/`, but NOT stdlib or dependency packages (which live at
/// other canonical paths). Module file paths are canonicalized absolute, so a
/// prefix match against the canonicalized `dir` selects exactly this package.
/// Carry each test's `FuncRef` so executors retain its source file.
pub(crate) fn collect_package_test_fns(
    ws: &bone_semantic::inference::Workspace,
    dir: &str,
) -> std::result::Result<Vec<(skull_mir::mir::FuncRef, skull::SkullTest)>, CompilerError> {
    let root = std::path::Path::new(dir)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(dir));
    let mut out = Vec::new();
    for m in ws.source_modules() {
        let under = std::path::Path::new(m.file.as_str())
            .canonicalize()
            .map(|c| c.starts_with(&root))
            .unwrap_or(false);
        if under {
            out.extend(
                skull::module_tests_with_ids(ws, m.id)?
                    .into_iter()
                    .map(|(fid, t)| {
                        (
                            skull_mir::mir::FuncRef {
                                module: m.id,
                                func: fid,
                            },
                            t,
                        )
                    }),
            );
        }
    }
    Ok(out)
}

/// Keep emitted-Lua dumps in error reports manageable.
#[cfg(test)]
fn truncate(s: &str) -> String {
    if s.len() > 4000 {
        let end = s.floor_char_boundary(4000);
        format!("{}\n... ({} more bytes)", &s[..end], s.len() - end)
    } else {
        s.to_string()
    }
}

/// Parse the `--profile` value for the `rock shader` entry forms -- the only
/// command that selects a profile (in-game source is emitted once and adapted
/// to the live GL context by the engine). No fallback: an unknown value rejects
/// listing the valid ones.
pub(crate) fn parse_glsl_profile(s: Option<&str>) -> Result<skull_codegen_glsl::GlslProfile> {
    use skull_codegen_glsl::GlslProfile;
    match s {
        None | Some("330core") => Ok(GlslProfile::Core330),
        Some(other) => {
            anyhow::bail!("rock shader: unknown --profile `{other}` (valid: 330core)")
        }
    }
}

fn emit_shader_program_with_view(
    ws: &bone_semantic::inference::Workspace,
    program: skull::shader_artifact::ShaderProgram,
    profile: skull_codegen_glsl::GlslProfile,
) -> std::result::Result<skull_codegen_glsl::ShaderEmission, skull_parser::CompilerError> {
    let roots: Vec<skull_mir::mir::FuncRef> = program
        .vert
        .into_iter()
        .chain(std::iter::once(program.frag))
        .collect();
    let graph = skull::instance_graph::build(ws, roots, false);
    let shader_scx = skull_mir::lower::ScalarCtx::new_values_first_class(ws)
        .sharing(std::sync::Arc::clone(graph.interner()));
    let programs = [program];
    let view = skull::shader_artifact::realize_shader_with_context(
        ws,
        &graph,
        &skull::shader_artifact::ShaderArtifactRequest {
            spec: skull::target_projection::TargetSpec {
                shader: Some(skull::target_projection::ShaderTargetSpec::Glsl(profile)),
                ..skull::target_projection::LUAJIT_DESKTOP
            },
            programs: &programs,
        },
        &shader_scx,
    )
    .ok_or_else(|| {
        skull_parser::CompilerError::new_no_location("GLSL shader view is unavailable".to_string())
    })?;
    let program_view = view.program_views.first().ok_or_else(|| {
        skull_parser::CompilerError::new_no_location("GLSL shader view has no program".to_string())
    })?;
    match program.vert {
        Some(_) => skull_codegen_glsl::emit_shader_pipeline_with_view(
            ws,
            &shader_scx,
            program_view,
            profile,
        ),
        None => {
            skull_codegen_glsl::emit_shader_entry_with_view(ws, &shader_scx, program_view, profile)
        }
    }
}

/// `rock shader <file.rock> <fn> [fn...] [--output <file.glsl>]` -- GLSL emission.
/// Three forms:
///   - fn library (route B): plain roots, no `#version` line -- `--profile`
///     rejects here (nothing for it to select).
///   - single `@shader(fragment)` root (A1 fullscreen) and
///     `<vert_fn> <frag_fn>` (A2 pipeline, selected when a root is
///     `@shader(vertex)`): both take `[--profile 330core]`.
///
/// A profile violation renders spanned against the source and exits non-zero.
pub(crate) fn skull_shader(
    files: &[String],
    output: Option<String>,
    profile: Option<String>,
) -> Result<()> {
    let Some((path, roots)) = files.split_first() else {
        anyhow::bail!(
            "rock shader: usage: rock shader <file.rock> <fn> [fn...] --output <file.glsl>"
        );
    };
    if roots.is_empty() {
        anyhow::bail!("rock shader: need at least one root fn name");
    }
    if let Some(out) = output.as_deref() {
        invalidate_shader_output(path, out)?;
    }
    let source = match rock_host::source_files::source_file_new_with_context(path) {
        Ok(source) => source,
        Err(e) => {
            log::error!(target: "SHADER", "cannot load {}: {}", path, e);
            crate::invocation_log::exit(1);
        }
    };
    let src = source.contents;
    let mut user = match skull_parser::parse_module_with_dialect(
        &src,
        ustr::Ustr::from(path.as_str()),
        source.dialect,
    ) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{}", render_file_errs(path, &src, &[e]));
            crate::invocation_log::exit(1);
        }
    };
    user.package = source.package;
    user.global_policy = source.global_policy;
    let user_file = user.file;
    let ws = match crate::bone_backend::check_with_stdlib([user]) {
        Ok(ws) => ws,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::invocation_log::exit(1);
        }
    };
    let user_mid = crate::bone_backend::entry_module_id(&ws, user_file);
    let root_names = roots;
    let fragment = ws.fragment_scopes.get(&user_file);
    let roots: Vec<String> = roots
        .iter()
        .map(|name| {
            bone_semantic::lower::resolve::resolve_bare(&ws, fragment, ustr::Ustr::from(name))
                .map(|resolved| resolved.key.to_string())
                .map_err(|error| anyhow::anyhow!("rock shader: root `{name}`: {error}"))
        })
        .collect::<Result<_>>()?;

    // Pipeline form: any root is a `@shader(vertex)` entry.
    let is_pipeline = roots
        .iter()
        .any(|r| skull_codegen_glsl::is_vertex_shader_root(&ws, user_mid, r));

    let glsl = if is_pipeline {
        if roots.len() != 2 {
            anyhow::bail!(
                "rock shader: the vertex+fragment pipeline form takes exactly two roots: <vert_fn> <frag_fn>"
            );
        }
        let prof = parse_glsl_profile(profile.as_deref())?;
        let vert = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[0]) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::invocation_log::exit(1);
            }
        };
        let frag = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[1]) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::invocation_log::exit(1);
            }
        };
        match emit_shader_program_with_view(
            &ws,
            skull::shader_artifact::ShaderProgram {
                vert: Some(vert),
                frag,
            },
            prof,
        )
        .map(|emission| emission.source)
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::invocation_log::exit(1);
            }
        }
    } else {
        // The route-B fn library emits no `#version` line, so a profile cannot
        // apply to it; the single-root fullscreen entry takes one like the
        // pipeline form does.
        let is_entry = roots.len() == 1
            && skull_codegen_glsl::is_fragment_shader_root(&ws, user_mid, &roots[0]);
        if profile.is_some() && !is_entry {
            anyhow::bail!(
                "rock shader: --profile applies to shader entries (pipeline or @shader(fragment) fullscreen), not the route-B fn library (it emits no #version line)"
            );
        }
        let prof = parse_glsl_profile(profile.as_deref())?;
        let glsl = if is_entry {
            let frag = match skull_codegen_glsl::resolve_root(&ws, user_mid, &roots[0]) {
                Ok(root) => root,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::invocation_log::exit(1);
                }
            };
            emit_shader_program_with_view(
                &ws,
                skull::shader_artifact::ShaderProgram { vert: None, frag },
                prof,
            )
            .map(|emission| emission.source)
        } else {
            let root_refs = roots
                .iter()
                .map(|root| skull_codegen_glsl::resolve_root(&ws, user_mid, root))
                .collect::<std::result::Result<Vec<_>, _>>();
            let root_refs = match root_refs {
                Ok(root_refs) => root_refs,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::invocation_log::exit(1);
                }
            };
            let session = match skull_codegen_glsl::shader_library_session(&ws, root_refs) {
                Ok(session) => session,
                Err(e) => {
                    eprintln!("{}", render_file_errs(path, &src, &[e]));
                    crate::invocation_log::exit(1);
                }
            };
            skull_codegen_glsl::emit_shader_library_with_view(&ws, &session.scx, &session.view)
        };
        match glsl {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{}", render_file_errs(path, &src, &[e]));
                crate::invocation_log::exit(1);
            }
        }
    };

    match output {
        Some(out) => {
            // Keep the fn-library header text stable: utils_math.glsl byte-identity.
            let mode = if glsl.starts_with("#version") {
                "shader entry"
            } else {
                "route B"
            };
            let profile_flag = profile
                .as_deref()
                .map(|p| format!(" --profile {p}"))
                .unwrap_or_default();
            let header = format!(
                "// GENERATED by skull-codegen-glsl ({mode}) -- DO NOT EDIT.\n\
                 // Regenerate: rock shader {} {}{} --output {}\n\n",
                path,
                root_names.join(" "),
                profile_flag,
                out
            );
            // A `#version` output puts the header AFTER the version line: the ES
            // profile requires `#version` to be the very first token (a leading
            // comment is a hard error), and the engine loader keeps the header
            // in-source, so a header-before-#version breaks the mobile profile.
            let file = if let Some(rest) = glsl.strip_prefix("#version") {
                let nl = rest.find('\n').map(|p| p + 1).unwrap_or(rest.len());
                let (version_tail, body) = rest.split_at(nl);
                let body = body.strip_prefix('\n').unwrap_or(body);
                format!("#version{version_tail}{header}{body}")
            } else {
                format!("{header}{glsl}")
            };
            rock_host::project::write_atomic(&out, &file)?;
            log::info!(target: "SHADER", "{} [{}] -> {}", path, root_names.join(", "), out);
        }
        None => print!("{glsl}"),
    }
    Ok(())
}

fn invalidate_shader_output(input: &str, output: &str) -> Result<()> {
    let input = std::path::Path::new(input);
    let output = std::path::Path::new(output);
    let aliases = input == output
        || match (fs_err::canonicalize(input), fs_err::canonicalize(output)) {
            (Ok(input), Ok(output)) => input == output,
            _ => false,
        };
    if aliases {
        anyhow::bail!("rock shader: --output must not refer to the input file");
    }
    match fs_err::remove_file(output) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn skull_compile_single(path: &str, output: Option<String>) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_compile_project(path, output);
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            crate::invocation_log::exit(1);
        }
    };
    let budget = crate::compile_budget::Scope::begin(path);
    match skull_emit(path, &src) {
        Ok((lua, _emits_main)) => {
            match output {
                Some(out) => {
                    rock_host::project::write_atomic(&out, &lua)?;
                    log::info!(target: "SKULL", "{} -> {}", path, out);
                }
                None => {
                    // Result output (the compiled Lua), not logging — pipeable.
                    print!("{}", lua);
                }
            }
            crate::compile_budget::phase("write");
            budget.success();
            Ok(())
        }
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::invocation_log::exit(1);
        }
    }
}

/// `rock watch --backend skull` — live recompile loop: on each change,
/// re-discover sources, bone-check, and emit the full `.lua` via the new
/// (cell-model) path to `output` (default `tmp/main.lua`). A parse/type error
/// renders and serves the previous good output; the loop keeps running. (The old
/// IncrementalEmitter was retired with the old IR; a batch re-emit is fast —
/// a game project assembles in a few ms.)
pub(crate) fn skull_watch(path: &str, output: Option<String>) -> Result<()> {
    let out_file = output
        .map(std::path::PathBuf::from)
        .unwrap_or_else(rock_host::project::default_main_lua_path);
    if let Some(parent) = out_file.parent() {
        if !parent.exists() {
            fs_err::create_dir_all(parent)?;
        }
    }

    // Live recompile backed by bone's incremental `Session` (warm stdlib,
    // per-body re-inference) + skull's per-body emit cache: a body edit re-emits
    // only the re-inferred chunks (+ their inline-dependent callers), reusing the
    // rest. An interface/structural change (signature, add/remove file) shifts
    // `Idx<Function>`, so the cache is rebuilt with a full emit.
    let mut session: Option<bone_incremental::session::Session> = None;
    let mut cache: Option<skull::mir_emit::SkullEmitCache> = None;

    let build = |label: &str,
                 session: &mut Option<bone_incremental::session::Session>,
                 cache: &mut Option<skull::mir_emit::SkullEmitCache>| {
        let source_spec = crate::common::path_to_source_spec(path);
        let sources = match crate::unified_compile::discover_sources(&source_spec) {
            Ok(s) => s,
            Err(e) => {
                log::error!(target: "WATCH", "skull: source discovery failed: {e}");
                return;
            }
        };
        // Game build: drop test-only files (tests/ dirs) so a broken sim/test
        // can't break the watched game output -- parity with the rock backend's
        // is_game gate.
        let path_ref = std::path::Path::new(path);
        let sources = if path_ref.is_dir()
            && (crate::registry::RuntimeServices::registered().is_game_project)(path_ref)
                .unwrap_or(false)
        {
            match sources.production_only(path_ref) {
                Ok(sources) => sources,
                Err(error) => {
                    log::error!(target: "WATCH", "skull: production source selection failed: {error}");
                    return;
                }
            }
        } else {
            sources
        };
        let files = crate::bone_backend::session_files(&sources);

        let t = Instant::now();
        let mut reinferred: Vec<(
            bone_semantic::prelude::ModuleId,
            bone_semantic::hir::BodyOwner,
        )> = Vec::new();
        let mut full = false; // interface/structural change -> rebuild cache
        let mut update_failed = false;

        match session {
            None => match crate::bone_backend::bone_session_for_emission(
                files.clone(),
                std::path::Path::new(path),
            ) {
                Ok(s) => {
                    *session = Some(s);
                    full = true; // first build: populate the cache
                }
                Err(e) => {
                    eprintln!("{}", e.render(&sources));
                    log::error!(target: "WATCH", "skull {label}: session init failed");
                    return;
                }
            },
            Some(s) => {
                for f in &files {
                    match s.update_file(&f.path, &f.text) {
                        Ok(result) => {
                            if matches!(
                                result.tier,
                                bone_incremental::session::UpdateTier::Interface
                                    | bone_incremental::session::UpdateTier::Scope
                                    | bone_incremental::session::UpdateTier::FullUser
                            ) {
                                full = true;
                            }
                            reinferred.extend(result.reinferred);
                        }
                        Err(e) => {
                            update_failed = true;
                            eprintln!("{}", e.render(&sources));
                        }
                    }
                }
                let gone: Vec<String> = s
                    .tracked_paths()
                    .into_iter()
                    .filter(|p| !files.iter().any(|f| f.path == *p))
                    .map(|p| p.to_string())
                    .collect();
                for file_path in gone {
                    full = true; // removed file shifts module ids
                    if let Err(e) = s.remove_file(&file_path) {
                        update_failed = true;
                        eprintln!("{}", e.render(&sources));
                    }
                }
            }
        }
        let check_ms = t.elapsed().as_secs_f64() * 1000.0;

        let Some(s) = session else { return };
        // A broken edit still COMMITS its partial state to the session (the parse
        // is total now), so the incremental cache -- which reflects the last good
        // emit -- can no longer be diffed against it. Drop the cache on any error
        // so the next clean recompile does a correct full rebuild rather than an
        // incremental emit across the broken intermediate. The reload itself is
        // skipped (last good output keeps running) until the code is clean.
        if update_failed {
            *cache = None;
            log::error!(target: "WATCH", "skull {label}: update failed in {check_ms:.1}ms - serving last good output");
            return;
        }
        let errs = s.diagnostics();
        if !errs.is_empty() {
            *cache = None;
            for e in &errs {
                eprintln!("{}", e.render(&sources));
            }
            log::error!(target: "WATCH", "skull {label}: {} diagnostic(s) in {check_ms:.1}ms - serving last good output", errs.len());
            return;
        }

        // A recompile is a build, so the lint pass runs here too; an `Error`
        // finding refuses the reload the same way a type error does.
        let lint_errs = crate::lint_pass::run_build_lints(
            s.workspace(),
            &sources,
            std::path::Path::new(path),
            crate::lint_pass::Scope::WholeProject,
        );
        if !lint_errs.is_empty() {
            *cache = None;
            for e in &lint_errs {
                eprintln!("{}", render_project_err(&sources, e));
            }
            log::error!(target: "WATCH", "skull {label}: {} lint error(s) - serving last good output", lint_errs.len());
            return;
        }

        let ws = s.workspace();
        let Ok(mid) = skull::user_module_id(ws) else {
            log::error!(target: "WATCH", "skull {label}: no user module");
            return;
        };
        let te = Instant::now();
        let emit = if full || cache.is_none() {
            match skull::mir_emit::emit_full(ws, mid) {
                Ok((e, c)) => {
                    *cache = Some(c);
                    e
                }
                Err(e) => {
                    eprintln!("{}", render_project_err(&sources, &e));
                    log::error!(target: "WATCH", "skull {label}: emit failed - serving last good output");
                    return;
                }
            }
        } else {
            match skull::mir_emit::emit_incremental(ws, cache.as_mut().unwrap(), &reinferred) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("{}", render_project_err(&sources, &e));
                    log::error!(target: "WATCH", "skull {label}: incremental emit failed - serving last good output");
                    return;
                }
            }
        };
        let emit_ms = te.elapsed().as_secs_f64() * 1000.0;
        let mode = if full || reinferred.is_empty() {
            "full"
        } else {
            "incremental"
        };
        match rock_host::project::write_atomic(&out_file, &emit.lua) {
            Ok(()) => {
                log::info!(target: "WATCH",
                "skull {label}: {} files -> {} ({mode}, {} re-inferred; check {check_ms:.1}ms, emit {emit_ms:.1}ms)",
                sources.files.len(), out_file.display(), reinferred.len());
            }
            Err(err) => log::error!(target: "WATCH", "skull {label}: write failed: {err}"),
        }
    };

    build("initial", &mut session, &mut cache);

    crate::run_compile_watch_loop(
        vec![std::path::PathBuf::from(path)],
        &format!("skull: watching {path} for changes"),
        |_event| build("rebuild", &mut session, &mut cache),
    )
}

/// `rock run <file> --backend skull` — emit Lua, append a `main()` call if the
/// file defines one, and execute it.
pub(crate) fn skull_run(path: &str) -> Result<()> {
    if std::path::Path::new(path).is_dir() {
        return skull_run_project(path);
    }
    let src = match fs_err::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot read {}: {}", path, e);
            crate::invocation_log::exit(1);
        }
    };
    let (lua, emits_main) = match skull_emit(path, &src) {
        Ok(l) => l,
        Err(errs) => {
            eprintln!("{}", render_file_errs(path, &src, &errs));
            crate::invocation_log::exit(1);
        }
    };
    let program = if emits_main {
        format!("{lua}\nmain()\n")
    } else {
        lua
    };
    match crate::compile_cmd::run_lua_code(&program) {
        Ok(run) => {
            print!("{}", run.stdout);
            if !run.stderr.is_empty() {
                eprint!("{}", run.stderr);
            }
            // The program's own exit status is the answer. Printing a crash and
            // returning Ok is how `rock run` used to exit 0 on a dead program.
            if !run.success {
                log::error!(target: "SKULL", "program exited with a runtime error");
                crate::invocation_log::exit(1);
            }
            Ok(())
        }
        Err(e) => {
            log::error!(target: "SKULL", "runtime error: {}", e.message);
            crate::invocation_log::exit(1);
        }
    }
}

/// Render a bone `CompilerError` against a whole project's source set, so spans
/// resolve to the right file. Every discovered source is registered under its
/// id-path (the same id used when parsing into a module).
pub(crate) fn render_project_err(sources: &SourceFiles, e: &CompilerError) -> String {
    render_project_errs(sources, std::slice::from_ref(e))
}

/// [`render_project_err`] over every collected diagnostic, joined into one
/// report — the CLI prints ALL of them, matching the LSP.
pub(crate) fn render_project_errs(sources: &SourceFiles, errs: &[CompilerError]) -> String {
    let mut sf = SourceFiles::empty();
    for file in &sources.files {
        let f = file.borrow();
        let id = f.file.to_id_path();
        sf.add_or_update_file(source_file_from_path_and_contents(
            id.clone(),
            id,
            f.contents.clone(),
        ));
    }
    crate::bone_backend::render_errs(&sf, errs)
}

/// Discover + assemble + bone-typecheck + skull-emit a whole project directory
/// into one Lua program string. The shared core of `run <dir>` and `compile
/// <dir>` (the single-file forms share [`skull_emit_full`]); only the tail
/// differs — `run` wraps `main()` and execs, `compile` writes the string out.
/// Renders against the project sources and exits 1 on any discovery / type /
/// emit failure. The `bool` is the emitter's own `emits_main`, which decides
/// whether the caller appends the `__rock_main_wrapper(main)` entry.
fn skull_emit_project(dir: &str) -> Result<(GameProgram, bool)> {
    let inputs = match crate::comptime::inputs_for_project_dir(std::path::Path::new(dir)) {
        Ok(inputs) => inputs,
        Err(e) => {
            log::error!(target: "SKULL", "build var resolution failed for project {}: {:#}", dir, e);
            crate::invocation_log::exit(1);
        }
    };
    skull_emit_project_with_inputs(dir, inputs)
}

fn skull_emit_project_with_inputs(
    dir: &str,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<(GameProgram, bool)> {
    use std::path::PathBuf;
    let _slot = crate::compile_slots::acquire(dir)?;
    crate::compile_budget::phase("compile slot");
    // Discovery reads the project serially; the stdlib parse and the
    // compiler image hash need nothing from it and run alongside.
    skull::pipeline::warm_stdlib();
    crate::comptime::prefetch_compiler_identity(&inputs);
    let spec = crate::unified_compile::SourceSpec::Directory(PathBuf::from(dir));
    let sources = match crate::unified_compile::discover_sources(&spec) {
        Ok(s) => s,
        Err(e) => {
            log::error!(target: "SKULL", "cannot load project {}: {}", dir, e);
            crate::invocation_log::exit(1);
        }
    };
    // Game build: drop test-only files (tests/ dirs) so a compile error in a
    // sim/test can't break the running game.
    let sources = sources
        .production_only(std::path::Path::new(dir))
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    crate::compile_budget::phase("discovery");
    crate::compile_budget::sources(&sources);

    // Compile core: parse(assemble) + bone check + skull codegen. Excludes
    // discovery (disk read) and the caller's output write, matching the bone
    // backend's `compile` figure so loc/sec lines up across backends.
    let t = Instant::now();
    let (modules, entry_file) = match skull_assemble_project(&sources) {
        Ok(v) => v,
        Err(errs) => {
            eprintln!("{}", render_project_errs(&sources, &errs));
            crate::invocation_log::exit(1);
        }
    };
    crate::compile_budget::phase("parse+stdlib");

    // Project build: modules outside `dir` (the `engine` dependency, stdlib)
    // are libraries and prune to what the project reaches. The game build
    // dropped `tests/` above, hence ProductionOnly lints.
    let mut host = CliCompileHost {
        sources: &sources,
        lint_root: std::path::Path::new(dir),
        lint_scope: crate::lint_pass::Scope::ProductionOnly,
        comptime: CliComptime::Resolved(Some(inputs)),
        prepare_timings: Vec::new(),
    };
    let target = skull::pipeline::EmitTarget {
        project_root: Some(dir),
        renderer: skull::mir_emit::RendererTarget::Gl,
    };
    let compiled = skull::pipeline::compile_modules(modules, entry_file, target, &mut host);
    let skull::pipeline::Compiled {
        ws, emission: emit, ..
    } = match compiled {
        Ok(compiled) => {
            crate::compile_budget::emission(&compiled.emission);
            compiled
        }
        Err(failure) => {
            use skull::pipeline::Stage;
            eprintln!("{}", render_project_errs(&sources, &failure.errors));
            match failure.stage {
                Stage::Lint => {
                    log::error!(target: "SKULL", "{} lint error(s) in project {}", failure.errors.len(), dir)
                }
                Stage::Emit => {
                    log::error!(target: "SKULL", "skull emit failed for project {}", dir)
                }
                Stage::Parse | Stage::Check | Stage::Comptime => {
                    log::error!(target: "SKULL", "bone type-check failed for project {}", dir)
                }
            }
            crate::invocation_log::exit(1);
        }
    };
    let allocation_report_path = allocation_report_path_from_env();
    if let Err(e) =
        write_allocation_report(allocation_report_path.as_deref(), &emit.allocation_report)
    {
        log::error!(target: "SKULL", "cannot write allocation report for {}: {:#}", dir, e);
        crate::invocation_log::exit(1);
    }
    let emits_main = emit.emits_main;
    let lua = emit.lua;
    let compile = t.elapsed();
    crate::bone_backend::log_compile_stats(&sources, &ws, compile, compile, None);

    let lua = match install_global_accessors(lua, &ws) {
        Ok(lua) => lua,
        Err(error) => {
            eprintln!("{}", render_project_err(&sources, &error));
            log::error!(target: "SKULL", "global accessor planning failed for {}", dir);
            crate::invocation_log::exit(1);
        }
    };
    // The bone Workspace is dead once `lua` is built -- this is a terminal
    // compile path (`compile`/`run` of a project; never looped in tests or
    // backend-stats), so the only thing after this is the output write / a
    // luajit exec / process exit. Hand the drop to the rayon pool so it overlaps
    // that tail. `sources` (rock SourceFiles, RefCell-backed and ~0.01ms to
    // drop) stays inline. `ws` is owned (bone has no Rc/RefCell), hence Send.
    // A `sequential` build drops inline.
    #[cfg(not(feature = "sequential"))]
    rayon::spawn(move || drop(ws));
    #[cfg(feature = "sequential")]
    drop(ws);
    Ok((lua, emits_main))
}

fn write_allocation_report(
    path: Option<&std::ffi::OsStr>,
    report: &skull::allocation_report::AllocationReport,
) -> Result<()> {
    let Some(path) = path else { return Ok(()) };
    if path.is_empty() {
        anyhow::bail!("SKULL_ALLOCATION_REPORT must name an output file");
    }
    let path = std::path::Path::new(path);
    rock_host::project::write_atomic(path, report.render_text())
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn allocation_report_path_from_env() -> Option<std::ffi::OsString> {
    std::env::var_os("SKULL_ALLOCATION_REPORT").or_else(|| {
        let mut path = std::path::PathBuf::from(std::env::var_os("SKULL_ALLOCATION_ACTIVATION")?);
        path.set_extension("manifest.txt");
        Some(path.into_os_string())
    })
}

/// Emitted Lua with compiler-owned metadata, passed unchanged to the runtime host.
pub struct GameProgram {
    comptime_inputs: bone_semantic::inference::ComptimeInputs,
    lua: String,
    metadata: String,
    metadata_at: usize,
}

impl GameProgram {
    fn with_metadata(lua: String, metadata: String) -> std::result::Result<Self, CompilerError> {
        let marker = "__rock_globals_install_bridge()\n";
        let at = lua.find(marker).ok_or_else(|| {
            CompilerError::new_no_location(
                "compiled program is missing its global directory".to_string(),
            )
        })?;
        Ok(Self {
            lua,
            metadata,
            metadata_at: at + marker.len(),
            comptime_inputs: Default::default(),
        })
    }

    fn parts(&self) -> [&str; 3] {
        let (head, tail) = self.lua.split_at(self.metadata_at);
        [head, &self.metadata, tail]
    }
}

fn install_global_accessors(
    lua: String,
    ws: &bone_semantic::inference::Workspace,
) -> std::result::Result<GameProgram, CompilerError> {
    let mut rows = std::collections::BTreeMap::new();
    let scx = skull_mir::lower::ScalarCtx::new(ws);
    build_global_nobox_accessors(ws, &scx, &mut rows)?;
    let mut metadata = String::from("\n_G.__rock_global_decorate({");
    for (identity, fields) in rows.into_iter().filter(|(_, fields)| !fields.is_empty()) {
        metadata.push_str(&format!("[{identity:?}]={{{}}},\n", fields.join(",")));
    }
    metadata.push_str("})\n");
    let mut program = GameProgram::with_metadata(lua, metadata)?;
    program.comptime_inputs = ws.comptime_inputs.clone();
    Ok(program)
}

/// The `get`/`set` directory fields of each `nobox` global: the rebuild and
/// take-apart pair. A `nobox` global's storage is its leaf slots, so reflection
/// cannot reach it as one stored value; these closures are its accessors.
///
/// Both directions consume one canonical accessor plan
/// (`nobox_global_accessor_shape`), so they cannot drift from each other or from the
/// storage: `get` is the positional leaf ctor over the plan's paths, and
/// `set` reads the same paths in order. Nested cdata uses the flattened `_lct`
/// representation; ordinary cdata and GC-table values retain `_ct` and `_vt`.
///
fn build_global_nobox_accessors(
    ws: &bone_semantic::inference::Workspace,
    scx: &skull_mir::lower::ScalarCtx,
    rows: &mut std::collections::BTreeMap<String, Vec<String>>,
) -> std::result::Result<(), CompilerError> {
    let state_slots = skull::mir_emit::lua_global_state_slots(ws, scx)?;
    for m in ws.source_modules() {
        for (_gid, g) in m.globals.iter().filter(|(_, global)| !global.is_const) {
            let Some(acc) = skull_mir::lower::nobox_global_accessor_shape(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?
            else {
                continue;
            };
            let skull_mir::lower::NoboxGlobalAccessorShape {
                ty_name,
                paths,
                nullable,
                storage,
                ctor_suffix: suffix,
                presence_path: _,
                unbuildable,
            } = acc;
            let ty_symbol = skull_mir::mangle::type_symbol(ws, ty_name);
            let plan = skull_mir::lower::GlobalPlan::for_declared_type(ws, scx, g.ty)
                .map_err(|e| CompilerError::new_no_location(e.message))?;
            // The leaves are emitted symbols, matching the storage slot map.
            let all_leaves = plan
                .leaf_names(skull_mir::mangle::symbol_of_key(g.name))
                .ok_or_else(|| {
                    CompilerError::new_no_location(format!(
                        "nobox global `{}` has an accessor shape but no leaf-name plan",
                        g.name
                    ))
                })?;
            // A nullable global leads with a presence leaf that has no path: it is
            // not a field, so it is branched on rather than read. Everything below
            // pairs the REMAINING leaves with the paths.
            let (present, leaves) = if nullable {
                let Some((p, rest)) = all_leaves.split_first() else {
                    return Err(CompilerError::new_no_location(format!(
                        "nullable nobox global `{}` has no presence leaf",
                        g.name
                    )));
                };
                (Some(*p), rest.to_vec())
            } else {
                (None, all_leaves)
            };
            if leaves.len() != paths.len() {
                return Err(CompilerError::new_no_location(format!(
                    "nobox global `{}` has {} leaves but {} leaf paths",
                    g.name,
                    leaves.len(),
                    paths.len()
                )));
            }
            // Both accessor directions are POSITIONAL over the leaf sequence, so
            // they are sound only where the ctor is too. `nobox_global_accessor_shape`
            // makes that call; guessing is how a leak becomes a miscompile, so an
            // unbuildable shape rejects rather than emit a wrong accessor.
            if let Some(why) = &unbuildable {
                return Err(CompilerError::new_no_location(format!(
                    "nobox global `{}` of type `{ty_name}` cannot be rebuilt: {why}",
                    g.name
                )));
            }
            let all_leaf_names = if let Some(present) = present {
                std::iter::once(present)
                    .chain(leaves.iter().copied())
                    .collect::<Vec<_>>()
            } else {
                leaves.clone()
            };
            let state_keys = all_leaf_names
                .iter()
                .map(|leaf| {
                    state_slots.get(leaf).copied().ok_or_else(|| {
                        CompilerError::new_no_location(format!(
                            "nobox global `{}` leaf `{leaf}` has no Lua storage slot",
                            g.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (present_key, value_keys) = if nullable {
                let (present, values) = state_keys
                    .split_first()
                    .expect("a nullable shape has a presence storage slot");
                (Some(*present), values.to_vec())
            } else {
                (None, state_keys)
            };
            let args = value_keys
                .iter()
                .map(|key| format!("__rock_globals[{key}]"))
                .collect::<Vec<_>>()
                .join(", ");
            let writes = leaves
                .iter()
                .zip(&paths)
                // Table boxes (leaf-table, and a GC table's `nobox` fields) store
                // dotted paths as literal keys; cdata fields remain ordinary
                // nested field reads.
                .zip(&value_keys)
                .map(|((_, p), key)| {
                    let value = if storage != skull_mir::lower::ValueStorage::CData {
                        format!("v[{:?}]", p.as_str())
                    } else {
                        format!("v.{}", p.as_str())
                    };
                    format!("__rock_globals[{key}] = {value}")
                })
                .collect::<Vec<_>>()
                .join("; ");
            let (get, set) = match present {
                // Presence gates BOTH directions: rebuilding an absent value would
                // hand reflection a struct of nils, and a write that left the leaves
                // behind would let a later read resurrect the old value.
                Some(_) => (
                    format!(
                        "if not __rock_globals[{}] then return nil end return {ty_symbol}{suffix}({args})",
                        present_key
                            .as_ref()
                            .expect("nullable accessor has a presence key")
                    ),
                    format!(
                        "__rock_globals[{}] = v ~= nil; if v ~= nil then {writes} else {} end",
                        present_key
                            .as_ref()
                            .expect("nullable accessor has a presence key"),
                        leaves
                            .iter()
                            .zip(&value_keys)
                            .map(|(_, key)| format!("__rock_globals[{key}] = nil"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                ),
                None => (format!("return {ty_symbol}{suffix}({args})"), writes),
            };
            rows.entry(skull_mir::global_storage::global_identity(m.file, g.name))
                .or_default()
                .push(format!(
                    "get=function() {get} end,set=function(v) {set} end"
                ));
        }
    }
    Ok(())
}

/// A checked sim declaration, in workspace/source order. Wire identities use
/// the same derivation as the emitted registry; authored environment pairs
/// retain their order. Runtime interpretation belongs to the host.
pub struct SimDescriptor {
    pub module: bone_core::prelude::ModuleId,
    pub function: bone_core::prelude::Idx<bone_core::hir::Function>,
    pub wire_name: String,
    pub source_file: String,
    pub env_pairs: Vec<(String, String)>,
    pub render: bool,
}

pub fn collect_sim_descriptors(ws: &bone_semantic::inference::Workspace) -> Vec<SimDescriptor> {
    use bone_semantic::hir::AnnoValue;

    let mut sims = Vec::new();
    for module in ws.source_modules() {
        for (fid, f) in module.functions.iter() {
            let Some(wire) = skull_mir::lifecycle::sim_wire_name(f) else {
                continue;
            };
            let mut env_pairs = Vec::new();
            for ann in &f.annotations {
                if !ann.is_builtin("sim_env") {
                    continue;
                }
                let (Some(AnnoValue::String(k)), Some(AnnoValue::String(v))) = (
                    ann.args.first().map(|a| &a.value),
                    ann.args.get(1).map(|a| &a.value),
                ) else {
                    continue;
                };
                env_pairs.push((k.to_string(), v.to_string()));
            }
            sims.push(SimDescriptor {
                module: module.id,
                function: fid,
                wire_name: wire.as_str().to_string(),
                source_file: module.file.as_str().to_string(),
                env_pairs,
                render: f.annotations.iter().any(|a| a.is_builtin("sim_render")),
            });
        }
    }
    sims
}

/// `rock compile <project-dir> --backend skull` — assemble + type-check + emit
/// the whole project to Lua and write it to `output` (or stdout). The directory
/// analogue of the single-file [`skull_compile_single`] path; shares
/// [`skull_emit_project`] with `run`.
fn skull_compile_project(dir: &str, output: Option<String>) -> Result<()> {
    let inputs = crate::comptime::inputs_for_project_dir(std::path::Path::new(dir))?;
    skull_compile_project_with_inputs(dir, output, inputs)
}

pub(crate) fn skull_compile_project_with_inputs(
    dir: &str,
    output: Option<String>,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<()> {
    skull_compile_project_with_recorded_inputs(dir, output, inputs).map(|_| ())
}

/// Compile with pre-resolved host inputs and return the final recorded map,
/// including compiler-derived declarations resolved after semantic checking.
/// Packaging uses this map so metadata and executable reads agree.
pub(crate) fn skull_compile_project_with_recorded_inputs(
    dir: &str,
    output: Option<String>,
    inputs: bone_semantic::inference::ComptimeInputs,
) -> Result<bone_semantic::inference::ComptimeInputs> {
    let budget = crate::compile_budget::Scope::begin(dir);
    let (emit, emits_main) = skull_emit_project_with_inputs(dir, inputs)?;
    // A game's runnable main.lua is the same shape run/engine produce via
    // write_full_main: preamble (game source dir) + emit +
    // __rock_main_wrapper(main). Without the preamble game asset paths are
    // wrong; without the wrapper it never starts.
    let preamble = (RuntimeServices::registered().program_preamble)(dir, ProgramKind::Project)?;
    let parts = skull_game_main(&preamble, &emit, emits_main);
    match output {
        Some(out) => {
            rock_host::project::write_atomic_streamed(&out, |file| {
                write_program_parts(file, &parts)
            })?;
            log::info!(target: "SKULL", "{} -> {}", dir, out);
        }
        None => {
            write_program_parts(&mut std::io::stdout().lock(), &parts)?;
        }
    }
    crate::compile_budget::phase("write");
    budget.success();
    Ok(emit.comptime_inputs)
}

fn skull_run_project(dir: &str) -> Result<()> {
    let runtime = RuntimeServices::registered();
    (runtime.prepare_run)(dir)?;
    let (lua, emits_main) = skull_emit_project(dir)?;
    (runtime.launch)(dir, &lua, emits_main)
}

/// One JSON line per finished `@test`: `{unit, name, ms, status}`.
fn write_test_timings(path: &std::path::Path, timings: &[(String, TestTiming)]) -> Result<()> {
    let mut text = String::new();
    for (unit, t) in timings {
        let row =
            serde_json::json!({ "unit": unit, "name": t.name, "ms": t.ms, "status": t.status });
        text.push_str(&row.to_string());
        text.push('\n');
    }
    fs_err::write(path, text)?;
    Ok(())
}

fn print_slowest_tests(timings: &[(String, TestTiming)]) {
    if timings.is_empty() {
        return;
    }
    let mut slowest: Vec<&TestTiming> = timings.iter().map(|(_, t)| t).collect();
    slowest.sort_by(|a, b| b.ms.total_cmp(&a.ms));
    println!("\n{}", "=== slowest @tests (skull) ===".bold());
    for t in slowest.iter().take(10) {
        println!("  {:>10.1} ms  {}", t.ms, t.name);
    }
}

/// `rock test <paths> --backend skull` — for every `.rock` file, run the skull
/// pipeline and the luajit validity gate; report pass/fail + a bucketed
/// failure-reason histogram (the skull analogue of the bone test histogram).
/// Split `paths` into test UNITS: isolated files (root `tests/`: independent
/// programs that would collide in one workspace) and whole `stick.toml`
/// packages built as ONE workspace each. A cross-file lib can only be tested
/// the second way -- per-file, any file referencing a sibling's symbol fails to
/// compile. Shared by every test executor (luajit + JS).
pub(crate) fn collect_test_units(paths: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut files = Vec::new();
    let mut package_dirs = Vec::new();
    for path in paths {
        if is_test_package_dir(path) {
            package_dirs.push(path.clone());
        } else if let Some(pkg) = is_package_tests_dir(path) {
            package_dirs.push(pkg);
        } else {
            walk_test_units(path, &mut files, &mut package_dirs)?;
        }
    }
    files.sort();
    files.dedup();
    package_dirs.sort();
    // `project`, `project/`, `./project` and `project/tests` all name one package: dedup by
    // its canonical root, keeping the first spelling as the unit label.
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(package_dirs.len());
    for dir in package_dirs {
        if seen.insert(fs_err::canonicalize(&dir)?) {
            unique.push(dir);
        }
    }
    Ok((files, unique))
}

/// `test_threads` caps how many file units compile and run at once (default:
/// one per CPU); packages run after them, one at a time.
pub(crate) fn skull_test(
    paths: &[String],
    filter: Option<&crate::test::TestFilter>,
    test_threads: Option<usize>,
) -> Result<()> {
    crate::lint_pass::set_corpus_mode();
    let verbose = std::env::var("ROCK_TEST_VERBOSE").is_ok();
    let timings_path = std::env::var_os("ROCK_TEST_TIMINGS");

    let (files, package_dirs) = collect_test_units(paths)?;
    let shards = test_shard_count()?;

    let start = Instant::now();
    let mut emit_ok = 0usize; // units: emit valid Lua (passes or test-fails)
    let mut exec_pass = 0usize; // units: emit + all @tests pass
    let mut failed = 0usize; // units: emit/load failure
    let mut tests_total = 0usize; // individual @test functions executed
    let mut tests_passed = 0usize; // individual @test functions that passed
    let mut tests_skipped = 0usize;
    let mut reason_counts: HashMap<String, usize> = HashMap::new();
    let mut details: Vec<(String, String)> = Vec::new();
    let mut timings: Vec<(String, TestTiming)> = Vec::new();

    // bone can panic during inference on some inputs (a bone bug, not ours).
    // Silence the default panic print during the sweep and catch each unit's
    // panic so one bad unit doesn't abort the whole measurement — it's reported
    // as a `bone panic` failure instead.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    // Run every unit (files first, then packages), collecting labelled outcomes
    // so both kinds fold through one reporting path.
    type UnitResult = std::thread::Result<std::result::Result<SkullOutcome, SkullFailure>>;
    // Sampling builds keep one stack for the profiler.
    let threads = if cfg!(feature = "sequential") {
        1
    } else {
        test_threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        })
    };
    let mut outcomes: Vec<(String, UnitResult)> =
        crate::parallel_runner::map_in_order(&files, threads, |file| {
            let outcome: UnitResult = match fs_err::read_to_string(file) {
                Ok(src) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    skull_build(file, &src, filter)
                })),
                Err(e) => Ok(Err(SkullFailure {
                    reason: e.to_string(),
                    rendered: e.to_string(),
                })),
            };
            (short_path(file).to_string(), outcome)
        });
    for dir in &package_dirs {
        let outcome: UnitResult = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            skull_build_package(dir, shards, filter)
        }));
        outcomes.push((short_path(dir).to_string(), outcome));
    }

    for (label, outcome) in outcomes {
        match outcome {
            Ok(Ok(SkullOutcome::Pass {
                tests,
                skipped,
                timings: unit_timings,
            })) => {
                timings.extend(unit_timings.into_iter().map(|t| (label.clone(), t)));
                emit_ok += 1;
                exec_pass += 1;
                tests_total += tests;
                tests_passed += tests;
                tests_skipped += skipped;
                if verbose {
                    println!("{:.<60} {}", label, "PASS".green());
                }
            }
            Ok(Ok(SkullOutcome::TestFail {
                passed,
                fails,
                skipped,
                timings: unit_timings,
                diagnostics,
            })) => {
                timings.extend(unit_timings.into_iter().map(|t| (label.clone(), t)));
                emit_ok += 1;
                tests_total += passed + fails.len();
                tests_passed += passed;
                tests_skipped += skipped;
                println!(
                    "{:.<60} {}  {}",
                    label,
                    "TESTFAIL".yellow(),
                    format!("{} @test(s) failed", fails.len()).dimmed()
                );
                for diagnostic in diagnostics {
                    println!("  {diagnostic}");
                }
                *reason_counts
                    .entry("@test execution failed".to_string())
                    .or_insert(0) += 1;
                if verbose {
                    details.push((label, fails.join("\n")));
                }
            }
            Ok(Err(SkullFailure { reason, rendered })) => {
                failed += 1;
                println!("{:.<60} {}  {}", label, "FAILED".red(), reason.dimmed());
                *reason_counts.entry(reason).or_insert(0) += 1;
                if verbose {
                    details.push((label, rendered));
                }
            }
            Err(panic) => {
                failed += 1;
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                let reason = "bone panic (inference)".to_string();
                println!("{:.<60} {}  {}", label, "PANIC".red().bold(), msg.dimmed());
                *reason_counts.entry(reason).or_insert(0) += 1;
                if verbose {
                    details.push((label, msg));
                }
            }
        }
    }

    std::panic::set_hook(prev_hook);

    if !reason_counts.is_empty() {
        let mut rows: Vec<(String, usize)> = reason_counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        println!("\n{}", "=== failure reasons (skull) ===".bold());
        let shown = rows.len().min(25);
        for (reason, count) in rows.iter().take(shown) {
            println!("  {:>4}  {}", count.to_string().yellow(), reason);
        }
        if rows.len() > shown {
            println!(
                "  {:>4}  ({} more distinct reasons)",
                "...",
                rows.len() - shown
            );
        }
    }

    if verbose && !details.is_empty() {
        println!("\n{}", "=== failure details ===".bold());
        for (file, rendered) in &details {
            println!("\n--- {} ---\n{}", file, rendered);
        }
    } else if failed > 0 || emit_ok > exec_pass {
        println!(
            "\n{}",
            "(re-run with ROCK_TEST_VERBOSE=1 for full per-file errors)".dimmed()
        );
    }

    if let Some(path) = &timings_path {
        write_test_timings(std::path::Path::new(path), &timings)?;
    }
    print_slowest_tests(&timings);

    let total = emit_ok + failed;
    let test_fails = emit_ok - exec_pass;
    let tests_failed = tests_total - tests_passed;
    let filter_summary = filter.map_or_else(String::new, |filter| {
        format!(
            "; filter '{}': {} selected, {} skipped",
            filter.display(),
            tests_total,
            tests_skipped
        )
    });
    crate::invocation_log::note_phase("units", start.elapsed().as_millis() as u64);
    crate::invocation_log::note_phase(
        "tests_run",
        timings.iter().map(|(_, t)| t.ms).sum::<f64>() as u64,
    );
    crate::invocation_log::note_counts(&[
        ("units_selected", total),
        ("units_failed", failed),
        ("units_test_failed", test_fails),
        ("tests_total", tests_total),
        ("tests_passed", tests_passed),
        ("tests_failed", tests_failed),
    ]);
    println!(
        "\nrock test: {}/{} @tests passed ({} failed) across {} files ({:.2}s){}",
        tests_passed.to_string().green(),
        tests_total,
        if tests_failed == 0 {
            tests_failed.to_string().green()
        } else {
            tests_failed.to_string().red()
        },
        total,
        start.elapsed().as_secs_f64(),
        filter_summary,
    );
    println!(
        "  files: {} all-pass, {} emit valid Lua ({} with test failures), {} emit-fail, {} total",
        exec_pass.to_string().green(),
        emit_ok.to_string().green(),
        test_fails.to_string().yellow(),
        if failed == 0 {
            failed.to_string().green()
        } else {
            failed.to_string().red()
        },
        total,
    );
    if !package_dirs.is_empty() {
        println!("  @test shards: {shards} LuaJIT processes per package (ROCK_TEST_SHARDS)");
    }

    // LSP fixtures run through the bone-backed LSP (skull's language server),
    // via the same dispatch helper rock's `test` uses. Failures count toward
    // the non-zero exit alongside @test failures.
    let suites_start = Instant::now();
    let mut lsp_failed = 0usize;
    for path in paths {
        let (_t, _p, f, _i) =
            crate::lsp_test::run_lsp_suite(std::path::Path::new(path), crate::Backend::Skull)?;
        lsp_failed += f;
    }

    // Error-test fixtures (`.rock.err`) verify rejection, which happens in the
    // shared bone front end before codegen -- so skull folds them in too.
    crate::invocation_log::note_phase("lsp_suite", suites_start.elapsed().as_millis() as u64);
    let err_start = Instant::now();
    let mut err_failed = 0usize;
    for path in paths {
        let (_t, _p, f, _i) =
            crate::error_test::run_error_suite(std::path::Path::new(path), verbose)?;
        err_failed += f;
    }

    crate::invocation_log::note_phase("error_suite", err_start.elapsed().as_millis() as u64);
    crate::invocation_log::note_counts(&[("lsp_failed", lsp_failed), ("error_failed", err_failed)]);

    if let Some(filter) = filter.filter(|_| tests_total == 0) {
        anyhow::bail!(
            "rock test filter '{}' matched no @tests (0 selected, {} skipped)",
            filter.display(),
            tests_skipped
        );
    }

    if tests_failed > 0 || test_fails > 0 || failed > 0 || lsp_failed > 0 || err_failed > 0 {
        crate::invocation_log::exit(1);
    }
    Ok(())
}

// luajit run + stdout-normalization helpers, shared by the hot-reload gates below.

/// Per-test execution outcome (clean stdout vs errored), as the gates read it.
#[cfg(test)]
#[path = "skull_backend_tests.rs"]
mod tests;

// Edit helpers for `lsp-survey`.

pub(crate) fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// A real semantic body edit: flip the first single ASCII digit inside the body
/// of `fn <name>` (d -> d-1, '0'->'1'), a genuine 1-char constant change that
/// re-applying the original text reverts. Returns None if the fn or a body
/// digit isn't found.
pub(crate) fn body_digit_flip(text: &str, name: &str) -> Option<String> {
    let fn_pos = text.find(&format!("fn {name}"))?;
    let brace = text[fn_pos..].find('{')? + fn_pos;

    // Bound the body by brace-matching so we only touch THIS function's digits.
    let mut depth = 0i32;
    let mut end = text.len();
    for (i, c) in text[brace..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = brace + i;
                    break;
                }
            }
            _ => {}
        }
    }

    let rel = text[brace..end].find(|c: char| c.is_ascii_digit())?;
    let abs = brace + rel;
    let d = text.as_bytes()[abs];
    let flipped = if d == b'0' { b'1' } else { d - 1 };

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..abs]);
    out.push(flipped as char);
    out.push_str(&text[abs + 1..]);
    Some(out)
}

// ---------------------------------------------------------------------------
// Incremental compiler: two-file output (main.lua + incremental.lua)
// ---------------------------------------------------------------------------

/// Game-shaped `main.lua`: preamble + emit + `__rock_main_wrapper(main)` entry.
///
/// `emits_main` comes from the emitter, not from searching `emit_lua`: the
/// emitted spelling of a definition is the emitter's business and has already
/// changed once (callee binding moves every body into a
/// factory, so `function main(` appears nowhere), which left a chunk that
/// defined the whole game and then ran nothing.
pub(crate) fn skull_game_main<'a>(
    preamble: &'a str,
    program: &'a GameProgram,
    emits_main: bool,
) -> [&'a str; 5] {
    let [head, metadata, tail] = program.parts();
    let entry = if emits_main {
        "\n__rock_main_wrapper(main)\n"
    } else {
        ""
    };
    [preamble, head, metadata, tail, entry]
}

fn write_program_parts(writer: &mut impl std::io::Write, parts: &[&str]) -> std::io::Result<()> {
    for part in parts {
        writer.write_all(part.as_bytes())?;
    }
    Ok(())
}

/// Remove the accumulated overlay. A missing file is the normal "no overlay yet"
/// case and is fine; ANY other error is surfaced -- a stale `incremental.lua`
/// left beside a fresh `main.lua` would reintroduce the overlay/main desync the
/// two-file split exists to avoid.
pub(crate) fn wipe_overlay(incremental_path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(incremental_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "skull: failed to remove stale overlay {}: {e}",
            incremental_path.display()
        )),
    }
}

/// Write the full game-shaped `main.lua` and wipe any accumulated overlay -- the
/// shared tail of every FULL (re)build: `init`, the interface-rebuild branch of
/// `recompile`, and `force_full`. Returns the bytes written (the size the `Full`
/// reload reports).
pub fn write_full_main(
    preamble: &str,
    emit_lua: &GameProgram,
    emits_main: bool,
    main_path: &std::path::Path,
    incremental_path: &std::path::Path,
) -> Result<usize> {
    let main = skull_game_main(preamble, emit_lua, emits_main);
    let bytes = main.iter().map(|part| part.len()).sum();
    rock_host::project::write_atomic_streamed(main_path, |file| write_program_parts(file, &main))?;
    wipe_overlay(incremental_path)?;
    Ok(bytes)
}

/// What an incremental recompile produced. `cook_ms` is the front end (source
/// re-read + per-file incremental cook + re-typecheck via `Session.update_file`);
/// `emit_ms` is codegen + file write (`emit_overlay`/`emit_full`). `bytes` is the
/// written output size -- for `Incremental` that's the overlay size, the headline
/// signal for the overlay-size question.
#[derive(Debug)]
pub enum SkullReload {
    /// Full rebuild after an interface or structural edit.
    Full {
        bytes: usize,
        cook_ms: f64,
        emit_ms: f64,
        write_ms: f64,
    },
    /// Body edit: only `incremental.lua` rewritten (the overlay). `reinferred` is
    /// the number of bodies this edit re-inferred (the edit's churn). `emit_ms` is
    /// pure `emit_overlay`; `write_ms`
    /// is the separate disk write of the overlay.
    Incremental {
        bytes: usize,
        reinferred: usize,
        cook_ms: f64,
        emit_ms: f64,
        write_ms: f64,
    },
    /// A full artifact is required before the current process can observe this
    /// update; no output was written on this path.
    FullRequired { reasons: Vec<String> },
    /// A type's runtime SHAPE changed:
    /// live state cannot be migrated in place. Nothing was written -- the
    /// supervisor must use the restart fallback. `types` names the drifted
    /// types for the
    /// why-log.
    ShapeChanged { types: Vec<String> },
    /// Nothing changed (content-hash no-op).
    NoChange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkullRouteProfile {
    pub session_snapshot_ns: u128,
    pub source_lookup_feed_ns: u128,
    pub session_update_ns: u128,
    pub diagnostics_ns: u128,
    pub lint_total_ns: u128,
    pub local_lint_derivation_ns: u128,
    pub global_lint_dependency_ns: u128,
    pub global_lint_derivation_ns: u128,
    pub suppression_filtering_ns: u128,
    pub suppression_filter_calls: usize,
    pub report_prepare_ns: u128,
    pub summary_sort_ns: u128,
    pub render_ns: u128,
    pub finding_aggregation_ns: u128,
    pub publication_shape_checks_ns: u128,
    pub transaction_checkpoint_ns: u128,
    pub body_transactions_started: usize,
    pub transaction_commit_ns: u128,
    pub affected_chunk_selection_ns: u128,
    pub emit_overlay_ns: u128,
    pub atomic_output_write_ns: u128,
    pub rollback_ns: u128,
    pub live_update_ns: u128,
    pub live_project_call_ns: u128,
    pub route_emit_call_ns: u128,
    pub live_project_unaccounted_ns: u128,
    pub route_unaccounted_ns: u128,
    pub total_ns: u128,
    pub unaccounted_ns: u128,
}

impl SkullRouteProfile {
    fn from_live(timing: crate::live_project::LiveRouteTiming) -> Self {
        Self {
            session_snapshot_ns: timing.session_snapshot_ns,
            source_lookup_feed_ns: timing.source_lookup_feed_ns,
            session_update_ns: timing.session_update_ns,
            diagnostics_ns: timing.diagnostics_ns,
            lint_total_ns: timing.lint_total_ns,
            local_lint_derivation_ns: timing.local_lint_derivation_ns,
            global_lint_dependency_ns: timing.global_lint_dependency_ns,
            global_lint_derivation_ns: timing.global_lint_derivation_ns,
            suppression_filtering_ns: timing.suppression_filtering_ns,
            suppression_filter_calls: timing.suppression_filter_calls,
            report_prepare_ns: timing.report_prepare_ns,
            summary_sort_ns: timing.summary_sort_ns,
            render_ns: timing.render_ns,
            finding_aggregation_ns: timing.finding_aggregation_ns,
            rollback_ns: timing.rollback_ns,
            live_update_ns: timing.live_update_ns,
            ..Self::default()
        }
    }

    fn finish(&mut self, total_ns: u128) {
        self.total_ns = total_ns;
        self.live_project_unaccounted_ns = self
            .live_project_call_ns
            .saturating_sub(self.session_snapshot_ns + self.live_update_ns);
        self.route_unaccounted_ns = self.route_emit_call_ns.saturating_sub(
            self.publication_shape_checks_ns
                + self.transaction_checkpoint_ns
                + self.transaction_commit_ns
                + self.emit_overlay_ns
                + self.atomic_output_write_ns,
        );
        let accounted = self.live_project_call_ns + self.route_emit_call_ns + self.rollback_ns;
        self.unaccounted_ns = total_ns.saturating_sub(accounted);
    }

    fn add_live(&mut self, timing: crate::live_project::LiveRouteTiming) {
        self.session_snapshot_ns += timing.session_snapshot_ns;
        self.source_lookup_feed_ns += timing.source_lookup_feed_ns;
        self.session_update_ns += timing.session_update_ns;
        self.diagnostics_ns += timing.diagnostics_ns;
        self.lint_total_ns += timing.lint_total_ns;
        self.local_lint_derivation_ns += timing.local_lint_derivation_ns;
        self.global_lint_dependency_ns += timing.global_lint_dependency_ns;
        self.global_lint_derivation_ns += timing.global_lint_derivation_ns;
        self.suppression_filtering_ns += timing.suppression_filtering_ns;
        self.suppression_filter_calls += timing.suppression_filter_calls;
        self.report_prepare_ns += timing.report_prepare_ns;
        self.summary_sort_ns += timing.summary_sort_ns;
        self.render_ns += timing.render_ns;
        self.finding_aggregation_ns += timing.finding_aggregation_ns;
        self.rollback_ns += timing.rollback_ns;
        self.live_update_ns += timing.live_update_ns;
    }
}

fn emit_phase_ns(phases: &[(&'static str, Duration)], label: &str) -> u128 {
    phases
        .iter()
        .find(|(name, _)| *name == label)
        .map_or(0, |(_, duration)| duration.as_nanos())
}

/// Persistent incremental compile state: a warm bone `Session` + skull emit cache
/// driving the two-file split. A body
/// edit re-cooks just the edited file and writes a small `incremental.lua` the
/// running game light-reloads; an interface/structural edit rewrites `main.lua`
/// (full) and wipes the overlay.
pub struct IncrementalProject {
    project: crate::live_project::LiveProject,
    cache: skull::mir_emit::SkullEmitCache,
    preamble: String,
    renderer_target: skull::mir_emit::RendererTarget,
    /// Per-type layout hashes of the last program the game process could have
    /// loaded. A full recompile whose hashes drifted returns
    /// `SkullReload::ShapeChanged` (full-reconstruction request) instead of writing.
    shape_hashes: skull::ShapeHashes,
    route_profile_enabled: bool,
    last_route_profile: Option<SkullRouteProfile>,
    artifact_fingerprint: String,
}

impl IncrementalProject {
    /// Initial build: warm the Session, `emit_full` -> game-shaped `main.lua`,
    /// wipe `incremental.lua`. Errors render against the project sources.
    /// Game build: excludes test-only files (tests/ dirs) so a broken sim/test
    /// can't break the running game.
    pub fn init(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
    ) -> Result<IncrementalProject> {
        Self::init_impl(dir, main_path, incremental_path, false, None, |_| Ok(None))
    }

    /// Sim-runner build: same as [`IncrementalProject::init`] but discovery KEEPS
    /// test-only files -- `@sim` bodies live in `tests/sims/` and must be part
    /// of the emitted program.
    pub fn init_with_tests(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
    ) -> Result<IncrementalProject> {
        Self::init_impl(dir, main_path, incremental_path, true, None, |_| Ok(None))
    }

    pub fn init_with_tests_and_allocation_report(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        allocation_manifest: &std::path::Path,
    ) -> Result<IncrementalProject> {
        let _activation = skull::allocation_report::activation_scope(true);
        Self::init_impl(
            dir,
            main_path,
            incremental_path,
            true,
            Some(allocation_manifest),
            |_| Ok(None),
        )
    }

    /// Compile checked sim identities selected by the caller. Selection runs
    /// once on the same workspace used for emission; `None` emits every entry.
    pub fn init_for_sim_selection(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        allocation_manifest: Option<&std::path::Path>,
        select_sims: impl FnOnce(&bone_semantic::inference::Workspace) -> Result<Option<Vec<String>>>,
    ) -> Result<IncrementalProject> {
        Self::init_impl(
            dir,
            main_path,
            incremental_path,
            true,
            allocation_manifest,
            select_sims,
        )
    }

    fn init_impl(
        dir: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        include_tests: bool,
        explicit_allocation_report: Option<&std::path::Path>,
        select_sims: impl FnOnce(&bone_semantic::inference::Workspace) -> Result<Option<Vec<String>>>,
    ) -> Result<IncrementalProject> {
        let _slot = crate::compile_slots::acquire(dir)?;
        let renderer_target = skull::mir_emit::RendererTarget::Gl;
        let project = crate::live_project::LiveProject::init(dir, include_tests)?;
        let (lua, cache, artifact_fingerprint, emits_main) = {
            let ws = project.workspace();
            let mid = skull::user_module_id(ws).map_err(no_user_module)?;
            let selected = select_sims(ws)?;
            let (emit, c) = if let Some(selected) = selected {
                skull::mir_emit::FullEmit::selected_sims(&selected)
                    .project_root(Some(dir))
                    .renderer(renderer_target)
                    .run(ws, mid)
            } else {
                skull::mir_emit::FullEmit::program()
                    .project_root(Some(dir))
                    .renderer(renderer_target)
                    .run(ws, mid)
            }
            .map_err(|e| anyhow::anyhow!("{}", render_project_err(project.sources(), &e)))?;
            let allocation_report_path = allocation_report_path_from_env();
            write_allocation_report(
                explicit_allocation_report
                    .map(std::path::Path::as_os_str)
                    .or(allocation_report_path.as_deref()),
                &emit.allocation_report,
            )?;
            let artifact_fingerprint = emit.artifact_fingerprint.clone();
            let emits_main = emit.emits_main;
            let lua = install_global_accessors(emit.lua, ws)
                .map_err(|e| anyhow::anyhow!(render_project_err(project.sources(), &e)))?;
            (lua, c, artifact_fingerprint, emits_main)
        };
        let preamble = (RuntimeServices::registered().program_preamble)(dir, ProgramKind::Game)?;
        write_full_main(&preamble, &lua, emits_main, main_path, incremental_path)?;
        let shape_hashes = skull::type_shape_hashes(project.workspace())
            .map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(IncrementalProject {
            project,
            cache,
            preamble,
            renderer_target,
            shape_hashes,
            route_profile_enabled: std::env::var_os("ROCK_SKULL_ROUTE_PROFILE").is_some(),
            last_route_profile: None,
            artifact_fingerprint,
        })
    }

    pub fn workspace(&self) -> &bone_semantic::inference::Workspace {
        self.project.workspace()
    }

    pub fn registered_sim_names(&self) -> std::collections::HashSet<String> {
        self.cache.registered_sim_names(self.project.workspace())
    }

    #[cfg(test)]
    pub(crate) fn enable_route_profile(&mut self) {
        self.route_profile_enabled = true;
        self.project.enable_route_profile();
    }

    pub fn take_last_route_profile(&mut self) -> Option<SkullRouteProfile> {
        self.last_route_profile.take()
    }

    fn finish_route_profile(&mut self, started: Option<Instant>, mut profile: SkullRouteProfile) {
        if let Some(started) = started {
            profile.finish(started.elapsed().as_nanos());
            self.last_route_profile = Some(profile);
        } else {
            self.last_route_profile = None;
        }
    }

    pub fn artifact_fingerprint(&self) -> &str {
        &self.artifact_fingerprint
    }

    /// Source texts that produced this compilation.
    pub fn sources(&self) -> &SourceFiles {
        self.project.sources()
    }

    /// The `[build.vars]` selection this build stamped into its workspace.
    pub fn comptime_inputs(&self) -> &bone_semantic::inference::ComptimeInputs {
        &self.project.workspace().comptime_inputs
    }

    /// On a watch event: refresh the cached sources (re-read only `changed` off
    /// disk, or fully re-discover if the file SET changed), `update_file` every
    /// source (the Session content-hashes unchanged ones to a no-op), then route
    /// by the worst tier -- any interface/structural edit -> full `main.lua` +
    /// wipe overlay; otherwise the accumulated body edits -> `incremental.lua`
    /// overlay. `changed` is the watcher's changed-path set; pass empty to force a
    /// full re-discover.
    pub(crate) fn recompile(
        &mut self,
        changed: &[std::path::PathBuf],
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
    ) -> Result<SkullReload> {
        let started = self.route_profile_enabled.then(Instant::now);
        self.last_route_profile = None;
        let project_started = self.route_profile_enabled.then(Instant::now);
        let publication = self.project.recompile_for_publication(changed);
        let project_call_ns = project_started.map_or(0, |started| started.elapsed().as_nanos());
        let (update, snapshot) = match publication {
            Ok(value) => value,
            Err(error) => {
                let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
                profile.live_project_call_ns = project_call_ns;
                self.finish_route_profile(started, profile);
                return Err(error);
            }
        };
        let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
        profile.live_project_call_ns = project_call_ns;
        let route_started = self.route_profile_enabled.then(Instant::now);
        let result = self.route_and_emit(
            update,
            main_path,
            incremental_path,
            self.route_profile_enabled.then_some(&mut profile),
        );
        if let Some(started) = route_started {
            profile.route_emit_call_ns += started.elapsed().as_nanos();
        }
        match result {
            Ok(reload) => {
                self.project.commit_body_transaction();
                self.finish_route_profile(started, profile);
                Ok(reload)
            }
            Err(error) => {
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                let restored = self.project.restore(snapshot);
                let cache_restored = if restored.is_ok() {
                    self.cache.rollback_body_edit(self.project.workspace())
                } else {
                    Ok(())
                };
                profile.add_live(self.project.take_route_timing());
                if let Some(started) = rollback_started {
                    profile.rollback_ns += started.elapsed().as_nanos();
                }
                self.finish_route_profile(started, profile);
                if let Err(restore) = restored {
                    return Err(anyhow::anyhow!(
                        "skull publication rollback failed after disk edit: {restore}; original error: {error}"
                    ));
                }
                if let Err(restore) = cache_restored {
                    return Err(anyhow::anyhow!(
                        "skull cache rollback failed after disk edit: {restore}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// In-memory edit: replace one source's contents WITHOUT touching disk and
    /// route/emit exactly like a watcher edit. The sim reload-injection path:
    /// the runner edits a source in memory
    /// and writes only the overlay; the project tree is never mutated. Rejects
    /// a path the build didn't discover -- an edit to an unknown file would
    /// otherwise silently do nothing.
    pub fn apply_edit(
        &mut self,
        path: &std::path::Path,
        new_text: &str,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
    ) -> Result<SkullReload> {
        let started = self.route_profile_enabled.then(Instant::now);
        self.last_route_profile = None;
        let project_started = self.route_profile_enabled.then(Instant::now);
        let publication = self.project.apply_edit_for_publication(path, new_text);
        let project_call_ns = project_started.map_or(0, |started| started.elapsed().as_nanos());
        let (update, snapshot) = match publication {
            Ok(value) => value,
            Err(error) => {
                let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
                profile.live_project_call_ns = project_call_ns;
                self.finish_route_profile(started, profile);
                return Err(error);
            }
        };
        let mut profile = SkullRouteProfile::from_live(self.project.take_route_timing());
        profile.live_project_call_ns = project_call_ns;
        let route_started = self.route_profile_enabled.then(Instant::now);
        let result = self.route_and_emit(
            update,
            main_path,
            incremental_path,
            self.route_profile_enabled.then_some(&mut profile),
        );
        if let Some(started) = route_started {
            profile.route_emit_call_ns += started.elapsed().as_nanos();
        }
        match result {
            Ok(reload) => {
                self.project.commit_body_transaction();
                self.finish_route_profile(started, profile);
                Ok(reload)
            }
            Err(error) => {
                let rollback_started = self.route_profile_enabled.then(Instant::now);
                let restored = self.project.restore(snapshot);
                let cache_restored = if restored.is_ok() {
                    self.cache.rollback_body_edit(self.project.workspace())
                } else {
                    Ok(())
                };
                profile.add_live(self.project.take_route_timing());
                if let Some(started) = rollback_started {
                    profile.rollback_ns += started.elapsed().as_nanos();
                }
                self.finish_route_profile(started, profile);
                if let Err(restore) = restored {
                    return Err(anyhow::anyhow!(
                        "skull publication rollback failed after in-memory edit: {restore}; original error: {error}"
                    ));
                }
                if let Err(restore) = cache_restored {
                    return Err(anyhow::anyhow!(
                        "skull cache rollback failed after in-memory edit: {restore}; original error: {error}"
                    ));
                }
                Err(error)
            }
        }
    }

    /// The current in-session text of a source (reflects prior `apply_edit`s,
    /// which never touch disk). None if the path isn't a discovered source.
    pub(crate) fn source_text(&self, path: &std::path::Path) -> Option<String> {
        self.project.source_text(path)
    }

    /// Package roots retained by the source discovery used by this session.
    pub fn package_roots(&self) -> &[(u32, std::path::PathBuf)] {
        &self.project.sources().package_roots
    }

    /// Absolute paths and compiled-byte hashes of embedded inputs and resolved
    /// source-fingerprint dependencies in the current program. The engine
    /// watcher treats these files as sources: an edit to one routes as a
    /// changed non-source path, which forces the full rebuild that re-reads it.
    /// The hash lets a watcher seeding the input
    /// compare against what was compiled rather than against the startup
    /// instant.
    pub(crate) fn comptime_watch_files_with_hashes(&self) -> Vec<(std::path::PathBuf, u64)> {
        // File ids are already the absolute canonical path the resolver read.
        let mut files = self
            .cache
            .comptime_file_hashes()
            .into_iter()
            .collect::<rustc_hash::FxHashMap<_, _>>();
        files.extend(
            self.project
                .workspace()
                .comptime_inputs
                .source_fingerprint_files
                .iter()
                .map(|(path, hash)| (*path, *hash)),
        );
        files
            .into_iter()
            .map(|(abs, hash)| (std::path::PathBuf::from(abs.as_str()), hash))
            .collect()
    }

    /// Feed source edits into the Session and route by the worst tier. Body-only
    /// edits write `incremental.lua`; a full artifact is either written or
    /// returned as `FullRequired` when a safety gate defers publication.
    /// Shared by the disk-driven watcher and in-memory sim reload paths.
    /// `t_cook` is the caller's front-end timer (includes its feed construction).
    fn route_and_emit(
        &mut self,
        update: crate::live_project::LiveUpdate,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
        mut profile: Option<&mut SkullRouteProfile>,
    ) -> Result<SkullReload> {
        let (full, reinferred, span_edits, cook_ms) = match update {
            crate::live_project::LiveUpdate::NoChange => return Ok(SkullReload::NoChange),
            crate::live_project::LiveUpdate::BodyOnly {
                reinferred,
                span_edits,
                cook_ms,
            } => (false, reinferred, span_edits, cook_ms),
            crate::live_project::LiveUpdate::Full { cook_ms } => {
                (true, Vec::new(), Vec::new(), cook_ms)
            }
        };
        let t_emit = Instant::now();
        let shape_started = profile.is_some().then(Instant::now);
        let shader_keys_changed = if full {
            self.cache
                .shader_registration_keys_changed(self.project.workspace())
                .map_err(|e| {
                    anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e))
                })?
        } else {
            self.cache
                .shader_registration_keys_changed_for_bodies(self.project.workspace(), &reinferred)
                .map_err(|e| {
                    anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e))
                })?
        };
        if shader_keys_changed {
            if let (Some(started), Some(profile)) = (shape_started, profile.as_deref_mut()) {
                profile.publication_shape_checks_ns += started.elapsed().as_nanos();
            }
            return Ok(SkullReload::FullRequired {
                reasons: vec!["shader registration key set changed".to_string()],
            });
        }
        if full {
            // Schema-drift gate: live
            // data outlives every reload tier, so if any type's runtime layout
            // moved, hot-reloading the recompiled program would drive old-shape
            // state with new-shape readers. Request a complete reconstruction
            // and defer ALL writes until the restart path. Re-baseline the stored hashes now: the
            // delta is consumed by this one full publication.
            let new_shapes = skull::type_shape_hashes(self.project.workspace())
                .map_err(|e| anyhow::anyhow!(e.message))?;
            let drifted = skull::shape_diff(&self.shape_hashes, &new_shapes);
            if !drifted.is_empty() {
                self.shape_hashes = new_shapes;
                return Ok(SkullReload::ShapeChanged { types: drifted });
            }
            let lua = {
                let ws = self.project.workspace();
                let mid = skull::user_module_id(ws).map_err(no_user_module)?;
                let (emit, c) = skull::mir_emit::FullEmit::program()
                    .project_root(Some(self.project.dir()))
                    .renderer(self.renderer_target)
                    .run(ws, mid)
                    .map_err(|e| {
                        anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e))
                    })?;
                if self
                    .cache
                    .global_layout()
                    .is_some_and(|old| c.global_layout().is_some_and(|new| old != new))
                {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec!["global layout changed; restart required".to_string()],
                    });
                }
                if self
                    .cache
                    .lifecycle_plan()
                    .is_some_and(|old| old.removed_gated_wire(&emit.lifecycle_plan))
                {
                    return Ok(SkullReload::FullRequired {
                        reasons: vec![
                            "removing a gated @wire requires a fresh Session/HostProjection"
                                .to_string(),
                        ],
                    });
                }
                let emits_main = emit.emits_main;
                let lua = install_global_accessors(emit.lua, ws)
                    .map_err(|e| anyhow::anyhow!(render_project_err(self.project.sources(), &e)))?;
                ((lua, emits_main), c)
            };
            self.preamble = (RuntimeServices::registered().program_preamble)(
                self.project.dir(),
                ProgramKind::Game,
            )?;
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            let ((lua, emits_main), staged_cache) = lua;
            let bytes = write_full_main(
                &self.preamble,
                &lua,
                emits_main,
                main_path,
                incremental_path,
            )?;
            self.cache = staged_cache;
            self.artifact_fingerprint = self
                .cache
                .artifact_fingerprint()
                .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))?
                .to_string();
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Full {
                bytes,
                cook_ms,
                emit_ms,
                write_ms,
            })
        } else {
            if let (Some(started), Some(profile)) = (shape_started, profile.as_deref_mut()) {
                profile.publication_shape_checks_ns += started.elapsed().as_nanos();
            }
            let checkpoint_started = profile.is_some().then(Instant::now);
            self.cache
                .begin_body_edit_transaction(&reinferred, &span_edits);
            if let Some(profile) = profile.as_deref_mut() {
                profile.body_transactions_started += 1;
            }
            if let (Some(started), Some(profile)) = (checkpoint_started, profile.as_deref_mut()) {
                profile.transaction_checkpoint_ns += started.elapsed().as_nanos();
            }
            let emit_started = profile.is_some().then(Instant::now);
            let overlay = {
                let ws = self.project.workspace();
                skull::mir_emit::emit_overlay_with_span_edits(
                    ws,
                    &mut self.cache,
                    &reinferred,
                    &span_edits,
                )
                .map_err(|e| {
                    anyhow::anyhow!("{}", render_project_err(self.project.sources(), &e))
                })?
            };
            if let (Some(started), Some(profile)) = (emit_started, profile.as_deref_mut()) {
                profile.emit_overlay_ns += started.elapsed().as_nanos();
                profile.affected_chunk_selection_ns +=
                    emit_phase_ns(&overlay.phases, "  affected selection");
            }
            if self
                .cache
                .body_edit_baseline_lifecycle_plan()
                .is_some_and(|old| {
                    old.structure_changed(
                        self.cache
                            .lifecycle_plan()
                            .expect("overlay always carries a LifecyclePlan"),
                    )
                })
            {
                let reason = if self
                    .cache
                    .body_edit_baseline_lifecycle_plan()
                    .is_some_and(|old| {
                        old.removed_gated_wire(
                            self.cache
                                .lifecycle_plan()
                                .expect("overlay always carries a LifecyclePlan"),
                        )
                    }) {
                    "removing a gated @wire requires a fresh Session/HostProjection"
                } else {
                    "LifecyclePlan structure changed; full artifact transition required"
                };
                self.cache.commit_body_edit_transaction();
                return Ok(SkullReload::FullRequired {
                    reasons: vec![reason.to_string()],
                });
            }
            let bytes = overlay.lua.len();
            let emit_ms = t_emit.elapsed().as_secs_f64() * 1000.0;
            let t_write = Instant::now();
            rock_host::project::write_atomic(incremental_path, &overlay.lua)?;
            if let Some(profile) = profile.as_deref_mut() {
                profile.atomic_output_write_ns += t_write.elapsed().as_nanos();
            }
            self.artifact_fingerprint = self
                .cache
                .artifact_fingerprint()
                .ok_or_else(|| anyhow::anyhow!("compiler did not publish an Artifact fingerprint"))?
                .to_string();
            let commit_started = profile.is_some().then(Instant::now);
            self.cache.commit_body_edit_transaction();
            if let (Some(started), Some(profile)) = (commit_started, profile) {
                profile.transaction_commit_ns += started.elapsed().as_nanos();
            }
            let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
            Ok(SkullReload::Incremental {
                bytes,
                reinferred: reinferred.len(),
                cook_ms,
                emit_ms,
                write_ms,
            })
        }
    }

    /// Canonicalize to a single-file program: `emit_full` from the CURRENT
    /// session workspace -> `main.lua`, and wipe `incremental.lua`. Same effect
    /// as the full-rebuild branch of `recompile`, but driven by a caller rather
    /// than an edit tier and with no re-discovery (the session is already
    /// current).
    ///
    /// Used on the engine restart path: a process restart (engine library/compiler
    /// rebuild) re-spawns luajit, which loads `main.lua` only -- it never
    /// dual-loads the accumulated `incremental.lua` the way the live hot-reload
    /// watcher does. Folding the overlay back into `main.lua` before the restart
    /// means the new process gets the complete current program. The overlay
    /// stays a within-process, single-consumer artifact that never has to cross
    /// a process boundary.
    pub(crate) fn force_full(
        &mut self,
        main_path: &std::path::Path,
        incremental_path: &std::path::Path,
    ) -> Result<()> {
        // A recompile that hit a type error bails AFTER `update_file` has already
        // mutated the session into the broken state, so the restart-path fold must
        // re-gate or it would write a partial `main.lua` to the respawned process.
        self.project.verdict("on restart fold")?;
        let (lua, cache, artifact_fingerprint, emits_main) = {
            let ws = self.project.workspace();
            let mid = skull::user_module_id(ws).map_err(no_user_module)?;
            let (emit, c) = skull::mir_emit::FullEmit::program()
                .project_root(Some(self.project.dir()))
                .renderer(self.renderer_target)
                .run(ws, mid)
                .map_err(|e| anyhow::anyhow!("skull force_full emit: {}", e.message))?;
            let artifact_fingerprint = emit.artifact_fingerprint.clone();
            let emits_main = emit.emits_main;
            let lua =
                install_global_accessors(emit.lua, ws).map_err(|e| anyhow::anyhow!(e.message))?;
            (lua, c, artifact_fingerprint, emits_main)
        };
        self.preamble = (RuntimeServices::registered().program_preamble)(
            self.project.dir(),
            ProgramKind::Game,
        )?;
        write_full_main(
            &self.preamble,
            &lua,
            emits_main,
            main_path,
            incremental_path,
        )?;
        self.artifact_fingerprint = artifact_fingerprint;
        self.cache = cache; // resets changed_since_full -- the overlay is now folded in
        // This program is what the (re)spawned process loads -- re-baseline the
        // shape hashes to it, including drift that arrived via a recompile that
        // bailed on a type error after mutating the session.
        self.shape_hashes = skull::type_shape_hashes(self.project.workspace())
            .map_err(|e| anyhow::anyhow!(e.message))?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/stream_write/mod.rs"]
mod stream_write_tests;
