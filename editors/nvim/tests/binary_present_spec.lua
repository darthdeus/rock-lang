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

local client = require('rock.client')
local state = require('rock.state')

local disable_calls = 0
local orig_enable = vim.lsp.enable
vim.lsp.enable = function(name, enable_flag)
  if name == 'rock' and enable_flag == false then
    disable_calls = disable_calls + 1
  end
  return orig_enable(name, enable_flag)
end

local cfg = client.make_config({ binary = fake_bin })

local bin_info = state.get().binary
if not bin_info.exists then
  fail('test setup wrong: fake_bin not seen as existing: ' .. tostring(fake_bin))
end

local exited = false
cfg.cmd({ on_exit = function() exited = true end })
vim.wait(5000, function() return exited end)
if not exited then fail('fake server did not exit') end

for _, n in ipairs(notifications) do
  if n.msg:find('binary not found', 1, true) then
    fail('missing-binary notification fired despite binary existing: ' .. n.msg)
  end
end

if disable_calls ~= 0 then
  fail('vim.lsp.enable(name, false) was called ' .. disable_calls .. ' times from present-binary path')
end

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

require('rock.binary').stop_watch()

print(('OK: present-binary path runs cmd() without error notifications (final state=%s)')
  :format(final_state))
os.exit(0)
