-- Neovim respects sortText only with fuzzy completion enabled.

package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local NvimChild = require('_lib.nvim_child')

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local child = NvimChild.open_completion('rankpop', [[
global LOOT_PANEL: int = 0
global LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0

fn main() {
    let x = 0
}
]], false)

child:rpc('nvim_win_set_cursor', 0, { 4, 4 })
child:input('cc    let x = LOOTPA<C-x><C-o>')

local words = assert(child:popup_words())

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

if panel_idx ~= 1 then
  fail(('LOOT_PANEL must be the top popup item; got order %s'):format(
    vim.inspect(words)))
end

child:stop()
print(string.format('OK: LOOTPA pum top=%s, %d items', words[1], #words))
os.exit(0)
