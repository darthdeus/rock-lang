# Testing compiler changes

Run these commands from the repository root. Rust and Cargo build the compiler;
`luajit` runs the default backend's programs, and `node` runs the JS backend's
programs. No installed `rock` binary is needed.

Use a current stable Rust toolchain and a working C compiler/linker (the CLI
builds bundled mimalloc). SDL2 is not needed for the default build or tests.
CI uses Node 24 and LuaJIT 2.1.1785763465, pinned to commit
`1edc3e52b67eaf6ce5f809be8e17d6862594b8bc`. Older LuaJIT builds can fail numeric
formatting tests because they round exact ties differently. Check `rustc --version`,
`cc --version`, `luajit -v` and `node --version` before diagnosing compiler failures.

## Clippy and Rust formatting

Install the components for the same stable toolchain used to build the compiler:

```sh
rustup component add clippy
cargo clippy --locked --workspace --all-targets -- -D warnings
```

The workspace is Clippy-clean and CI runs this command, so any new warning
fails the build. Fix the cause of a diagnostic rather than adding a blanket
allowance. Crates inherit the shared lint policy through `[lints] workspace = true`.

Rust formatting uses the formatter shipped with Rust 1.99.1, independently of
which stable compiler you use to build. Install it once, then format and check:

```sh
rustup toolchain install 1.99.1 --profile minimal --component rustfmt
cargo +1.99.1 fmt --all
cargo +1.99.1 fmt --all -- --check
```

The entire public Rust workspace, including upstream-derived crates, shares this
baseline. The formatting workflow runs the check on pushes and pull requests;
the full build/test workflow remains manual. Update the formatter version here,
in `AGENTS.md` and in `.github/workflows/format.yml` together, keeping any resulting
formatting sweep separate from semantic changes. The Rust formatter is separate
from `rock fmt`, which formats Rock source files.

The baseline formatting commit is recorded in `.git-blame-ignore-revs`. GitHub
uses this file for blame; enable it for local Git blame with:

```sh
git config blame.ignoreRevsFile .git-blame-ignore-revs
```

## Use the compiler you are editing

```sh
cargo run --quiet --bin rock -- run examples/guide/17_tests.rock
cargo run --quiet --bin rock -- test examples/guide/17_tests.rock
cargo run --quiet --bin rock -- test examples/guide/17_tests.rock --backend js
```

The `--` separates Cargo options from compiler arguments. Cargo rebuilds changed
compiler code before running it. `run` executes `main`; `test` executes the
`@test` functions. The example above prints `3` under `run` and passes two tests
under `test`, on either backend.

For repeated invocations after one build:

```sh
cargo build --locked -p rock-cli
./target/debug/rock test examples/guide/17_tests.rock
```

Rebuild after editing Rust code if you invoke the binary directly. An installed
`rock` on PATH may be a different version from this checkout.

## Runtime cases

Put small named cases in a `.rock` file, for example `cases.rock`:

```rock
@test
fn addition() {
    assert(2 + 3 == 5)
}

@test(output = "7")
fn printed_result() {
    print(3 + 4)
}

fn main() {}
```

```sh
cargo run --quiet --bin rock -- test cases.rock
cargo run --quiet --bin rock -- test cases.rock --backend js
cargo run --quiet --bin rock -- test cases.rock --filter addition
```

Assertions, runtime errors and output mismatches fail the test command with a
nonzero exit status. `--filter` selects by function name or source path substring;
comma-separated patterns select multiple matches. A filter selecting no tests
fails. Without a filter, a file with no `@test` functions can succeed with zero
tests: check the reported count, not only the exit status.

A test that only makes sense on some backends names them with
`@test(backends = "lua")`; the value is a comma-separated list of `lua`, `js`
and `glsl`, and it combines with the other arguments
(`@test(output = "3", backends = "lua")`). Other backends neither compile nor
run the test. Use it only when the test is about the backend itself: LuaJIT
FFI, Lua tables, metatables or GC, the Lua physical layout, or the
`inline(lua)` mechanism. A test that fails on JS because of an emitter gap or a
Lua-only helper with a portable twin stays unmarked, so it keeps showing as a
JS skip. An unknown backend name is a compile error.

