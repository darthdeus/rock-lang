-- Verifies init.lua's minimum-nvim-version gate.
--
-- Drives the real setup() entry point. Mocks vim.version() to simulate
-- both an old nvim (must reject + notify ERROR + return without
-- registering anything) and the current nvim (must proceed past the
-- gate). We DO NOT call setup() in "current" mode end-to-end — that
-- would try to register lsp config + bind keymaps which the missing
-- binary path stress-tests separately. We only assert the gate itself
-- did not bail.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local notifications = {}
vim.notify = function(msg, level, opts)
  table.insert(notifications, { msg = tostring(msg), level = level })
end

local rock = require('rock')
if type(rock.min_nvim) ~= 'table' or #rock.min_nvim ~= 3 then
  fail('rock.min_nvim is not a {major,minor,patch} triple')
end

-- Phase 1: simulate an old nvim. Must bail with ERROR notify and not
-- touch lsp.config / autocmds.
local orig_version = vim.version
vim.version = setmetatable({}, { __call = function() return { major = 0, minor = 10, patch = 5 } end })
vim.version.lt = orig_version.lt

local lsp_config_called = false
local orig_lsp_config = vim.lsp.config
vim.lsp.config = function(...) lsp_config_called = true; return orig_lsp_config(...) end

rock.setup({ binary = '/tmp/does-not-matter-' .. os.time() })

if lsp_config_called then
  fail('setup() reached vim.lsp.config despite old nvim version')
end
if rock.opts ~= nil then
  fail('setup() populated rock.opts despite old nvim version (gate did not bail)')
end

local saw_version_err = false
for _, n in ipairs(notifications) do
  if n.level == vim.log.levels.ERROR
      and n.msg:find('requires Neovim', 1, true)
      and n.msg:find('Plugin not loaded', 1, true) then
    saw_version_err = true; break
  end
end
if not saw_version_err then
  for i, n in ipairs(notifications) do
    io.stderr:write(('  [%d] level=%s msg=%s\n'):format(i, tostring(n.level), n.msg))
  end
  fail('expected version-gate ERROR notification, did not see one')
end

-- Phase 2: restore real version + lsp.config and confirm setup proceeds
-- past the gate (rock.opts gets populated). Use a guaranteed-missing
-- binary so we don't actually spin up an LSP.
vim.version = orig_version
vim.lsp.config = orig_lsp_config
notifications = {}

local missing = '/tmp/rock-lsp-version-spec-' .. os.time()
rock.setup({ binary = missing, auto_open_panel = false })

if rock.opts == nil then
  fail('setup() did not proceed past the gate on current nvim version')
end
if rock.opts.binary ~= missing then
  fail('setup() did not merge user opts (binary mismatch)')
end

print('OK: version gate rejects old, accepts current')
os.exit(0)
