-- rock-lsp writes a `.rock.tags` per stick.toml package. With buffers open in
-- two different packages, a tag jump (`<C-]>`) from one must resolve symbols in
-- the other -- so every rock buffer's `tags` must search the UNION of all open
-- packages' tag files, not just its own. Guards the cross-package aggregation
-- in `client.lua` (`note_package_tags`): opening a buffer in a NEW package
-- refreshes tags on every already-open rock buffer.

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

local base = vim.fn.tempname()
local function mkpkg(name, fn_name)
  local dir = base .. '/' .. name
  vim.fn.mkdir(dir, 'p')
  local f = assert(io.open(dir .. '/stick.toml', 'w'))
  f:write(('[package]\nname = "%s"\nversion = "0.1.0"\n'):format(name)); f:close()
  f = assert(io.open(dir .. '/' .. name .. '.rock', 'w'))
  f:write(('fn %s() -> int { 1 }\n'):format(fn_name)); f:close()
  return dir
end
local pkg_a = mkpkg('alpha', 'alpha_fn')
local pkg_b = mkpkg('beta', 'beta_fn')

local client = require('rock.client')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

local attached = {}
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached[#attached + 1] = args.buf end
  end,
})

-- Open package A's file, then package B's file.
vim.cmd('edit ' .. vim.fn.fnameescape(pkg_a .. '/alpha.rock'))
local buf_a = vim.api.nvim_get_current_buf()
if not vim.wait(5000, function() return #attached >= 1 end, 50) then
  fail('LspAttach did not fire for alpha')
end

vim.cmd('edit ' .. vim.fn.fnameescape(pkg_b .. '/beta.rock'))
if not vim.wait(5000, function() return #attached >= 2 end, 50) then
  fail('LspAttach did not fire for beta')
end
vim.wait(200, function() return false end)

-- buf_a (alpha, now backgrounded) must search BOTH packages' tag files.
local tags_a = vim.bo[buf_a].tags
if not tags_a:find(pkg_a .. '/.rock.tags', 1, true) then
  fail("alpha buffer tags missing alpha's own .rock.tags: " .. tags_a)
end
if not tags_a:find(pkg_b .. '/.rock.tags', 1, true) then
  fail("alpha buffer tags missing beta's .rock.tags (cross-package union): " .. tags_a)
end

print('OK: cross-package tag union -> ' .. tags_a)
os.exit(0)
