-- Broadening rock's tag wiring to every filetype under a stick root must NOT
-- trample buffers that aren't real rock-project files. Two guarantees:
--   1. A foreign buffer-local `tags` (an ftplugin / gutentags) present when rock
--      first wires a buffer is PRESERVED (rock appends, never replaces).
--   2. Scratch/unnamed buffers are left alone even when cwd sits under a stick
--      root (`vim.fs.root` falls back to cwd for a nameless buffer).
-- Pure client-side (no `$ROCK_LSP_BIN`): drives the autocmds, not the server.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

vim.notify = function() end
vim.notify_once = function() end

local base = vim.fn.tempname()
local function mkpkg(name)
  local dir = base .. '/' .. name
  vim.fn.mkdir(dir, 'p')
  local f = assert(io.open(dir .. '/stick.toml', 'w'))
  f:write(('[package]\nname = "%s"\nversion = "0.1.0"\n'):format(name)); f:close()
  f = assert(io.open(dir .. '/shader.glsl', 'w'))
  f:write('void main() {}\n'); f:close()
  return dir
end
local pkg = mkpkg('delta')

local client = require('rock.client')
client.make_config({ binary = '/nonexistent/rock' })

-- Simulate an ftplugin / gutentags setting a buffer-local `tags` BEFORE rock
-- wires the buffer. Registered before setup_autocmds so it fires first on
-- BufReadPost (autocmds run in definition order).
vim.api.nvim_create_autocmd('BufReadPost', {
  pattern = '*.glsl',
  callback = function(args) vim.bo[args.buf].tags = '/foreign/sentinel.tags' end,
})

client.setup_autocmds()

-- Guarantee 1: foreign buffer-local tags preserved.
vim.cmd('edit ' .. vim.fn.fnameescape(pkg .. '/shader.glsl'))
local buf = vim.api.nvim_get_current_buf()
local tags = vim.bo[buf].tags
if not tags:find('/foreign/sentinel.tags', 1, true) then
  fail('foreign buffer-local tags discarded (should be preserved): ' .. tags)
end
if not tags:find(pkg .. '/.rock.tags', 1, true) then
  fail('rock tags not appended alongside the foreign base: ' .. tags)
end

-- Guarantee 2: a nameless scratch buffer under a cwd stick root is untouched
-- even when a new package opens and rock broadcasts over all loaded buffers.
-- Kept in its own window so `:edit` elsewhere can't reuse it.
vim.cmd('cd ' .. vim.fn.fnameescape(pkg))
vim.cmd('vnew') -- new window with a listed, loaded, buftype='', name='' buffer
local scratch = vim.api.nvim_get_current_buf()
local before = vim.bo[scratch].tags
vim.cmd('wincmd p') -- back to the glsl window; open the new package there
local pkg2 = mkpkg('epsilon')
vim.cmd('edit ' .. vim.fn.fnameescape(pkg2 .. '/shader.glsl'))
local after = vim.bo[scratch].tags
if after ~= before then
  fail('scratch/unnamed buffer had its tags rewritten: ' .. tostring(before) .. ' -> ' .. tostring(after))
end
if after:find('.rock.tags', 1, true) then
  fail('scratch/unnamed buffer got rock tags it never asked for: ' .. after)
end

print('OK: foreign tags preserved + scratch buffers untouched')
os.exit(0)