For detailed Lua test failures:

```sh
ROCK_TEST_VERBOSE=1 cargo run --quiet --bin rock -- test cases.rock
```

For a quick diagnostic without running the program:

```sh
cargo run --quiet --bin rock -- check cases.rock
```

`check` is useful for inspecting type errors, but does not establish runtime
correctness. Exercise both backends when changing shared lowering behavior.

CI runs the `tests/` corpus on both backends. On JS, a test the backend
cannot compile is reported as skipped, and CI fails if the skip count rises
above `JS_MAX_SKIPS` in `.github/workflows/ci.yml`. When a change removes
skips, lower that number to the new count. Tests marked for other backends
with `@test(backends = ...)` are counted separately (`N skipped, M not for js`
in the summary) and are outside the cap. With `ROCK_TEST_VERBOSE=1`, the JS
run lists each skipped test with its reasons.

## Corpus layout

The `tests/` corpus is grouped into one directory per language topic:
`values/`, `nullable/`, `nobox/`, `control_flow/`, `strings/`, `closures/`,
`types/`, `iterators/`, `functions/`, `operators/`, `arenas/`, `patterns/`,
`structs/`, `vectors/`, `maps/`, `shadowing/`, `newtypes/`,
`native_buffers/`, `json/`, `coroutines/`, `library/` (standard library
helpers), `annotations/`, `syntax/` and `smoke/` (whole-program checks), plus
the optimizer and feature directories `inline/`, `scalarize/`, `macros/`,
`intrinsics/` and `comment_placement/`. `tests/errors/` and `tests/lsp/` hold
the expected-error and LSP suites, `tests/fixtures/` holds data files, and
directories with a `stick.toml` are package tests.

Put a new file in the directory for the feature it exercises. Follow these
rules when adding or renaming files:

- Use one level of nesting under `tests/`. Test labels show only the parent
  directory and the file name, so deeper paths become ambiguous.
- File names must be unique within a directory. Do not repeat the directory
  name as a prefix (`closures/capture_semantics.rock`, not
  `closures/closure_capture_semantics.rock`).
- Never name a topic directory `errors` or `lsp`: those names select the
  expected-error and LSP suites. Avoid `stdlib` as well, since a `stdlib` path
  component changes completion ranking.
- Do not prefix names with a backend (`c_`, `lua_`) or a pipeline stage or
  increment (`anf_`, `phaseN_`, `_incN`), and drop `test_`/`_test`. The corpus
  runs on every backend. Keep a `js_` prefix only on files that pin a
  JS-backend mechanism, such as a runtime helper or the emitted ABI.

`--filter <dir>/` selects one topic:

```sh
cargo run --quiet --bin rock -- test tests/ --filter closures/
```

## Expected compiler errors

Keep intentionally invalid programs separate from runtime cases. Save this as
`cases.rock.err`:

```text
//! Error: invalid types
//=== TEST CASE: mismatch_TypeMismatch ===
//! expect: type mismatch
fn main() { let x: int = "wrong" }
```

```sh
cargo run --quiet --bin rock -- test cases.rock.err
```

Add another `//=== TEST CASE: name_ErrorKind ===` header for each independent
program. The suffix such as `TypeMismatch` is a descriptive label, not an
assertion about the actual error kind. Always use `//! expect: <text>` to pin a
diagnostic substring; multiple `expect` lines must all match. Without them, any
compiler rejection can satisfy the case, including an unrelated syntax error.

The command fails if a case is accepted or the diagnostic lacks an expected
substring. These cases test compiler rejection, not runtime failure. They run
through the shared front end and default lowering/error-lint checks; use the
default backend command above. Their summary appears separately as `Error tests`
after the runtime-test summary, which can show `0/0` for an error-only file.

## Compiler tests and checking the checks

From the repository root, plain `cargo test` selects every workspace member except
`stone-glsl-runner`. This default suite needs LuaJIT and Node, but not SDL2:

