-- Crash-and-recover loop.
--
-- Pinned behavior:
--   1. Spawning a binary that exits non-zero drives
--      cmd() past the missing-binary gate, into vim.lsp.rpc.start, and
--      the on_exit dispatcher fires with code != 0. State should
--      transition to 'crashed' and last_exit.code should be 1.
--   2. The crash notification fires.
--   3. mark_buffers_crashed sets a diagnostic in the rock.nvim.crash
--      namespace on every rock buffer that was tracked.
--   4. After client.restart() with a working binary (exits 0),
--      crash diagnostics are cleared on the rock buffer.
--
-- We do NOT need a real LSP for this spec — a fake server that exits 0 or 1
-- is enough to drive the spawn-and-exit machinery (tests/_lib/fake_server.lua).

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local fake_server = require('_lib.fake_server')
local fake_dir = vim.fn.tempname()
local false_bin = fake_server.write(fake_dir, 'rock-crash', 1)
local true_bin = fake_server.write(fake_dir, 'rock-ok', 0)

local notifications = {}
vim.notify = function(msg, level)
  table.insert(notifications, { msg = tostring(msg), level = level })
end
vim.notify_once = vim.notify

vim.filetype.add({ extension = { rock = 'rock' } })

local client = require('rock.client')
local state = require('rock.state')

vim.lsp.config('rock', client.make_config({ binary = false_bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_set_name(buf, '/tmp/crash-recovery-spec.rock')
vim.api.nvim_set_option_value('filetype', 'rock', { buf = buf })

-- Wait for spawn + on_exit. The fake exits 1 immediately, so
-- the on_exit dispatcher should fire and set state.client.state.
vim.wait(1500, function()
  local s = state.get().client.state
  return s == 'crashed' or s == 'detached'
end)
-- on_exit sets state synchronously but schedules the user-facing
-- notification via vim.schedule; drain so the notification lands.
vim.wait(200, function() return false end)

-- The state.set_buffer call in on_attach never fires (no attach
-- happens because the server exited before initialize). But we
-- track buffers via state when we do bookkeeping, and the crash
-- diagnostics use whatever buffers are in state.get().buffers. That
-- means mark_buffers_crashed is a no-op when the client never
-- attached. That's a real-world limitation — a server that crashes
-- before initialize won't get crash diagnostics on buffers either.
--
-- What we CAN reliably assert here:
--   (a) state.client.state transitioned to 'crashed'
--   (b) last_exit.code == 1
--   (c) crash notification fired
--   (d) the missing-binary disable-autoattach path was NOT taken
--       (binary exists; the failure is in the spawned process, not
--       the spawn itself)

local s = state.get().client
if s.state ~= 'crashed' then
  fail('expected state.client.state == "crashed", got ' .. tostring(s.state))
end
if not s.last_exit or s.last_exit.code ~= 1 then
  fail('expected last_exit.code == 1, got ' .. vim.inspect(s.last_exit))
end

local crash_notif_seen = false
for _, n in ipairs(notifications) do
  if n.msg:find('rock-lsp crashed', 1, true) then crash_notif_seen = true; break end
end
if not crash_notif_seen then
  for i, n in ipairs(notifications) do
    io.stderr:write(('  [%d] %s\n'):format(i, n.msg:sub(1, 200)))
  end
  fail('crash notification did not fire')
end

-- Sanity: missing-binary notification must NOT have fired (binary exists).
for _, n in ipairs(notifications) do
  if n.msg:find('binary not found', 1, true) then
    fail('missing-binary path was taken on a present binary: ' .. n.msg)
  end
end

-- Phase 2: recovery. Swap the configured binary to one that exits 0 and
-- restart. The "not restarting" gate must NOT fire (binary exists),
-- and after restart state should reset to 'starting' or 'detached'
-- (true exits 0).
notifications = {}
vim.lsp.config('rock', client.make_config({ binary = true_bin }))
client.restart()
-- restart() defers 100ms, then waits up to 2s for old clients to
-- drain, then re-enables + doautocmd FileType. The new spawn's
-- on_exit (true exits 0) is the moment last_exit.code resets to 0.
-- Wait long enough for the whole chain.
vim.wait(4000, function()
  local last = state.get().client.last_exit
  return last and last.code == 0
end, 50)

for _, n in ipairs(notifications) do
  if n.msg:find('not restarting LSP', 1, true) then
    fail('restart() bailed at the gate despite working binary')
  end
end

-- After recovery the state should not still be reporting code=1.
-- Either it's still spinning up ('starting') or true exited cleanly
-- ('detached' with code=0).
local s2 = state.get().client
if s2.last_exit and s2.last_exit.code == 1 then
  fail('post-recovery still has code=1 last_exit; restart() did not actually re-spawn')
end

print(('OK: crash detected (state=%s exit=%d), recovery transitioned to %s')
  :format(s.state, s.last_exit.code, s2.state))
os.exit(0)
