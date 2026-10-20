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
`tests/Dockerfile` (nvim pinned to the version in `M.min_nvim`), builds or
reuses a cached `rock` via `tests/build-rock-lsp.sh`, and runs the same
runner inside the container.

## Licence

MIT OR Apache-2.0, the same as the rest of the rock-lang repository
(`LICENSE-MIT`, `LICENSE-APACHE`).

## Backend

The server runs the **bone** typechecker (the skull/bone LSP) — the only
backend. It serves hover, completion, goto-definition, references, rename,
signature help, inlay hints, semantic tokens, and document/workspace symbols.

When semantic token highlighting misbehaves (fields wrong colors, type
names split, no highlighting despite valid server output), see
[`semantic-tokens-debugging.md`](semantic-tokens-debugging.md) —
it documents the `:RockSemTokens*` commands and the known failure modes.

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
| `:RockSemTokensDebug` / `:RockSemTokensHook` / `:RockSemTokensTrace` | semantic-token highlighter internals (see `semantic-tokens-debugging.md`) |
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

## Panel sections

From top to bottom:

```
rock.nvim               [s] swap side [q] close [?] help

BINARY
  <path>                      -- resolved absolute path
  mtime HH:MM:SS (rel)  size  -- latest stat
  id <sha8>                   -- flips on rebuild
  MISMATCH                    -- present only when worktree != binary's
    cwd expects: <worktree>
    binary from: <worktree>

CLIENT
  state: attached  up 12m  restarts 2
  id 3
  last exit: code=1 sig=0 (3s ago)   -- only after a crash

BUFFERS (N)
  * 1   main.rock                    -- * = attached, . = detached

PERF  (<leader>rt for detail)        -- compact; full view in :RockPerf
  method                n   p50   p95  hist
  td/completion         8  11.1  11.2  ▂        █
  ...

MODULES  (N total, M contexts)       -- polled every 10s via rock/modules
  [script]    2  main.rock
     . main.rock                     540ms ago
     . unified.rock                  2.9s ago  STALE
  ...

STATS (rock/stats)                   -- polled every 5s
  avg_compile_ms        9.9
  document_versions     3
  incremental_hit_rate  0.66
  inlay_hint_cache      3
  last_compile_ms       2.1
  last_modules          40
  last_sources          5
  last_stale_modules    2
  lib_files             0
  projects              1
  script_files          0
  semantic_token_cache  3
  total_compiles        47

LAST COMPLETION  (<leader>rc for detail)
  14:22:03  items=87  4.2ms  trig='.'
    1. foo
    2. foo_bar
    ...

LOG (N, tail)                        -- filtered lines from :LspLog
  [INFO]... "LSP[rock]" ...

EVENTS (N, newest first)
  14:22:03 <-resp  textDocument/comp {"result"
  14:22:03 ->req   textDocument/comp {"req_id"
  ...
```

## `rock/stats` fields

The server answers a custom `rock/stats` request (implemented in
`crates/rock-lsp/src/lib.rs`) with counts and compile metrics:

| Field | Meaning |
|-------|---------|
| `projects` | Number of `ProjectContext`s (one per `stick.toml` workspace). |
| `script_files` | Files with a `#script` pragma, each in their own context. |
| `lib_files` | `//! love-lib:`-tagged contexts. |
| `document_versions` | Buffers the LSP is version-tracking. |
| `semantic_token_cache` | Entries in the semantic-token cache. |
| `inlay_hint_cache` | Entries in the inlay-hint cache. |
| `total_compiles` | Cumulative compile count for this process. |
| `last_compile_ms` | Wall-time of the most recent compile. |
| `avg_compile_ms` | `total_compile_us / total_compiles`. |
| `last_stale_modules` | Modules marked stale on the last compile. |
| `last_sources` | Source files in the last `CompilationRequest`. |
| `last_modules` | `storage.modules.len()` after the last compile. |
| `incremental_hit_rate` | `1 - last_stale_modules / last_sources`. |
| `method_counts` | Per-LSP-method request count (server-side). Nested map; rendered in the panel as `tostring()` so you'll see a compact inline summary. Full breakdown via `:RockPerf`. |
| `process_rss_mb` | Resident set size of the rock-lsp process in megabytes (macOS: `task_info`; Linux: `/proc/self/status` VmRSS). Useful for spotting memory leaks across long sessions. |
| `git_sha` | Git commit SHA baked into the binary at compile time (8-char short SHA). Cross-check with the binary `id` in the BINARY section to confirm what you're running. |

