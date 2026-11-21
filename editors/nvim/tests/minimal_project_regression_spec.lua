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

local project = dofile('tests/_lib/project.lua')
local _, client = project.open('minimal', { ['main.rock'] = 'fn main() {}\n' },
  nil, '[package]\nname = "minimal"\n')

-- The bug exited 1 within ~100ms post-attach. Wait 1s and confirm
-- the client is still alive. (Same liveness check as the smoke spec.)
vim.wait(1000, function() return false end)
if not vim.lsp.get_client_by_id(client.id) or client:is_stopped() then
  local s = require('rock.state').get().client
  fail(('REGRESSION: rock-lsp died on minimal stick.toml. state=%s last_exit=%s')
    :format(s.state, vim.inspect(s.last_exit)))
end

print('OK: rock-lsp survives minimal stick.toml (no version field)')
os.exit(0)
