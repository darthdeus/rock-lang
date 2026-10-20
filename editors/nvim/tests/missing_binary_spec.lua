-- Regression: opening N rock buffers with the binary missing must surface
-- exactly one user-facing notification (no "Press ENTER" prompt loop, no
-- nvim "Client quit / cmd failed" floods, no "RockRestart" loop).
--
-- This spec drives the *real* attach path: vim.lsp.enable + FileType
-- autocmd → cmd() → failure. That's where the bug lived. We do NOT call
-- init.lua's setup(), because setup() has its own pre-enable gate; if we
-- relied on that gate the cmd() failure path would never run and the test
-- would be silently passing for the wrong reason (review note #3).

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

-- Capture every vim.notify call. nvim's framework emits "Client quit" /
-- "cmd failed" through vim.notify in 0.11+, so a clean run should have
-- exactly one entry from us and zero from the framework.
local notifications = {}
local orig_notify = vim.notify
vim.notify = function(msg, level, opts)
  table.insert(notifications, { msg = tostring(msg), level = level, opts = opts })
end
vim.notify_once = function(msg, level, opts)
  for _, n in ipairs(notifications) do
    if n.msg == tostring(msg) then return end
  end
  table.insert(notifications, { msg = tostring(msg), level = level, opts = opts })
end

-- Also capture :echomsg / nvim_echo so a future regression that switches
-- from notify to echo (which can ALSO trigger "Press ENTER") is caught.
-- We snapshot :messages history at the end and assert no error-class
-- echoes leaked.
local function messages_history()
  local ok, out = pcall(vim.api.nvim_exec2, 'messages', { output = true })
  if not ok then return '' end
  return out and out.output or ''
end

local missing = '/tmp/rock-lsp-does-not-exist-' .. os.time()

vim.filetype.add({ extension = { rock = 'rock' } })

local ok, client = pcall(require, 'rock.client')
if not ok then fail('failed to require rock.client: ' .. tostring(client)) end

local cfg = client.make_config({ binary = missing })
vim.lsp.config('rock', cfg)
client.setup_autocmds()

-- Sanity: the binary should NOT exist.
local bin_info = require('rock.state').get().binary
if bin_info.exists then
  fail('test setup wrong: binary is reported as existing: ' .. tostring(bin_info.path))
end

-- Step 1: drive the actual bug path. Enable LSP with a missing binary,
-- then create rock buffers and fire FileType so cmd() runs for each.
-- Pre-fix: each buffer's cmd() failure produces (a) our notify (b)
-- nvim's framework "cmd failed" notify (c) "Client quit" notify.
-- Post-fix: first cmd() disables autoattach; subsequent buffers don't
-- trigger cmd(); only one of our notifications surfaces.
vim.lsp.enable('rock')

local bufs = {}
for i = 1, 5 do
  local buf = vim.api.nvim_create_buf(true, false)
  vim.api.nvim_buf_set_name(buf, ('/tmp/spec-%d.rock'):format(i))
  vim.api.nvim_set_option_value('filetype', 'rock', { buf = buf })
  table.insert(bufs, buf)
end
-- Drain scheduled callbacks (notify is dispatched via vim.schedule).
vim.wait(300, function() return false end)

-- Step 2: simulate :RockRestart with binary still missing. This should
-- bail with one ERROR rather than re-enabling LSP and looping again.
client.restart()
vim.wait(400, function() return false end)

-- Expectations after both phases:
--   * Exactly 1 cmd-path notification ("rock-lsp binary not found: ...")
--   * Exactly 1 restart-path notification ("not restarting LSP")
--   * NO nvim-framework "Client ... quit" notifications
--   * NO "Press ENTER" / hit-enter prompt indicators in :messages
local cmd_notifs, restart_notifs, framework_notifs = 0, 0, 0
for _, n in ipairs(notifications) do
  if n.msg:find('binary not found:', 1, true) then
    cmd_notifs = cmd_notifs + 1
  elseif n.msg:find('not restarting LSP', 1, true) then
    restart_notifs = restart_notifs + 1
  elseif n.msg:find('Client', 1, true) and n.msg:find('quit', 1, true) then
    framework_notifs = framework_notifs + 1
  elseif n.msg:find('cmd failed', 1, true) then
    framework_notifs = framework_notifs + 1
  end
end

local hist = messages_history()

local function dump()
  io.stderr:write(('  cmd_notifs=%d restart_notifs=%d framework_notifs=%d\n')
    :format(cmd_notifs, restart_notifs, framework_notifs))
  for i, n in ipairs(notifications) do
    io.stderr:write(('  [%d] level=%s msg=%s\n')
      :format(i, tostring(n.level), n.msg:sub(1, 200)))
  end
  io.stderr:write('--- :messages ---\n' .. hist .. '\n--- end ---\n')
end

if cmd_notifs ~= 1 then
  dump(); fail('expected exactly 1 cmd-path notification, got ' .. cmd_notifs)
end
if restart_notifs ~= 1 then
  dump(); fail('expected exactly 1 restart-path notification, got ' .. restart_notifs)
end
if framework_notifs ~= 0 then
  dump(); fail('nvim framework leaked ' .. framework_notifs .. ' notifications')
end
-- :messages should not contain any per-buffer "cmd failed" / "Client ... quit"
-- echoes either (some nvim versions route through echomsg).
if hist:find('cmd failed', 1, true) then
  dump(); fail(':messages shows "cmd failed" -- prompt-loop bug is back')
end

-- Step 3: recovery. After binary "appears", restart() should re-enable
-- and the dedup flag should reset so a future disappearance is reported
-- again. Touch the file to make fs_stat succeed.
local f = io.open(missing, 'w')
if f then f:close() end
require('rock.binary').refresh(missing)

-- Re-stub notify counter for the recovery probe.
local recovery_notifs = {}
vim.notify = function(msg, level, opts)
  table.insert(recovery_notifs, { msg = tostring(msg), level = level })
  table.insert(notifications, { msg = tostring(msg), level = level, opts = opts })
end

-- restart() will try to enable + start. The "binary" is an empty file
-- and rpc.start will fail. That's fine — what we're verifying is that
-- restart() DID NOT bail at the gate (it should not, binary exists), and
-- that a SECOND disappearance later would re-notify (dedup flag reset).
client.restart()
vim.wait(300, function() return false end)
os.remove(missing)

-- We don't strictly assert on recovery_notifs count (rpc start failure
-- mode varies by nvim version), but we do verify the gate passed: the
-- "not restarting" message should NOT appear, since the binary existed
-- at restart time.
local gate_blocked_recovery = false
for _, n in ipairs(recovery_notifs) do
  if n.msg:find('not restarting LSP', 1, true) then
    gate_blocked_recovery = true; break
  end
end
if gate_blocked_recovery then
  fail('restart() bailed at the gate even though binary existed -- recovery broken')
end

print(('OK: %d notifications total, %d recovery notifications')
  :format(#notifications, #recovery_notifs))
os.exit(0)
