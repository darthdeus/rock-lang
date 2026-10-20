-- Multi-buffer attach + restart re-attaches all.
--
-- Open three .rock files in the same project, all should attach to a
-- *single* rock-lsp client (nvim's reuse_client deduplicates by name +
-- root). After client.restart(), all three buffers must re-attach to
-- the new client.
--
-- This is the most bug-prone scenario in normal use: getting reuse
-- wrong spawns three servers; getting restart wrong leaves orphaned
-- buffers without semantic tokens. Both have happened during
-- development of this plugin.

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
  f:write('[package]\nname = "multi"\nversion = "0.1.0"\n'); f:close()
  for _, name in ipairs({ 'a.rock', 'b.rock', 'c.rock' }) do
    f = assert(io.open(root .. '/' .. name, 'w'))
    f:write('fn ' .. name:sub(1, 1) .. '() {}\n'); f:close()
  end
end

local client = require('rock.client')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

local attaches = {}
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then
      attaches[args.buf] = args.data.client_id
    end
  end,
})

local bufs = {}
for _, name in ipairs({ 'a.rock', 'b.rock', 'c.rock' }) do
  vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/' .. name))
  table.insert(bufs, vim.api.nvim_get_current_buf())
end

if not vim.wait(8000, function()
  return attaches[bufs[1]] and attaches[bufs[2]] and attaches[bufs[3]]
end, 50) then
  fail(('only %d of 3 buffers attached: %s'):format(
    vim.tbl_count(attaches), vim.inspect(attaches)))
end

-- All three buffers must share a single client (reuse_client by root).
local first_id = attaches[bufs[1]]
for _, b in ipairs(bufs) do
  if attaches[b] ~= first_id then
    fail(('buffer %d attached to client %d; expected shared client %d (reuse failed -> N servers)')
      :format(b, attaches[b], first_id))
  end
end
local rock_clients_pre = vim.lsp.get_clients({ name = 'rock' })
if #rock_clients_pre ~= 1 then
  fail(('expected exactly 1 rock client, got %d'):format(#rock_clients_pre))
end

-- Phase 2: restart. All buffers should re-attach to a new client. The
-- new client's id will differ from first_id.
attaches = {}
client.restart()
if not vim.wait(8000, function()
  return attaches[bufs[1]] and attaches[bufs[2]] and attaches[bufs[3]]
end, 50) then
  fail(('after restart, only %d of 3 buffers re-attached: %s'):format(
    vim.tbl_count(attaches), vim.inspect(attaches)))
end

local new_id = attaches[bufs[1]]
if new_id == first_id then
  fail('client id unchanged after restart -- restart() did not actually re-spawn')
end
for _, b in ipairs(bufs) do
  if attaches[b] ~= new_id then
    fail(('post-restart buffer %d attached to client %d; expected shared %d')
      :format(b, attaches[b], new_id))
  end
end

local rock_clients_post = vim.lsp.get_clients({ name = 'rock' })
if #rock_clients_post ~= 1 then
  fail(('post-restart: expected exactly 1 rock client, got %d'):format(#rock_clients_post))
end

print(('OK: 3 buffers shared client %d -> restart -> shared client %d'):format(first_id, new_id))
os.exit(0)
