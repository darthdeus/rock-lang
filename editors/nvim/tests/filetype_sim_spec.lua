-- Verifies that .sim files resolve to filetype `rocksim`, not the
-- builtin `simula` mapping. Driven through ftdetect/sim.lua loaded via
-- the rtp set up by tests/run.sh.
--
-- Background: rock-cli emits scripttest files with the `.sim`
-- extension. nvim's builtin filetype.lua maps .sim -> simula (Simula 67),
-- which is wrong for our use case. ftdetect/sim.lua registers the
-- override via vim.filetype.add.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

vim.cmd('edit /tmp/rock-nvim-ft-spec-' .. os.time() .. '.sim')
local ft = vim.bo.filetype
if ft ~= 'rocksim' then
  fail(('expected filetype `rocksim` for .sim, got `%s`'):format(ft))
end

print('OK: .sim resolves to rocksim')
os.exit(0)
