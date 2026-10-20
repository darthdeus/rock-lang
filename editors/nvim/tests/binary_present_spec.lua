-- Inverse of missing_binary_spec: with a fake-but-existing rock-lsp,
-- the cmd() path should NOT short-circuit at the missing-binary gate.
--
-- The fake binary is a script that exits 0 immediately (see
-- tests/_lib/fake_server.lua). That means: (a) the missing-binary branch in cmd()
-- is NOT taken; (b) vim.lsp.rpc.start spawns it; (c) the on_exit
-- dispatcher fires with code=0 and we end in state='detached', NOT
-- state='crashed'. Crucially, we should NOT get the "binary not found"
-- notification, and `vim.lsp.enable(SERVER_NAME, false)` should NOT be
-- invoked from the cmd() path.
--
-- This pins the recovery path: a user who fixes their build should
-- never see the missing-binary error after the fix.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local fake_server = require('_lib.fake_server')
local fake_bin = fake_server.write(vim.fn.tempname(), 'rock', 0)

local notifications = {}
vim.notify = function(msg, level, opts)
  table.insert(notifications, { msg = tostring(msg), level = level })
end
vim.notify_once = vim.notify

vim.filetype.add({ extension = { rock = 'rock' } })

local client = require('rock.client')
local state = require('rock.state')

-- Track whether anyone called vim.lsp.enable(name, false) from cmd().
-- The missing-binary branch does this; the present branch must not.
local disable_calls = 0
local orig_enable = vim.lsp.enable
vim.lsp.enable = function(name, enable_flag)
  if name == 'rock' and enable_flag == false then
    disable_calls = disable_calls + 1
  end
  return orig_enable(name, enable_flag)
end

local cfg = client.make_config({ binary = fake_bin })
vim.lsp.config('rock', cfg)
client.setup_autocmds()

-- Sanity: state should report binary exists.
local bin_info = state.get().binary
if not bin_info.exists then
  fail('test setup wrong: fake_bin not seen as existing: ' .. tostring(fake_bin))
end

vim.lsp.enable('rock')

-- Create a rock buffer and let cmd() fire.
local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(buf, '/tmp/binary-present-spec.rock')
vim.api.nvim_set_option_value('filetype', 'rock', { buf = buf })

-- Wait for cmd() + the spawned `true` to exit and on_exit to fire.
vim.wait(800, function()
  local s = state.get().client.state
  return s == 'detached' or s == 'crashed'
end)

-- Assertion 1: no missing-binary notification.
for _, n in ipairs(notifications) do
  if n.msg:find('binary not found', 1, true) then
    fail('missing-binary notification fired despite binary existing: ' .. n.msg)
  end
end

-- Assertion 2: cmd() did NOT disable autoattach.
if disable_calls ~= 0 then
  fail('vim.lsp.enable(name, false) was called ' .. disable_calls .. ' times from present-binary path')
end

-- Assertion 3: client transitioned starting -> detached (true exits 0).
local final_state = state.get().client.state
if final_state ~= 'detached' then
  -- Some nvim versions classify a server that exits before initialize
  -- as crashed. Accept that, but require the exit code be 0 (true exits 0).
  local last = state.get().client.last_exit
  if not last or last.code ~= 0 then
    fail(('client did not end in detached/exit-0; state=%s last_exit=%s')
      :format(final_state, vim.inspect(last)))
  end
end

-- Assertion 4: binary watch was set up. It's an internal handle but we
-- can check stop_watch is callable without errors (proxy for "watch
-- was started").
require('rock.binary').stop_watch()

print(('OK: present-binary path runs cmd() without error notifications (final state=%s)')
  :format(final_state))
os.exit(0)
