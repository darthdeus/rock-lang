-- Server-side ranking smoke test through the real LSP. Asserts that
-- the response item list comes back ranked (sort_text in non-decreasing
-- lexicographic order) and that the editor-visible top item for a
-- realistic fuzzy prefix is the obvious match.
--
-- Why this lives in editors/nvim/tests and not rock-lsp/tests: the
-- nvim docker harness drives the *live* LSP server (jsonrpc over
-- stdio), not the in-process `do_completion` API. Per CLAUDE.md "LSP
-- bugs: reproduce through nvim, not the CLI" — server / CLI have
-- drifted before. This spec catches drift in the request/response
-- shape, sort_text encoding, and overall jsonrpc plumbing.

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

-- Server returns `CompletionResponse::Array`; lsp_types deserializes
-- it as a plain table list of items.
local items = response
if type(items) == 'table' and items.items then
  items = items.items -- handle CompletionList shape too
end
if type(items) ~= 'table' or #items == 0 then
  fail('expected non-empty items, got ' .. vim.inspect(response))
end

-- Assertion 1: LOOT_PANEL is present.
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

-- Assertion 2: every item carries sort_text (server is taking
-- responsibility for ranking, not punting to the editor).
for _, it in ipairs(items) do
  if not it.sortText or it.sortText == '' then
    fail('item ' .. tostring(it.label) .. ' has no sortText: ' .. vim.inspect(it))
  end
end

-- Assertion 3: items arrive sorted by sortText (lexicographic). The
-- server already sorts; nvim is allowed to re-sort, but the wire
-- order should be the canonical one.
for i = 2, #items do
  if items[i].sortText < items[i - 1].sortText then
    fail(string.format(
      'items not in sortText order: items[%d].sortText=%s < items[%d].sortText=%s',
      i, items[i].sortText, i - 1, items[i - 1].sortText))
  end
end

-- Assertion 4: LOOT_PANEL is the FIRST item — for prefix `LOOTPA`
-- this is the only candidate that combines a contiguous run of 4 with
-- a word-boundary gap before the remaining 2 chars. The
-- LATER_..._ASTEROID candidate has 6 boundary hits but 0 contiguous
-- chars, so the `*80` contig weight in fuzzy_score must put it
-- behind. If this flips, the score weights drifted and the user-
-- visible win is gone.
if panel_idx ~= 1 then
  fail('LOOT_PANEL must rank first; got order ' .. vim.inspect(labels))
end

print(string.format('OK: LOOTPA -> %d items, top=%s', #items, items[1].label))
os.exit(0)
