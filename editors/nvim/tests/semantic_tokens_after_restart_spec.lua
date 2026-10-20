-- Semantic-token re-request after restart. A known-fragile path:
-- client.lua:328 explicitly calls
-- vim.lsp.semantic_tokens.force_refresh on every rock buffer 300ms
-- after restart, because in earlier nvim versions the auto-refresh
-- on attach didn't always fire the second time around.
--
-- This spec asserts: open a .rock buffer, wait for the initial
-- semanticTokens request, restart, and confirm a SECOND
-- semanticTokens request fires within a reasonable window.

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
  f:write('[package]\nname = "semtok_restart"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn main() {\n    let x = 42\n}\n'); f:close()
end

local client = require('rock.client')
local state = require('rock.state')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')
vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))

local function semtok_keys()
  local rt = state.get().request_timings
  local keys = {}
  for k in pairs(rt) do
    if k:match('^textDocument/semanticTokens') then table.insert(keys, k) end
  end
  return keys
end

local function semtok_total()
  local n = 0
  for _, k in ipairs(semtok_keys()) do
    n = n + #(state.get().request_timings[k].samples or {})
  end
  return n
end

-- Phase 1: wait for the initial attach + first semanticTokens.
local attached_id
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})
if not vim.wait(5000, function() return attached_id ~= nil end, 50) then
  fail('initial LspAttach did not fire')
end
if not vim.wait(4000, function() return semtok_total() > 0 end, 50) then
  fail('initial semanticTokens request never captured before restart')
end
local pre_restart = semtok_total()
local first_id = attached_id

-- Phase 2: restart. attached_id will be reset by the next LspAttach.
attached_id = nil
client.restart()
if not vim.wait(8000, function()
  return attached_id ~= nil and attached_id ~= first_id
end, 50) then
  fail('post-restart LspAttach did not fire with a fresh client id')
end

-- Phase 3: assert at least one ADDITIONAL semanticTokens request
-- fires after the restart. client.lua:328 force_refreshes 300ms
-- after the buffer re-attaches, so allow up to 4s.
if not vim.wait(4000, function() return semtok_total() > pre_restart end, 50) then
  io.stderr:write('--- semtok keys after restart ---\n')
  for _, k in ipairs(semtok_keys()) do
    io.stderr:write(('  %s -> %d samples\n'):format(k,
      #(state.get().request_timings[k].samples or {})))
  end
  fail(('post-restart semanticTokens never re-fired (pre=%d post=%d)')
    :format(pre_restart, semtok_total()))
end

print(('OK: semanticTokens re-fired after restart (pre=%d post=%d, client %d -> %d)')
  :format(pre_restart, semtok_total(), first_id, attached_id))
os.exit(0)
