# rock.nvim

Neovim integration and live debug dashboard for the Rock language server.
It is part of the [rock-lang](https://github.com/darthdeus/rock-lang)
repository; load it from a checkout with lazy.nvim's `dev = true` so you
iterate on it alongside the compiler and the server.

## What it gives you

- **Sidebar panel** (right side, auto-opens on first `.rock` buffer) with
  live binary info, client state, per-buffer attachment, `rock/stats`
  snapshot, last-completion summary, a tail of the LSP log, and a
  newest-first event ring of every JSON-RPC frame in both directions.
- **Worktree/binary mismatch detection** — auto-picks the `target/debug/rock`
  that lives in the cwd's worktree when `ROCK_LSP_DIR` is unset, and
  warns when the active binary comes from a *different* worktree.
- **Binary fs-watch** — panel id flips the second a `cargo build`
  finishes in another terminal.
- **Restart with proper reattach** — semantic tokens and inlay hints
  come back on their own after `<Tab>` / `:RockRestart`.
- **Crash surfacing** — non-zero exit paints a virtual-line diagnostic
  in every open `.rock` buffer plus a toast. Missing-binary surfaces
  instantly instead of hanging at "starting".
- **Completion debug floater** — full dump of the raw
  `textDocument/completion` response (label, kind, detail, latency,
  trigger, item count) so you can distinguish server bugs from cmp-side
  filtering issues.
- **Per-method latency floater** with ASCII histograms, min/mean/p50/p95/p99/max
  computed client-side from every JSON-RPC round-trip. Spot slow hovers,
  completion tail latency, inlay-hint spikes at a glance.
- **Loaded modules floater** fetched via a custom `rock/modules` server
  request, listing every compiled module per context with its last
  compile duration, how recently it was compiled, and stale/stdlib/builtin
  flags.
- **Status-line component** for lualine.

## Install (lazy.nvim)

The plugin drives the server through `rock lsp serve`, so it needs a `rock`
build that carries the language server. Build one from your rock-lang
checkout:

```sh
git clone git@github.com:darthdeus/rock-lang.git ~/projects/rock-lang
cd ~/projects/rock-lang && cargo build --bin rock
```

```lua
{
  dir = vim.fn.expand('~/projects/rock-lang/editors/nvim'),
  name = 'rock.nvim',
  ft = 'rock',
  config = function()
    require('rock').setup({
      -- binary = '/path/to/rock',  -- optional; see Binary resolution
      capabilities = require('cmp_nvim_lsp').default_capabilities(),
      on_attach = function(client, bufnr)
        -- your keymaps, e.g. buf_set_keymap(bufnr, 'n', 'gd', ...)
      end,
      -- toggle_key     = '<leader>rp',  -- panel toggle
      -- completion_key = '<leader>rc',  -- completion floater toggle
      -- panel_side     = 'right',       -- 'left' | 'right'
    })
  end,
}
```

**Remove** any existing inline `vim.lsp.config('rock', ...)` /
`lspconfig.rock.setup` in your dotfiles — this plugin owns that now.

## Tests

The suite is `tests/*_spec.lua`, each one a headless nvim process driven
by `tests/run.sh`:

```sh
tests/run.sh        # against the host nvim
```

Specs that need a real server key off `$ROCK_LSP_BIN` and print `SKIP`
(often `SKIP: $ROCK_LSP_BIN not set`) when it is unset. To run them:

```sh
ROCK_LSP_BIN=~/projects/rock-lang/target/debug/rock tests/run.sh
```

`tests/docker-run.sh` is the clean-room variant: it builds
`Dockerfile` (nvim pinned to the version in `M.min_nvim`), builds or
reuses a cached `rock` via `tests/build-rock-lsp.sh`, and runs the same
runner inside the container.

`scripts/semtok_caps.lua` answers capability questions by observation: it
drives a real server through the plugin and prints what nvim declares for
semantic tokens and which request kinds it actually sent:

```sh
ROCK_LSP_BIN=~/projects/rock-lang/target/debug/rock \
  nvim --headless --clean -u NONE -l scripts/semtok_caps.lua
```

## Licence

MIT OR Apache-2.0, the same as the rest of the rock-lang repository
(`LICENSE-MIT`, `LICENSE-APACHE`).

## Backend

The server runs the **bone** typechecker (the skull/bone LSP) — the only
backend. It serves hover, completion, goto-definition, references, rename,
signature help, inlay hints, semantic tokens, and document/workspace symbols.
Workspace symbol queries match the symbol's own name (case-insensitive
substring), or its qualified name when the query contains `::`; an empty query
returns everything. Test functions are listed after other symbols.

## User commands

