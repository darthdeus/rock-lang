-- Requires an executable server in ROCK_LSP_BIN.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end
if not (vim.uv or vim.loop).fs_stat(bin) then
  fail('$ROCK_LSP_BIN points at non-existent file: ' .. bin)
end

local notifications = {}
vim.notify = function(msg, level, opts)
  table.insert(notifications, { msg = tostring(msg), level = level })
end
vim.notify_once = vim.notify

vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
local f = assert(io.open(root .. '/stick.toml', 'w'))
-- A manifest version is required for initialization.
f:write('[package]\nname = "smoke"\nversion = "0.1.0"\n')
f:close()
local rock_path = root .. '/main.rock'
f = assert(io.open(rock_path, 'w'))
f:write('fn main() {}\n')
f:close()

local client = require('rock.client')
local cfg = client.make_config({ binary = bin })
vim.lsp.config('rock', cfg)
client.setup_autocmds()

if not require('rock.state').get().binary.exists then
  fail('binary marked non-existent despite $ROCK_LSP_BIN being valid')
end

vim.lsp.enable('rock')

vim.cmd('edit ' .. vim.fn.fnameescape(rock_path))

local attached_id = nil
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})

local ok = vim.wait(5000, function() return attached_id ~= nil end, 50)
if not ok then
  io.stderr:write('--- notifications ---\n')
  for i, n in ipairs(notifications) do
    io.stderr:write(('  [%d] level=%s msg=%s\n'):format(i, tostring(n.level), n.msg:sub(1, 300)))
  end
  io.stderr:write('--- :messages ---\n')
  local out = pcall(vim.api.nvim_exec2, 'messages', { output = true })
  if out then io.stderr:write(tostring(out) .. '\n') end
  io.stderr:write('--- client state ---\n')
  io.stderr:write(vim.inspect(require('rock.state').get().client) .. '\n')
  fail('LspAttach did not fire within 5s for rock-lsp')
end

local c = vim.lsp.get_client_by_id(attached_id)
if not c then fail('attached client vanished from registry') end
if type(c.server_capabilities) ~= 'table' or vim.tbl_isempty(c.server_capabilities) then
  fail('attached client has no server_capabilities (initialize did not complete)')
end

-- Initialization alone does not prove the server stayed alive.
vim.wait(500, function() return false end)
local still = vim.lsp.get_client_by_id(attached_id)
if not still or still:is_stopped() then
  fail(('client died %s after attach (state=%s last_exit=%s)')
    :format(still and 'is_stopped' or 'vanished',
      require('rock.state').get().client.state,
      vim.inspect(require('rock.state').get().client.last_exit)))
end

for _, n in ipairs(notifications) do
  if n.msg:find('crashed', 1, true) then
    fail('rock-lsp crashed notification fired during smoke run: ' .. n.msg)
  end
  if n.msg:find('binary not found', 1, true) then
    fail('missing-binary notification fired despite real binary: ' .. n.msg)
  end
end

print(('OK: rock-lsp attached (id=%d), %d capabilities reported')
  :format(attached_id, vim.tbl_count(c.server_capabilities)))
os.exit(0)
