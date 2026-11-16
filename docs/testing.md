# Testing compiler changes

Run commands from the repository root. The default backend requires LuaJIT;
the JavaScript backend requires Node. `cargo run` rebuilds the compiler in
this checkout before running it.

```sh
cargo test --locked -p bone-semantic       # replace with each changed crate
cargo test --locked --no-fail-fast         # workspace default members
cargo test --locked --workspace --no-fail-fast
cargo +1.99.1 fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
```

The full workspace includes the GLSL runner and needs SDL2 development
libraries and `pkg-config`. Native Stone crates run through
`python3 scripts/test_native.py`, which needs `cargo-nextest`. Rust test targets using a shared integration
harness register their modules in the crate's `tests/main.rs`.

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
