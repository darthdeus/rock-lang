-- Headless nvim -l cannot enter insert mode; use a child event-loop nvim.

package.path = package.path .. ';' .. vim.fn.getcwd() .. '/tests/?.lua'
local NvimChild = require('_lib.nvim_child')

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local child = NvimChild.open_completion('popup', [[
global LOOT_PANEL: int = 0
global LOOT_TIMER: int = 0

fn main() {
    let x = 0
    LOOT
}
]], false)

-- Cursor at end of "    LOOT" on line 6 (1-based).
child:rpc('nvim_win_set_cursor', 0, { 6, 8 })

-- `a` -> insert mode at end of word; `<C-x><C-o>` fires omnifunc.
child:input('a<C-x><C-o>')

local words = assert(child:popup_words())


if not vim.tbl_contains(words, 'LOOT_PANEL') then
  fail('popup missing LOOT_PANEL; got ' .. vim.inspect(words))
end
if not vim.tbl_contains(words, 'LOOT_TIMER') then
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
print(string.format('OK: pum has %d items, words=%s', #words, table.concat(words, ',')))
os.exit(0)
