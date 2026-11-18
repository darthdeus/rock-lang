# Testing compiler changes

Run commands from the repository root. The default backend requires LuaJIT;
the JavaScript backend requires Node. `cargo run` rebuilds the compiler in
this checkout before running it.

```sh
cargo test --locked -p bone-semantic       # replace with each changed crate
cargo test --locked --no-fail-fast         # workspace default members
cargo test --locked --workspace --exclude stone-rs --exclude stone-gl --exclude stone-capture --exclude stone-imgui-abi
cargo +1.99.1 fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
```

The full workspace includes the GLSL runner and needs SDL2 development
libraries and `pkg-config`. Native Stone crates run through
`python3 scripts/test_native.py`, which needs `cargo-nextest`. Rust test targets using a shared integration
harness register their modules in the crate's `tests/main.rs`.

Keep a separate Cargo target directory for each Git worktree. Sharing targets
can reuse another checkout's dependencies and embedded source paths, making
builds and test results stale.

## Rock tests

```sh
cargo run --quiet --bin rock -- test tests/values/refs.rock
cargo run --quiet --bin rock -- test tests/values/refs.rock --backend js
cargo run --quiet --bin rock -- test tests/ --filter closures/
```

A file contains named `@test` functions; `@test(output = "text")` checks
stdout. `--filter` matches a function or path substring, and comma-separated
patterns select several matches. A filter matching no tests fails; an
unfiltered file without tests can succeed with zero tests. Check the count.
Use `ROCK_TEST_VERBOSE=1` for detailed failures. `rock check` checks a program
without running it.

`@test(backends = "lua")` limits a backend-specific test; accepted names are
`lua`, `js`, and `glsl`. Shared language behavior should run on both LuaJIT
and JS. Tests that JS cannot compile are reported as skips; CI caps their
count with `JS_MAX_SKIPS` in `.github/workflows/ci.yml`.

The runtime corpus lives in `tests/`, grouped by feature. Expected compiler
errors live under `tests/errors/`; editor fixtures live under `tests/lsp/`.
Test labels use only the parent directory and filename, so avoid deeper topic
nesting. A path component named `errors` or `lsp` selects a special runner.

## Differential testing

The CLI integration tests generate function bodies from a small typed grammar.
A typed environment tracks locals through nested blocks, so declarations,
assignments and expressions are well-typed by construction. Random statements
cover value copies and refs, mutable captures, bounded loops, enums with generated payload schemas,
collections, interpolation, coroutines, overloads and generic calls. Each program
prints a deterministic trace frequently. The runner compares LuaJIT and Node
stdout with MIR checks both off and on, and treats compile failures, runtime errors and hangs as failures.
Programs repeat their probes with the JIT enabled to exercise hot code too.
Map lookups use a fixed order. Numeric traces normalize signed zero, which differs
between LuaJIT platforms; signed bitwise inputs stay below the documented
platform-dependent conversion range. Integer leaves stay within signed 32 bits
so generated stores obey the value-struct `int` field range;
uint covers its full unsigned 32-bit range.

```sh
cargo test --locked -p rock-cli --test rock_cli_tests differential::smoke
ROCK_DIFF_SEEDS=5000 cargo test --locked -p rock-cli --test rock_cli_tests differential::campaign -- --ignored --nocapture
ROCK_DIFF_START=81 ROCK_DIFF_SEEDS=1 cargo test --locked -p rock-cli --test rock_cli_tests differential::campaign -- --ignored --nocapture
```

The ordinary cargo test run covers a fixed smoke seed set. Campaigns start at
seed zero by default. A failure reports the seed, source and first differing
trace line; set `ROCK_DIFF_OUTPUT` to a scratch directory to keep the generated
`.rock` files for replay with `rock run`. On failure the runner repeatedly drops
functions and statements, then simplifies expressions while preserving the failure
mode and diagnostic. It prints and saves the reduced source. Set
`ROCK_DIFF_SHRINK_STEPS` (default 150) to change the reduction probe budget. Each
compiler/runtime invocation has a 15-second timeout. Install LuaJIT and Node to
run both backends.

## Expected errors

An `.rock.err` file contains independent invalid programs:

```rock
//=== TEST CASE: mismatch_TypeMismatch ===
//! expect: type mismatch
fn main() { let x: int = "wrong" }
```

Run it with `cargo run --quiet --bin rock -- test cases.rock.err`. Each case
should assert the relevant diagnostic with `//! expect:`; multiple expectations
must all match. Without an expectation, an unrelated rejection can satisfy the
case. The case name suffix is only a label. The error runner scans the selected
file's parent directory, so use a separate directory to run one fixture alone.

## LSP fixtures

Run a single-request `.rock.lsp` fixture with:

```sh
cargo run --quiet --bin rock -- test tests/lsp/goto_definition.rock.lsp
```

For behavior that depends on prior edits, use an `edit_sequence` fixture such
as [`edit_sequence.rock.lsp`](../tests/lsp/edit_sequence.rock.lsp). Query and
protocol tests also live in the `bone-query` and `rock-lsp` crates.

LSP fixture files use TOML. Each `[[tests]]` entry has a `name`, a `kind` label,
source text (or `files` for a multi-file project), a `request`, and generic
`checks`. Requests select CLI or JSON-RPC transport and state the method or
arguments. Checks use JSON paths and predicates such as `contains_all`,
`unordered_equal`, `gte`, `empty`, and `applied_text_equals`. Edit checks apply
the returned changes to the fixture source before comparing the expected text.

Source can be shared from an earlier case with `source = { from_test = "case" }`.
Add `replace = [["old", "new"]]` pairs to create a variant. Edit sequences use
`[[tests.steps]]` entries with `mode = "replace"`, `"type_at"`, or
`"delete_at"`; a later full-document replacement can reuse an earlier step
with `source = { from_step = 1, replace = [["old", "new"]] }`. Checks attached
to a step run after that edit has been sent to the server.

## Stone simulations

A `@sim` runs a Stone game through the real engine loop in a fresh process.
Run the suite with `rock sim <project>` or name individual sims after the
project path. A sim must run a check before `stop_sim()`, unless it explicitly
uses `sim_no_checks("why")`. Sims marked `@sim_env("SIM_MANUAL", "1")` run
only when named. `ROCK_SIM_TIMEOUT_SECS` sets the wall-clock runaway guard.

`SIM_DRAW` selects `none` (default), `commands`, `on_demand`, or
`every_frame`. `@sim_render` selects on-demand drawing. Picture readers request
a draw when needed; raw FFI readers and draw-written globals require
`drawn_frame` or `every_frame`. A reader in update sees the world at that point
in the update. Run records are written to `.sim-runs/` or `ROCK_SIM_LOG_DIR`;
`rock sim-report` reads them.

Hot-reload sims opt in with `@sim_env("STONE_HOT_RELOAD", "1")` and use
`reload_edit(file, find, replace)` to apply a unique source-text match through
the incremental pipeline. The runner restores touched files afterward.

## Compile speed

Compile-speed regressions are caught by measurement, not by tests that count
the compiler's internal work. `perf/compare.py base/rock cand/rock` times
cold compiles of the Stone package, the demos and the test corpus with both
binaries interleaved and prints each workload's cand/base ratio with its
noise; see [`perf/README.md`](../perf/README.md#comparing-two-compilers).