Extend by adding fields in `LspWorkspace::stats_snapshot()` — the panel
renders any new key automatically (keys are sorted alphabetically).

## `rock/modules`

Custom request returning every module currently known to the server,
grouped by context:

```json
{
  "total_modules": 214,
  "contexts": [
    {
      "kind": "project",
      "root": "/path/to/workspace",
      "module_count": 42,
      "modules": [
        { "id": ".../main.rock", "last_compile_us": 1234,
          "ago_ms": 540, "stale": false,
          "is_stdlib": false, "is_builtin": false }
      ]
    }
  ]
}
```

`kind` is one of `project` (stick.toml workspace), `script` (`#script`
pragma), or `lib` (`//! love-lib:` tag). `last_compile_us` comes from
`CompilationStorage::module_compile_times`; `ago_ms` is
`Instant::now() - metadata.last_compiled`.

## Per-method latency (`:RockPerf`)

Client-side. Measures request send → response receive via the wrapped
`vim.lsp.rpc` transport. Every JSON-RPC request is sampled.

```
 PER-METHOD LATENCY (client-observed, ms)

 method                             n    min   mean    p50    p95    p99    max  hist
 -------------------------------------------------------------------------------------
 textDocument/completion           20   8.26  11.54  11.98  17.10  17.15  17.15  █▆ ▂ ...
 textDocument/inlayHint             3  11.05  11.63  11.83  11.99  11.99  11.99  █     ...
 rock/stats                         2   1.41   2.16   1.41   2.92   2.92   2.92  █     ...
```

Samples are capped at 500 per method (FIFO). `r` inside the floater
resets all samples. Sorted by sample count descending.

This is the client-observed number — what nvim (and your user
experience) actually sees, including transport and nvim-side
handler time. Compare with server `last_compile_ms` to see where time
is being spent.

## Completion debug

Triggered by `<leader>rc` or `:RockCompletion`. Shows:

```
 LAST COMPLETION  14:22:03
   location : main.rock:42:15
   trigger  : '.'   kind=TriggerChar
   latency  : 4.67ms   items=87  (isIncomplete)

 LABEL                            KIND         DETAIL
 ------------------------------------------------------------
 foo                              Function     fn(x: int) -> str
 foo_bar                          Function     fn() -> bool
 ...
```

Capped at 500 stored items and 200 rendered. Data source is the
wrapped `vim.lsp.rpc` layer in `lua/rock/client.lua` — no cmp cooperation
needed. Use this when cmp shows something weird: the floater is
ground truth for what the server sent.

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

## Architecture

```
lsp/rock.lua                -- nvim 0.11 auto-discovered default (fallback)
lua/rock/
  init.lua                  -- public API, setup(), user commands, keymaps
  state.lua                 -- central observable store; ring buffers
  binary.lua                -- resolve, stat, sha-id, worktree match, fs-watch
  client.lua                -- wraps vim.lsp.rpc.start dispatchers for
                            --   traffic capture; restart + reattach;
                            --   on_exit crash marks
  panel.lua                 -- right/left sidebar, auto-open, timer refresh
  completion.lua            -- floating window for last completion
  perf.lua                  -- per-method latency stats + histograms
  modules.lua               -- loaded-modules floater (rock/modules fetch)
  doctor.lua                -- :RockLspDoctor health check floater
  dump.lua                  -- :RockDump JSON snapshot writer
  ui.lua                    -- shared helpers (ensure_named_buffer, etc)
  log.lua                   -- tails vim.lsp.get_log_path(), filters to rock
  stats.lua                 -- polls rock/stats (5s) + rock/modules (10s)
  statusline.lua            -- lualine component
```

Every module feeds `state.lua`. The panel subscribes and re-renders on
any state change plus a 1-second wall-clock ticker (for the `up` field).

## Known followups

- **Completion side-by-side** — compare raw LSP items vs. what nvim-cmp
  ultimately displayed. Needs a cmp source wrapper.
- **Module load/unload timeline** — currently we show a point-in-time
  snapshot. A true timeline would track load/unload events over time.
- **Event filter keybinds** inside the panel (`f` to toggle `$/progress`).
- **Persisted panel side** across sessions (currently per-session).

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
