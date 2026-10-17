# Rock compiler development

`AGENTS.md` and `CLAUDE.md` are the same file.

This repository will be public. Treat every commit, file and commit message
as published the moment it is written.

- Never write private information: home-directory or machine-specific paths,
  host names, email addresses, credentials, or details of games and projects
  that use Rock. Describe a change by what it does to the compiler.
- Install the hook once per clone: `git config core.hooksPath .githooks`.
  It checks staged changes and commit messages; do not bypass it. A private
  deny-list is read from `~/.config/rock-lang/denylist` when present (one
  extended regex per line); it is never committed here.
- Keep history linear. Commit coherent changes with messages that describe
  the change.
- Use ASCII in code. Comment only the non-obvious why. Do not run `cargo fmt`
  over the workspace.
- Behavior changes start with a failing test.
- Run tests for the crates you changed: `cargo test -p <crate>`.
