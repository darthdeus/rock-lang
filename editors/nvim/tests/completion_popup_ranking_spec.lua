-- Insert-mode popup ranking. Mirrors completion_ranking_spec.lua
-- (response level) but asserts on `complete_info().items` order so
-- we catch the editor ignoring the server's `sortText`. nvim's
-- built-in completion respects sortText only when `completeopt`
-- includes `fuzzy` (the editor re-sorts otherwise) — this spec is
-- the regression check on that interaction.

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

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "rankpop"\nversion = "0.1.0"\n'); f:close()
  -- Two globals share a fuzzy match for prefix `LOOTPA`. LOOT_PANEL
  -- is the obviously-intended one (contiguous run of 4 + boundary
  -- gap before remaining 2). The other has 6 boundary hits but no
  -- contiguous run, so contig*80 must keep LOOT_PANEL on top.
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write([[
global LOOT_PANEL: int = 0
global LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0

fn main() {
    let x = 0
}
]])
  f:close()
end

local rtp = vim.fn.getcwd()
local child = NvimChild.start({ rtp = { rtp } })

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

-- Replace `let x = 0` with `let x = LOOTPA`, cursor at end of
-- prefix, fire omnifunc.
child:rpc('nvim_win_set_cursor', 0, { 4, 4 })
-- The line is "    let x = 0" — `cc` clears, retype with prefix.
child:input('cc    let x = LOOTPA<C-x><C-o>')

local ok = child:wait_for(function()
  return child:call('pumvisible', {}) == 1
end, 3000)
if not ok then
  fail('popup did not appear within 3s; mode='
    .. vim.inspect(child:rpc('nvim_get_mode'))
    .. '\nstderr=' .. table.concat(child.stderr, '\n'))
end

local items = child:call('complete_info', { { 'items', 'selected' } }).items or {}
if #items == 0 then fail('complete_info returned empty items') end

local words = {}
for _, it in ipairs(items) do table.insert(words, it.word) end

-- LOOT_PANEL must be present and ranked before
-- LATER_OUT_OF_TOWN_PERSON_ASTEROID.
local panel_idx, later_idx
for i, w in ipairs(words) do
  if w == 'LOOT_PANEL' then panel_idx = i end
  if w == 'LATER_OUT_OF_TOWN_PERSON_ASTEROID' then later_idx = i end
end
if not panel_idx then
  fail('LOOT_PANEL missing; got ' .. vim.inspect(words))
end
-- The `LATER_..._ASTEROID` candidate may legitimately fall off the
-- popup once nvim's fuzzy filter applies — only assert ordering when
-- both are present.
if later_idx and panel_idx > later_idx then
  fail(('LOOT_PANEL ranked behind LATER_…_ASTEROID (%d > %d); words=%s')
    :format(panel_idx, later_idx, vim.inspect(words)))
end

-- Top item should be LOOT_PANEL — the user-visible win we ship the
-- ranking for. If this flips, completeopt likely lost `fuzzy` (nvim
-- re-sorts) or sort_text encoding drifted.
if panel_idx ~= 1 then
  fail(('LOOT_PANEL must be the top popup item; got order %s'):format(
    vim.inspect(words)))
end

child:stop()
print(string.format('OK: LOOTPA pum top=%s, %d items', words[1], #items))
os.exit(0)
