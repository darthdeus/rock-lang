-- Drive a REAL rock LSP through the plugin and report what nvim declares for
-- semantic tokens (range / full.delta) AND which request kinds it actually
-- sends (the server logs them to /tmp/rock_semtok_caps.log). Run from the
-- plugin root:
--
--   ROCK_LSP_BIN=/path/to/rock nvim --headless --clean -u NONE -l scripts/semtok_caps.lua
--
-- Without $ROCK_LSP_BIN the plugin's own resolution applies (cwd worktree
-- build, rock-lang checkout, then `rock` on PATH). Capability questions ->
-- observation, not guessing.

local here = vim.fn.fnamemodify(debug.getinfo(1, 'S').source:sub(2), ':p:h')
local plugin = vim.fn.fnamemodify(here .. '/..', ':p'):gsub('/$', '')
vim.opt.runtimepath:append(plugin)
vim.filetype.add({ extension = { rock = 'rock' } })
vim.notify = function() end
vim.notify_once = function() end
vim.env.ROCK_BACKEND = nil
vim.env.ROCK_LSP_DEBUG = '/tmp/rock_semtok_caps.log'
os.remove('/tmp/rock_semtok_caps.log')

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
local f = assert(io.open(root .. '/stick.toml', 'w'))
f:write('[package]\nname="x"\nversion="0.1.0"\n'); f:close()
local rp = root .. '/main.rock'
f = assert(io.open(rp, 'w')); f:write('fn main() {\n  print("hi")\n}\n'); f:close()

local client = require('rock.client')
local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  bin = require('rock.binary').resolve(nil)
end
print('ROCK_BIN=' .. tostring(bin))
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')
vim.cmd('edit ' .. vim.fn.fnameescape(rp))

local att
vim.api.nvim_create_autocmd('LspAttach', { callback = function(a)
  local c = vim.lsp.get_client_by_id(a.data.client_id)
  if c and c.name == 'rock' then att = a.data.client_id end
end })
assert(vim.wait(8000, function() return att ~= nil end, 50), 'no LspAttach')
vim.wait(500)

-- What nvim's own semantic-token client supports (its declared capability).
local default = vim.lsp.protocol.make_client_capabilities()
local st = vim.tbl_get(default, 'textDocument', 'semanticTokens')
print('NVIM_DEFAULT_SEMTOK_REQUESTS=' .. vim.json.encode((st or {}).requests or vim.NIL))

-- What this attached client actually advertised.
local c = vim.lsp.get_client_by_id(att)
local cst = c and vim.tbl_get(c.config or {}, 'capabilities', 'textDocument', 'semanticTokens')
print('NVIM_CLIENT_SEMTOK_REQUESTS=' .. vim.json.encode((cst or {}).requests or vim.NIL))

-- Trigger highlighting + an edit, so the built-in semantic-token highlighter
-- issues whatever request kind it actually uses (full / full/delta / range).
vim.lsp.semantic_tokens.start(0, att)
vim.wait(800)
vim.api.nvim_buf_set_lines(0, 1, 2, false, { '  print("hello there")' })
vim.wait(1200)
vim.api.nvim_buf_set_lines(0, 1, 2, false, { '  print("hello there!!")' })
vim.wait(1200)
os.exit(0)