| Command | Action |
|---------|--------|
| `:RockPanel` | toggle the sidebar panel |
| `:RockPanel open\|close\|toggle` | explicit state |
| `:RockPanel left\|right` | force side and open |
| `:RockCompletion` | toggle the completion-debug floating window |
| `:RockPerf` | toggle the per-method latency floater (histograms + percentiles) |
| `:RockModules` | toggle the loaded-modules floater (refetches on open) |
| `:RockLspDoctor` | health check (binary, worktree match, client, capabilities, log, rock/stats + rock/modules probes) |
| `:RockColorPick` | highlight groups and hex colors under the cursor |
| `:RockCapture [note]` | write a bug snapshot (buffer, position, LSP state) for a report |
| `:RockSemTokens` / `:RockSemTokensAll` | dump the semantic tokens Neovim holds for the current line / whole buffer |
| `:RockSemTokensDebug` / `:RockSemTokensHook` / `:RockSemTokensTrace` | inspect, hook, and trace the semantic-token highlighter |
| `:RockCompletionLog [on\|off\|toggle]` | runtime switch for the JSONL completion log |
| `:RockDump [path]` | write a JSON snapshot of state + events + timings to `/tmp/rock-nvim-<ts>.json` (or given path) |
| `:RockRestart` | stop+reattach rock clients (also `<Tab>`, `<F3>`) |

## Keybinds

Global (all configurable via `setup()`):

| Key | Action |
|-----|--------|
| `<leader>rp` | toggle panel |
| `<leader>rc` | toggle completion-debug floater |
| `<leader>rt` | toggle per-method latency floater |
| `<leader>rm` | toggle loaded-modules floater |
| `<leader>rk` | color pick under cursor |
| `<C-g>c` | capture a bug snapshot (insert mode too) |
| `<Tab>` / `<F3>` | restart LSP (expected to be wired in your dotfiles to call `require('rock').restart()`) |

Inside the sidebar panel:

| Key | Action |
|-----|--------|
| `q` | close panel |
| `r` | restart LSP |
| `c` | clear the event ring |
| `e` / `<CR>` | expand event under cursor (full JSON in a floating window) |
| `s` | swap panel side (left ↔ right) |
| `E` | clear the silent-error ring |
| `f` | cycle event filter (all → no-progress → requests-only → notifications-only) |
| `/` | prompt for event method substring filter (empty clears) |
| `?` | help |

Inside the completion floater:

| Key | Action |
|-----|--------|
| `q` / `<Esc>` | close |

Inside the perf floater (`:RockPerf`):

| Key | Action |
|-----|--------|
| `q` / `<Esc>` | close |
| `r` | reset all sample buffers |

Inside the modules floater (`:RockModules`):

| Key | Action |
|-----|--------|
| `q` / `<Esc>` | close |
| `r` | refetch from server |

## Status line

```lua
-- lualine section_x
{ require('rock.statusline').component,
  cond = function() return vim.bo.filetype == 'rock' end }
```

Renders `●rock 8edb6ccf up 12m r2`:
- `●` green = attached, `●` yellow = starting, `.` grey = detached, `x` red = crashed
- `8edb6ccf` = binary id (flips on rebuild)
- `!` appended if there's a worktree mismatch
- `up Xs/m/h` = client uptime
- `rN` = total restart count this session

## Binary resolution

In priority order:

1. `opts.binary` passed to `setup()`.
2. `$ROCK_LSP_DIR/target/debug/rock` (if env var set).
3. The current worktree's `target/debug/rock`, found by walking up
   from `cwd` looking for a `crates/rock-cli/Cargo.toml`.
4. `$HOME/projects/rock-lang/target/debug/rock` (the dev checkout).
5. `rock` on `PATH`, so an installed compiler works with no checkout.

If the resolved path is a shell script, we peek inside for a line
matching `*/target/*/rock` and use that instead, so a wrapper around a
debug build still gets fs-watched as a real binary.

## Troubleshooting

- **"binary does not exist"** — either your worktree hasn't built
  rock yet (`cargo build --bin rock` in a rock-lang checkout) or the
  resolution picked a stale path. Check the BINARY section; the
  `configured_path` in `state.binary.configured_path` tells you what was
  asked for.
- **"MISMATCH"** — you're editing files in one worktree but the active
  binary is from another. Either `export ROCK_LSP_DIR=$(pwd)` and
  restart nvim, or delete `~/projects/rock-lang/target/debug/rock` so
  resolution falls through to the cwd-local build.
- **Completion floater shows stale data** — it shows the last
  completion captured; trigger a new one in a rock buffer and re-open.
- **Events section empty** — client is still starting. If it stays
  empty after 2s, check the `CLIENT` state line; `crashed` means check
  `:LspLog` (or wait for log-tail to pick it up).
