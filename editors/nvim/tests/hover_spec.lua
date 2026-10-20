-- Hover round-trip against a real rock-lsp.
--
-- Builds on real_lsp_smoke_spec but goes one step further: after
-- attach, send a `textDocument/hover` request through the standard
-- vim.lsp client API and assert a response comes back without error.
-- This proves the full request->response loop through client.lua's
-- wrapped rpc.request dispatcher (which the panel's event ring,
-- per-method timing histograms, and crash detection all rely on).
--
-- We intentionally do NOT assert a specific hover content shape —
-- rock-lsp's hover response format is its own contract, and pinning
-- it from this harness would just produce a brittle test that fails
-- whenever the language server polishes wording. We only require:
--   * the request reached the server (no error)
--   * the response surfaced via the per-method timing ring (proves
--     client.lua's outgoing-request wrapper instrumented it)

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end

vim.notify = function() end
vim.notify_once = function() end
vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  -- rock-lsp 0.1.0 crashes on initialize if stick.toml omits the
  -- version field. Real projects always set it; the docker harness
  -- has to mirror that. See rock_lsp_minimal_project_sentinel_spec.
  f:write('[package]\nname = "hover"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn main() {\n    let x = 42\n}\n'); f:close()
end

local client = require('rock.client')
local state = require('rock.state')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))

local attached_id
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})
if not vim.wait(5000, function() return attached_id ~= nil end, 50) then
  fail('LspAttach did not fire within 5s')
end

local bufnr = vim.api.nvim_get_current_buf()
local done, hover_err, hover_result = false, nil, nil
-- Position cursor on "main" identifier in `fn main()` (line 0, char 3).
local req_ok, _ = vim.lsp.buf_request(bufnr, 'textDocument/hover', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  position = { line = 0, character = 3 },
}, function(err, result)
  hover_err = err
  hover_result = result
  done = true
end)
if not req_ok then fail('vim.lsp.buf_request returned false (no client?)') end

if not vim.wait(3000, function() return done end, 25) then
  fail('hover response did not arrive within 3s')
end

if hover_err then
  fail('hover returned error: ' .. vim.inspect(hover_err))
end
-- A null result is allowed (rock-lsp may legitimately return no
-- hover content for that position) — we only fail on a server error.

-- Verify the timing ring captured this call. record_request_timing is
-- driven from client.lua's wrapped orig_request callback; if the
-- wrapper got dropped, this would be empty.
local samples = (state.get().request_timings['textDocument/hover'] or {}).samples
if not samples or #samples < 1 then
  fail('expected at least one textDocument/hover sample in request_timings; got '
    .. tostring(samples and #samples or 0))
end

print(('OK: hover round-trip completed (latency=%.2fms, result=%s)')
  :format(samples[1], hover_result and 'non-nil' or 'nil'))
os.exit(0)
