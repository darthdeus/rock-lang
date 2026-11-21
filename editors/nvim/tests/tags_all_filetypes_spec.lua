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
  f = assert(io.open(dir .. '/' .. name .. '.rock', 'w'))
  f:write('fn ' .. name .. '_fn() -> int { 1 }\n'); f:close()
  f = assert(io.open(dir .. '/shader.glsl', 'w'))
  f:write('void main() {}\n'); f:close()
  return dir
end
local pkg = mkpkg('gamma')

local client = require('rock.client')
client.make_config({ binary = '/nonexistent/rock' })
client.setup_autocmds()

-- Open the .glsl file: NOT a rock buffer, no LSP attaches.
vim.cmd('edit ' .. vim.fn.fnameescape(pkg .. '/shader.glsl'))
local buf = vim.api.nvim_get_current_buf()
if vim.bo[buf].filetype == 'rock' then
  fail('shader.glsl unexpectedly has filetype=rock; test would be meaningless')
end

local tags = vim.bo[buf].tags
if not tags:find(pkg .. '/.rock.tags', 1, true) then
  fail("glsl buffer tags missing the package's .rock.tags: " .. tags)
end

print('OK: non-rock buffer tags -> ' .. tags)
os.exit(0)
