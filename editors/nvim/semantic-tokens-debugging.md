# Semantic tokens debugging

Tools and lessons from chasing a "semantic tokens appear correct in the CLI
but Neovim shows no highlighting" bug. Keep this file alive — the same
symptoms have several wildly different causes, and the debug commands
below resolve each of them fast.

## Symptoms this chapter covers

- Fields of the same type render in different colors.
- Type names like `IVec2` split into `IVec` + `2`, where `2` highlights as
  a number.
- `rock lsp semantic-tokens some.rock` shows correct tokens
  but the same tokens aren't visible in Neovim.
- `vim.lsp.semantic_tokens.get_at_pos(0, row, col)` returns `{}` everywhere
  even though the server returned a non-empty response.

## Commands

All live in `lua/rock/semtok.lua`, registered by `require('rock').setup()`.

| Command | Purpose |
|---------|---------|
| `:RockSemTokens` | Dump LSP tokens for the current line (or visual range). Shows source lines, decoded tokens from Neovim's cache, and "uncovered" positions with their syntax/treesitter captures. |
| `:RockSemTokensAll` | Same, whole buffer (slow on huge files). |
| `:RockSemTokensDebug` | Inspect `__STHighlighter` internal state: is the highlighter attached, how many highlights are cached per client, what's in `current_result` / `active_request`, any extmarks in LSP-ish namespaces. |
| `:RockSemTokensHook` | Monkey-patch `STHighlighter.process_response` with an ENTER/EXIT trace so we can see what Neovim does with each response. Persists until nvim restart. |
| `:RockSemTokensTrace` | Show the trace ring populated by `:RockSemTokensHook`. |

Typical investigation flow:

1. `:RockSemTokens` on a suspicious line — compare against
   `rock lsp semantic-tokens path/to/file.rock`.
2. If Neovim's tokens section is empty but the CLI's isn't, run
   `:RockSemTokensDebug`. Empty `current_result` means the decoder isn't
   storing the response.
3. `:RockSemTokensHook`, `:lua vim.lsp.semantic_tokens.force_refresh(0)`,
   wait 1-2s, `:RockSemTokensTrace`. The ENTER line shows what args
   `process_response` received — most importantly `req=…` (the
   `ctx.request_id`) and `data_len=…`.

## Known failure modes

### Stale treesitter parser hijacks the buffer

Symptom: `:RockSemTokens` uncovered-positions section shows
`{ts:keyword, ts:variable.field}` on every letter; type names split mid-word
(`IVec` + `2`); fields get random colors independent of type.

Cause: a `parser/rock.so` left over from an earlier treesitter experiment
sits on the runtime path and claims the buffer. nvim-treesitter's
`highlight.enable = true` turns it on globally even without an
`ensure_installed` entry. A broken parser's highlight queries often degenerate
to `@keyword` on everything.

Fix: delete the parser and any `queries/rock/*.scm` in the config dir,
then add a belt-and-suspenders disable in your treesitter setup:

```lua
require('nvim-treesitter.configs').setup {
  highlight = {
    enable = true,
    disable = { "rock" },  -- rock has its own LSP tokens + syntax file
  },
  ...
}
```

### RPC callback signature regression (the big one)

Symptom: CLI returns correct tokens, the LSP panel shows a valid response
with `data: <table n=…>`, but `:RockSemTokensDebug` reports
`current_result = {}` and `active_request.request_id` stays non-nil
forever. `:RockSemTokensTrace` shows `process_response ENTER … req=nil …`.

Cause: a custom `cmd` function wraps `vim.lsp.rpc.start()` for telemetry
but forwards the wrong arity when re-dispatching. Neovim's rpc-level
callback signature is `(err, result, message_id)` — **three arguments**,
per `runtime/lua/vim/lsp/rpc.lua`. The third argument becomes
`ctx.request_id` in the higher Client layer
(`runtime/lua/vim/lsp/client.lua:741`). Semantic tokens' stale-response
check compares `ctx.request_id` against `active_request.request_id`; when
the first is `nil` it always treats the response as stale and returns
early. `current_result` never populates.

**This is the easiest bug to re-introduce.** Any wrapper around
`rpc.request` must forward all three callback args:

```lua
rpc.request = function(method, params, cb, notify_reply)
  return orig_request(method, params, function(err, result, message_id)
    -- ... telemetry ...
    if cb then return cb(err, result, message_id) end  -- ← forward ALL args
  end, notify_reply)
end
```

Most other LSP features (hover, completion, diagnostics) don't check
`request_id` and keep working, which is why the bug hides until you
need semantic tokens.

### Decode coroutine abandoned mid-flight

If `:RockSemTokensTrace` shows `ENTER` without a matching `EXIT`, Neovim's
`tokens_to_ranges` coroutine is yielding and never resuming. On resume,
it checks `version == util.buf_versions[bufnr] and request_id ==
request.request_id`; if either fails the coroutine is dropped silently.
Possible causes: overlapping requests (rapid edit + refresh cycle), a
client implementation that cancels before the decoder catches up, or
buffer version tracking being out of sync.

Hasn't been seen in practice once the RPC-callback issue above was fixed,
but the tracing is there if it reappears.

## Architecture notes

- Priority order Neovim applies: `@lsp.type.<type>.<lang>` (hl_group
  with language suffix), then `@lsp.type.<type>`, then whatever
  treesitter / vim syntax produces. So a correctly-sent token beats
  syntax fallback — but **if the token isn't cached, syntax wins silently**.
- The server's semantic tokens legend includes duplicate `"keyword"`
  entries in some builds — harmless (clients key by index), but worth
  deduping when touching `get_semantic_tokens_legend`.
- Neovim 0.11+ prefers `vim.lsp.config()` + `vim.lsp.enable()` over
  `nvim-lspconfig`. This plugin uses the native path; `nvim-lspconfig`
  wouldn't have prevented the RPC-wrap bug, it just wouldn't have had
  the wrap at all (and we'd lose the panel telemetry).
