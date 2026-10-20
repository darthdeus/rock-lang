-- Real-LSP completion request. Beyond verifying a response comes
-- back, this drives the `is_completion` branch in client.lua's
-- wrapped rpc.request — the one that populates state.last_completion
-- (the floater's data source).

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
  f:write('[package]\nname = "comp"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  -- Body with an identifier prefix to complete from.
  f:write('fn main() {\n    let xyz = 1\n    let abc = x\n}\n'); f:close()
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
  fail('LspAttach did not fire')
end
-- Settle so the server has indexed the file.
vim.wait(500, function() return false end)

local bufnr = vim.api.nvim_get_current_buf()
local done, err = false, nil
vim.lsp.buf_request(bufnr, 'textDocument/completion', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  -- Position is end of "let abc = x" on line 2 (0-indexed). Char 15
  -- is just past the "x" identifier.
  position = { line = 2, character = 15 },
  context = { triggerKind = 1 },
}, function(e, _) err = e; done = true end)
if not vim.wait(3000, function() return done end, 25) then
  fail('completion did not respond within 3s')
end
if err then fail('completion returned error: ' .. vim.inspect(err)) end

-- The wrapped rpc.request in client.lua has an `is_completion` branch
-- that records a snapshot into state.last_completion. Verify it.
local lc = state.get().last_completion
if not lc then
  fail('state.last_completion was not populated by completion handler')
end
if type(lc.item_count) ~= 'number' then
  fail('state.last_completion.item_count is not a number: ' .. vim.inspect(lc))
end
if type(lc.latency_ms) ~= 'number' then
  fail('state.last_completion.latency_ms is not a number')
end
if lc.uri ~= vim.uri_from_bufnr(bufnr) then
  fail(('state.last_completion.uri mismatch: got %s expected %s')
    :format(tostring(lc.uri), vim.uri_from_bufnr(bufnr)))
end

print(('OK: completion captured (items=%d, latency=%.2fms, incomplete=%s)')
  :format(lc.item_count, lc.latency_ms, tostring(lc.is_incomplete)))
os.exit(0)
