-- Regression: rock-lsp must stay alive when a project's stick.toml
-- omits `version`. Originally a sentinel for the bug
-- (rock_lsp_minimal_project_sentinel_spec.lua); flipped to a normal
-- regression test in the same commit as the fix in
-- rock/src/stick/mod.rs (Package.version became #[serde(default)]).
--
-- Bug shape: previously, a stick.toml of `[package]\nname = "x"\n`
-- crashed rock-lsp ~100ms after attach with a non-zero exit and no
-- diagnostic. The client showed `attached` with capabilities, then
-- every subsequent request hung silently.
--
-- This spec mirrors the smoke spec but without `version` to confirm
-- the fix still holds.

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
  -- Intentionally omit `version` — that was the trigger.
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "minimal"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write('fn main() {}\n'); f:close()
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

-- The bug exited 1 within ~100ms post-attach. Wait 1s and confirm
-- the client is still alive. (Same liveness check as the smoke spec.)
vim.wait(1000, function() return false end)
local c = vim.lsp.get_client_by_id(attached_id)
if not c or c:is_stopped() then
  local s = state.get().client
  fail(('REGRESSION: rock-lsp died on minimal stick.toml. state=%s last_exit=%s')
    :format(s.state, vim.inspect(s.last_exit)))
end

print('OK: rock-lsp survives minimal stick.toml (no version field)')
os.exit(0)
