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

The semantic fixture suite can also be narrowed to an existing file:

```sh
cargo test --locked -p bone-semantic --test fixtures or_patterns
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