```sh
cargo test --locked --no-fail-fast
```

Plain `cargo build` uses the same selection. The optional GPU runner can be built
explicitly with `cargo build --locked -p stone-glsl-runner`.

For the full Rust workspace (including the runner), install SDL2 development libraries and `pkg-config`
(the shader runner uses them to discover SDL2), plus LuaJIT and Node on `PATH`:

```sh
# macOS (Homebrew)
brew install sdl2 pkg-config
# Debian/Ubuntu
sudo apt-get install libsdl2-dev pkg-config

cargo test --locked --workspace --no-fail-fast
```

Run the Rust suites for every crate changed, for example:

```sh
cargo test --locked -p bone-semantic -p bone-inference -p skull-mir
```

The semantic fixture suite runs each `tests/fixtures/*.rock` file as its own
test through a small libtest-style harness (`tests/fixture_harness.rs`), and can
be narrowed to an existing file:

```sh
cargo test --locked -p bone-semantic --test fixtures or_patterns
```

The `bone-incremental`, `bone-lints`, `bone-query`, `bone-semantic`, `rock-cli`,
`skull`, `skull-codegen-glsl`, `skull-codegen-js`, `skull-fmt`, `skull-mir` and
`skull-transform` integration suites share a harness per suite instead of
linking the compiler into a binary for every file. Register new
integration-test modules in their `tests/main.rs`; automatic test-file discovery
is disabled for these crates. Targets that need their own process or harness,
such as `bone-semantic`'s `fixtures` and `rock-cli`'s `child_lifetime`, stay
listed separately in the crate's `Cargo.toml`. Filter by module or test name, for
example:

```sh
cargo test --locked -p bone-lints unused_var::
cargo test --locked -p bone-semantic --test bone_semantic_tests structural::
```

For new coverage, observe failure before implementing the change where possible.
Check a runtime test by temporarily making its expected result wrong. Check an
error case by replacing the invalid code with valid code, and separately by
changing the expected diagnostic to text that cannot match. Both mutations
should fail; restore the original cases afterward.

When comparing two implementations, use small inputs with independently known
answers. Agreement between backends alone is not enough: both may share the
same lowering bug.

## Terminal-dependent output

Text assertions must work both in a terminal and with output redirected. CI also
runs the diagnostic, CLI and logging tests with colors forced on:

```sh
ROCK_FORCE_COLOR=1 ROCK_LOG_COLOR=always CLICOLOR_FORCE=1 FORCE_COLOR=1 \
  cargo test --locked -p rock-core -p rock-cli -p rock-log
```

Use `test_utils::strip_ansi_sgr` when an assertion checks diagnostic text or
layout rather than styling. For Node subprocesses whose numeric stdout is test
data, remove `FORCE_COLOR` from the child environment; `NO_COLOR` alone does not
override it. Do not change process-wide environment variables in parallel tests.

## LSP fixtures

The `tests/lsp/` corpus contains `.rock.lsp` fixtures. A single-request fixture
specifies source, an operation and position, and expected results. For example,
[`goto_definition.rock.lsp`](../tests/lsp/goto_definition.rock.lsp) checks a
returned file and position. Run a file with the compiler being edited:

```sh
cargo run --quiet --bin rock -- test tests/lsp/goto_definition.rock.lsp
```

A single-request fixture starts a fresh process. For an editor bug that depends
on prior edits, use an `edit_sequence` fixture such as
[`edit_sequence.rock.lsp`](../tests/lsp/edit_sequence.rock.lsp), which sends
changes to one persistent server. In-process query and protocol tests also live
in the `bone-query` and `rock-lsp` crates:

```sh
cargo test --locked -p bone-query
cargo test --locked -p rock-lsp
```

Check the fixture's reported test count and assert the response content, not
just that the request succeeded. The parser accepts only specific expectation
forms for each operation and can silently skip unrecognized lines; make a valid
expectation fail temporarily to verify that it is actually checked. The fixture parser and validators are in
[`lsp_test.rs`](../crates/rock-cli/src/lsp_test.rs).
