-- Neovim maps .sim to simula by default.

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
