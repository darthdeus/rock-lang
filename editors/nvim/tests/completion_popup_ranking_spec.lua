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

local child = NvimChild.open_completion('rankpop', [[
global LOOT_PANEL: int = 0
global LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0

fn main() {
    let x = 0
}
]], false)

-- Replace `let x = 0` with `let x = LOOTPA`, cursor at end of
-- prefix, fire omnifunc.
child:rpc('nvim_win_set_cursor', 0, { 4, 4 })
-- The line is "    let x = 0" — `cc` clears, retype with prefix.
child:input('cc    let x = LOOTPA<C-x><C-o>')

local words = assert(child:popup_words())

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
print(string.format('OK: LOOTPA pum top=%s, %d items', words[1], #words))
os.exit(0)
