-- Exercise ranking through live JSON-RPC, including wire order and sortText.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end

local project = dofile('tests/_lib/project.lua')
local buf = project.open('ranking', { ['main.rock'] = [[
global LOOT_PANEL: int = 0
global LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0

fn main() {
    let x = LOOTPA
}
]] })
local response = project.request(buf, 'textDocument/completion', {
  position = { line = 4, character = 18 },
  context = { triggerKind = 1 },
})

local items = response
if type(items) == 'table' and items.items then
  items = items.items -- handle CompletionList shape too
end
if type(items) ~= 'table' or #items == 0 then
  fail('expected non-empty items, got ' .. vim.inspect(response))
end

local labels = {}
for _, it in ipairs(items) do table.insert(labels, it.label) end
local function find_label(target)
  for i, l in ipairs(labels) do
    if l == target then return i end
  end
  return nil
end

local panel_idx = find_label('LOOT_PANEL')
if not panel_idx then
  fail('LOOT_PANEL missing from completions; got ' .. vim.inspect(labels))
end

for _, it in ipairs(items) do
  if not it.sortText or it.sortText == '' then
    fail('item ' .. tostring(it.label) .. ' has no sortText: ' .. vim.inspect(it))
  end
end

-- The server supplies canonical sortText order.
for i = 2, #items do
  if items[i].sortText < items[i - 1].sortText then
    fail(string.format(
      'items not in sortText order: items[%d].sortText=%s < items[%d].sortText=%s',
      i, items[i].sortText, i - 1, items[i - 1].sortText))
  end
end

-- This prefix distinguishes contiguous matches from boundary-only matches.
if panel_idx ~= 1 then
  fail('LOOT_PANEL must rank first; got order ' .. vim.inspect(labels))
end

print(string.format('OK: LOOTPA -> %d items, top=%s', #items, items[1].label))
os.exit(0)
