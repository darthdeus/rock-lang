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
