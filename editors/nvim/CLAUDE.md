# CLAUDE.md

`rock.nvim` lives in the compiler's repository at `editors/nvim`; the root
`AGENTS.md` governs everything here, including the public-content check that
refuses private project names, machine paths and host names in files and
commit messages.

## What this is

`rock.nvim` — a Neovim plugin (Lua) that provides LSP integration and a live
debug dashboard for the Rock language server. The server runs as `rock lsp
serve` from the `rock` binary (there is no separate `rock-lsp` binary; that
crate folded into the CLI). The plugin is loaded from this checkout via
lazy.nvim `dev = true`.

## Architecture

All modules write into a single observable store (`state.lua`) and call
`notify()` to push snapshots to subscribers. The panel and statusline
subscribe; floaters read on demand.

```
lua/rock/
  init.lua        -- setup(), user commands, keymaps, entry point
  state.lua       -- central store with ring buffers; subscribe/notify pattern
  client.lua      -- wraps vim.lsp.rpc dispatchers to capture traffic; restart/reattach
  binary.lua      -- resolve binary path, stat, sha-id, worktree match, fs-watch
  panel.lua       -- sidebar (right/left), auto-open, 1s ticker
  stats.lua       -- polls rock/stats (5s) and rock/modules (10s)
  log.lua         -- tails vim.lsp.get_log_path(), filters to rock client
  completion.lua  -- floating window for last textDocument/completion response
  perf.lua        -- per-method latency histograms (client-observed)
  modules.lua     -- loaded-modules floater (rock/modules custom request)
  doctor.lua      -- :RockLspDoctor health check floater
  dump.lua        -- :RockDump JSON snapshot writer
  capture.lua     -- bug-snapshot capture (normal/insert/visual)
  colorpick.lua   -- highlight groups + hex colors under the cursor
  semtok.lua      -- :RockSemTokens* semantic-token debugging commands
  highlights.lua  -- identifier-tier highlight links
  completion_log.lua -- opt-in JSONL accept/dismiss log (needs nvim-cmp)
  ui.lua          -- shared buffer/window helpers
  statusline.lua  -- lualine component
lsp/rock.lua      -- nvim 0.11 auto-discovered LSP default config (fallback)
```

## Key patterns

- **State flow**: all modules call `require('rock.state').set_*()` or
  `record_*()`; never mutate `state` directly from outside `state.lua`.
- **`request_timings` skips notify** intentionally — per-request re-renders
  would thrash the panel. The perf floater reads on demand.
- **Binary resolution order** (`binary.lua`): `opts.binary` →
  `$ROCK_LSP_DIR/target/debug/rock` → cwd-local worktree
  (`crates/rock-cli/Cargo.toml` find-up → `<wt>/target/debug/rock`) →
  `$HOME/projects/rock-lang/target/debug/rock` → `rock` on `PATH`. Shell
  scripts are peeked for the real binary path (the `/target/*/rock` frontier
  pattern, never the `rock-fmt` sibling) so a wrapper around a debug build
  still gets fs-watched.
- **Restart flow** is in `client.lua`: stop all rock clients → wait → re-call
  `vim.lsp.enable('rock')` → semantic tokens and inlay hints re-request
  automatically.
- **Custom LSP requests**: `rock/stats` and `rock/modules` are non-standard —
  handled in `stats.lua` via `vim.lsp.buf_request`. Their server side is
  `crates/rock-lsp`.

## Testing

Headless spec suite under `tests/*_spec.lua`, runnable two ways:

```bash
tests/run.sh           # against host nvim (fast iteration; uses host binary)
tests/docker-run.sh    # clean-room: pinned nvim + cached rock binary in a container
```

Specs that need a real server key off `$ROCK_LSP_BIN`; run them against a
rock-lang debug build:

```bash
ROCK_LSP_BIN=$HOME/projects/rock-lang/target/debug/rock tests/run.sh
```

`docker-run.sh` builds the `rock` binary once (via `tests/build-rock-lsp.sh`)
and caches it at `~/.cache/rock-lsp/builds/<arch>/<sha>/rock`, keyed on a
content hash of the workspace sources and lockfile. Subsequent runs are
seconds.

**Spec patterns** (driven by `nvim --headless --clean -l`):
- Stub `vim.notify` and `vim.notify_once` at the top, assert on
  captured messages by substring.
- Specs that need a real LSP key off `$ROCK_LSP_BIN`; if unset they
  print `SKIP` and exit 0 so `run.sh` works without a built server.
- Drive *real* code paths. Bypassing gates to make a spec pass means
  it's testing the wrong thing — see the header comment in
  `tests/missing_binary_spec.lua` for why.
- Sentinels for known-but-unfixed bugs go in `*_sentinel_spec.lua`
  with the inversion-comment shape the existing sentinel files use:
  state the limitation, the fix, and the assertion to flip.

Manual testing still works: open a `.rock` file with the plugin loaded
and use `:RockLspDoctor`. Build the binary with
`cargo build --bin rock` (from the repository root) for live use.
