-- SENTINEL: pins the current limitation that .sim filetype detection
-- is only applied to buffers opened AFTER ftdetect/sim.lua has run.
-- A buffer opened before our ftdetect (e.g. via session restore, or
-- when the plugin is lazy-loaded by lazy.nvim with `ft = 'rock'` and
-- the user opens a .sim file before any .rock file) keeps nvim's
-- builtin `simula` mapping forever — vim.filetype.add only affects
-- subsequent detection.
--
-- TODO: make .sim -> rocksim work for already-open buffers too. Two
-- options worth considering:
--   1. Add a BufRead/BufNewFile autocmd in ftdetect/sim.lua (or a
--      plugin/ file) that re-applies `set filetype=rocksim` for
--      already-loaded .sim buffers.
--   2. Document `init = function() vim.filetype.add(...) end` in the
--      lazy.nvim spec example so the registration happens at startup
--      rather than at first .rock buffer.
-- Either way, when fixed: flip the assertion below to == 'rocksim'
-- and remove the TODO. Follow the `*_sentinel_spec.lua` header shape.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

-- Edit the .sim buffer BEFORE registering our extension override.
-- Mimics a session where the buffer was loaded before ftdetect ran.
local path = '/tmp/rock-nvim-ft-sentinel-' .. os.time() .. '.sim'
io.open(path, 'w'):close()

-- The run.sh-seeded rtp already installed a `sim -> rocksim` mapping
-- at startup via ftdetect/sim.lua. We can't actually unregister it
-- (vim.filetype.add merges; passing nil is a no-op since Lua drops
-- nil values from tables). Instead, force-override back to the
-- builtin `simula` mapping for this spec only, then later restore.
vim.filetype.add({ extension = { sim = 'simula' } })

vim.cmd('edit ' .. path)
local ft_before = vim.bo.filetype

-- Now register, the way ftdetect/sim.lua does. The already-open
-- buffer should NOT pick this up — that's the bug.
vim.filetype.add({ extension = { sim = 'rocksim' } })
local ft_after = vim.bo.filetype

if ft_before ~= 'simula' then
  fail(('expected pre-registration ft to be `simula` (the builtin), got `%s`. '
        .. 'Test setup is broken — likely the rocksim registration leaked in '
        .. 'and the wipe above did not take effect.'):format(ft_before))
end

-- Inverted assertion: pins the buggy behavior. Flip to == 'rocksim'
-- (and delete the TODO above) once detection is hardened.
if ft_after ~= 'simula' then
  fail(('SENTINEL FLIPPED: .sim opened before ftdetect now resolves to `%s`. '
        .. 'Detection has been hardened to handle already-open buffers — '
        .. 'invert the assertion in this spec and remove the TODO at the top.'):format(ft_after))
end

print('OK (sentinel): .sim opened before ftdetect stays as `simula`')
os.exit(0)
