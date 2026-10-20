-- Semantic tokens fire automatically after attach (nvim's built-in
-- behavior when the server reports semanticTokensProvider). The
-- plugin's request-timing ring should capture the call — proving the
-- wrapped rpc.request shim caught nvim's auto-fired traffic, not
-- just user-driven calls.

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
  f:write('[package]\nname = "semtok"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn main() {\n    let x = 42\n}\n'); f:close()
end

local client = require('rock.client')
local state = require('rock.state')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))

local attached
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached = args.data.client_id end
  end,
})
if not vim.wait(5000, function() return attached ~= nil end, 50) then
  fail('LspAttach did not fire')
end

-- Wait for nvim to auto-fire semantic tokens. The request-timing ring
-- is keyed by method; we look for textDocument/semanticTokens/full or
-- the Range variant.
local function semtok_seen()
  local rt = state.get().request_timings
  return (rt['textDocument/semanticTokens/full']
       or rt['textDocument/semanticTokens/full/delta']
       or rt['textDocument/semanticTokens/range']) ~= nil
end

if not vim.wait(4000, semtok_seen, 50) then
  io.stderr:write('--- request_timings keys seen ---\n')
  for k in pairs(state.get().request_timings) do
    io.stderr:write('  ' .. k .. '\n')
  end
  fail('no semanticTokens entry in request_timings within 4s')
end

local rt = state.get().request_timings
local key = (rt['textDocument/semanticTokens/full'] and 'full')
  or (rt['textDocument/semanticTokens/full/delta'] and 'full/delta')
  or 'range'
local samples = rt['textDocument/semanticTokens/' .. key].samples
if not samples or #samples == 0 then
  fail('semanticTokens entry exists but has no samples')
end

print(('OK: captured semanticTokens/%s (%d sample(s), first=%.2fms)')
  :format(key, #samples, samples[1]))
os.exit(0)
