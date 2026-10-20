# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`rock.nvim` — a Neovim plugin (Lua) that provides LSP integration and a live debug dashboard for the Rock language server. The server runs as `rock lsp serve` (the `rock` binary — there is no separate `rock-lsp` binary; the crate folded into `rock`). It lives inside the rock monorepo at `editors/nvim/` and is loaded via lazy.nvim `dev = true`.

## Architecture

All modules write into a single observable store (`state.lua`) and call `notify()` to push snapshots to subscribers. The panel and statusline subscribe; floaters read on demand.

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
  capture.lua     -- insert-mode bug-snapshot capture
  ui.lua          -- shared buffer/window helpers
  statusline.lua  -- lualine component
lsp/rock.lua      -- nvim 0.11 auto-discovered LSP default config (fallback)
```

## Key patterns

- **State flow**: all modules call `require('rock.state').set_*()` or `record_*()`; never mutate `state` directly from outside `state.lua`.
- **`request_timings` skips notify** intentionally — per-request re-renders would thrash the panel. The perf floater reads on demand.
- **Binary resolution order** (`binary.lua`): `opts.binary` → `$ROCK_LSP_DIR/target/debug/rock` → cwd-local worktree (`crates/rock-cli/Cargo.toml` find-up → `<wt>/target/debug/rock`) → `$HOME/projects/rock/target/debug/rock`. Shell scripts are peeked for the real binary path (the `/target/*/rock` frontier pattern, never the `rock-fmt` sibling) so `rerock.sh` still gets fs-watched.
- **Restart flow** is in `client.lua`: stop all rock clients → wait → re-call `vim.lsp.enable('rock')` → semantic tokens and inlay hints re-request automatically.
- **Custom LSP requests**: `rock/stats` and `rock/modules` are non-standard — handled in `stats.lua` via `vim.lsp.buf_request`.

## Testing

Headless spec suite under `tests/*_spec.lua`, runnable two ways:

```bash
tests/run.sh           # against host nvim (fast iteration; uses host binary)
tests/docker-run.sh    # clean-room: nvim 0.12.2 + cached rock binary in container
```

`docker-run.sh` builds the `rock` binary once (via `tests/build-rock-lsp.sh`)
and caches it at `~/.cache/rock-lsp/builds/<arch>/<sha>/rock` keyed on a content
hash of rock-cli + rock-lsp + rock + rock-types + Cargo.lock. Subsequent runs
are seconds.

**Spec patterns** (driven by `nvim --headless --clean -l`):
- Stub `vim.notify` and `vim.notify_once` at the top, assert on
  captured messages by substring.
- Specs that need a real LSP key off `$ROCK_LSP_BIN`; if unset they
  print `SKIP` and exit 0 so `run.sh` works without docker.
- Drive *real* code paths. Bypassing gates to make a spec pass means
  it's testing the wrong thing — see the header comment in
  `tests/missing_binary_spec.lua` for why.
- Sentinels for known-but-unfixed bugs go in `*_sentinel_spec.lua`
  with the inversion-comment shape of the existing `*_sentinel_spec.lua` files.

Manual testing still works: open a `.rock` file with the plugin loaded
and use `:RockLspDoctor`. Build the binary with
`cargo build --bin rock` (from repo root) for live use.
