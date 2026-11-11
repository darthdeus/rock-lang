# Rock compiler development

`AGENTS.md` and `CLAUDE.md` are the same file.

This repository is public. Every commit, file and commit message is
published the moment it is pushed.

Treat this codebase as yours: you will keep building on it, and how it is
built is your call. The author judges outcomes (code quality, tests and
measurements), not implementation choices, so decide those from evidence
without asking. Bring back only product decisions and trade-offs that no
measurement settles.

What the compiler is for, in priority order when goals conflict:

1. Correct output, with checks and tests that would catch a miscompile.
2. Lua that runs well under LuaJIT: steady frame times, few trace aborts, no
   allocation the source did not ask for.
3. Fast compiles from cold, without on-disk caches.
4. Less compiler: prefer deleting work and code to adding machinery.

Back a performance claim with a measurement, and say what was measured and
on what. Runtime is judged with the JIT on. An interpreter measurement can
explain a cause, but it is not the target: it neither justifies a change nor
blocks one.

Write what a later session needs into this file, `docs/` or an issue, not
into an agent's private memory.

- Never write private information: home-directory or machine-specific paths,
  host names, email addresses, credentials, or details of games and other
  private projects that use Rock. The Stone engine is published with Rock,
  so naming it is fine. Describe a change by what it does to the compiler.
- Install the hook once per clone: `git config core.hooksPath .githooks`.
  It checks staged changes and commit messages; do not bypass it. A private
  deny-list is read from `~/.config/rock-lang/denylist` when present (one
  extended regex per line); it is never committed here.
- Keep history linear. Commit coherent changes with messages that describe
  the change.
- Use ASCII in code. Comment only the non-obvious why. Keep Rust formatting
  clean with `cargo +1.99.1 fmt --all`; CI checks the same pinned formatter.
  Keep formatting-only changes separate from semantic changes.
- Behavior changes start with a failing test.
- Run tests for the crates you changed: `cargo test -p <crate>`.
