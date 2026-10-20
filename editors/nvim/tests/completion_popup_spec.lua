-- Insert-mode popup assertion via the child-nvim harness. Where the
-- response-level specs assert on `buf_request -> payload`, this one
-- enters real insert mode in a real event-loop nvim, fires omnifunc
-- (`<C-x><C-o>` -> vim.lsp.completion.get -> complete()), and reads
-- `complete_info()` — i.e. exactly what the user would see in the pum.
--
-- Catches a class of regressions response-level specs miss: trigger-
-- character drift, completeopt defaults, source pollution, the editor
-- ignoring sort_text. See `tests/_lib/nvim_child.lua` for why a child
-- nvim is required (headless -l mode never enters insert mode).

package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local NvimChild = require('_lib.nvim_child')

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end

-- Build project for the child to open.
local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "popup"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write([[
global LOOT_PANEL: int = 0
global LOOT_TIMER: int = 0

fn main() {
    let x = 0
    LOOT
}
]])
  f:close()
end

local rtp = vim.fn.getcwd() -- editors/nvim
local child = NvimChild.start({ rtp = { rtp } })

-- Wire the rock client in the child. autotrigger=false because we
-- want the test to drive completion explicitly via <C-x><C-o>;
-- autotrigger fires asynchronously and varies across nvim builds.
child:exec_lua([[
  vim.filetype.add({ extension = { rock = 'rock' } })
  local client = require('rock.client')
  vim.lsp.config('rock', client.make_config({ binary = ... }))
  client.setup_autocmds()
  vim.lsp.enable('rock')
  vim.api.nvim_create_autocmd('LspAttach', { callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then
      vim.lsp.completion.enable(true, c.id, args.buf, { autotrigger = false })
    end
  end })
]], { bin })

child:cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))
child:wait_for_lsp_attach('rock', 5000)

-- Cursor at end of "    LOOT" on line 6 (1-based).
child:rpc('nvim_win_set_cursor', 0, { 6, 8 })

-- `a` -> insert mode at end of word; `<C-x><C-o>` fires omnifunc.
child:input('a<C-x><C-o>')

local ok = child:wait_for(function()
  return child:call('pumvisible', {}) == 1
end, 3000)
if not ok then
  fail('popup did not appear within 3s; mode='
    .. vim.inspect(child:rpc('nvim_get_mode'))
    .. '\nstderr=' .. table.concat(child.stderr, '\n'))
end

local info = child:call('complete_info', { { 'items', 'selected', 'mode' } })
local items = info.items or {}
if #items == 0 then
  fail('complete_info returned no items: ' .. vim.inspect(info))
end

local words = {}
for _, it in ipairs(items) do table.insert(words, it.word) end
local function has(w)
  for _, x in ipairs(words) do if x == w then return true end end
  return false
end

if not has('LOOT_PANEL') then
  fail('popup missing LOOT_PANEL; got ' .. vim.inspect(words))
end
if not has('LOOT_TIMER') then
  fail('popup missing LOOT_TIMER; got ' .. vim.inspect(words))
end

-- Shape guard: every popup item is an L-prefix candidate. Catches
-- buffer-keyword pollution sneaking in from the same file (the `0`
-- in `let x = 0`, etc.) — those wouldn't satisfy `^[Ll]` even if
-- nvim's fuzzy match accepted them.
for _, w in ipairs(words) do
  if not (w:lower()):find('^l') then
    fail('popup contains non-L-prefix item ' .. tostring(w)
      .. '; full items=' .. vim.inspect(words))
  end
end

child:stop()
print(string.format('OK: pum has %d items, words=%s', #items, table.concat(words, ',')))
os.exit(0)
