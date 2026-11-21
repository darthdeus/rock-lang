-- Current limitation: filetype registration does not update already-open buffers.
-- When fixed, the assertion should expect rocksim.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local path = '/tmp/rock-nvim-ft-sentinel-' .. os.time() .. '.sim'
io.open(path, 'w'):close()

-- vim.filetype.add merges mappings; restore the original mapping after the test.
vim.filetype.add({ extension = { sim = 'simula' } })

vim.cmd('edit ' .. path)
local ft_before = vim.bo.filetype

vim.filetype.add({ extension = { sim = 'rocksim' } })
local ft_after = vim.bo.filetype

if ft_before ~= 'simula' then
  fail(('expected pre-registration ft to be `simula` (the builtin), got `%s`. '
        .. 'Test setup is broken — likely the rocksim registration leaked in '
        .. 'and the wipe above did not take effect.'):format(ft_before))
end

if ft_after ~= 'simula' then
  fail(('SENTINEL FLIPPED: .sim opened before ftdetect now resolves to `%s`. '
        .. 'Detection has been hardened to handle already-open buffers — '
        .. 'invert the assertion in this spec and remove the TODO at the top.'):format(ft_after))
end

print('OK (sentinel): .sim opened before ftdetect stays as `simula`')
os.exit(0)
