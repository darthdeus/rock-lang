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

local bin = vim.env.ROCK_LSP_BIN
if not bin or bin == '' then
  print('SKIP: $ROCK_LSP_BIN not set')
  os.exit(0)
end

vim.notify = function() end
vim.notify_once = function() end
vim.filetype.add({ extension = { rock = 'rock' } })

local root = vim.fn.tempname()
vim.fn.mkdir(root, 'p')
do
  local f = assert(io.open(root .. '/stick.toml', 'w'))
  f:write('[package]\nname = "ranking"\nversion = "0.1.0"\n'); f:close()
  -- Two globals share a fuzzy match for prefix `LOOTPA`. `LOOT_PANEL`
  -- is the obviously-intended one. The other has every prefix char
  -- on a word boundary but is much longer with no contiguous run, so
  -- the ranking algorithm must score `LOOT_PANEL` lower (=higher
  -- relevance).
  f = assert(io.open(root .. '/main.rock', 'w'))
  f:write([[
global LOOT_PANEL: int = 0
global LATER_OUT_OF_TOWN_PERSON_ASTEROID: int = 0

fn main() {
    let x = LOOTPA
}
]])
  f:close()
end

local client = require('rock.client')
vim.lsp.config('rock', client.make_config({ binary = bin }))
client.setup_autocmds()
vim.lsp.enable('rock')

vim.cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))
local attached_id
vim.api.nvim_create_autocmd('LspAttach', {
  callback = function(args)
    local c = vim.lsp.get_client_by_id(args.data.client_id)
    if c and c.name == 'rock' then attached_id = args.data.client_id end
  end,
})
if not vim.wait(5000, function() return attached_id ~= nil end, 50) then
  fail('LspAttach did not fire')
end
vim.wait(500, function() return false end)

local bufnr = vim.api.nvim_get_current_buf()

-- Cursor right after `LOOTPA` on line index 4 (0-based), char 18.
-- Line content: "    let x = LOOTPA"
local response, err
vim.lsp.buf_request(bufnr, 'textDocument/completion', {
  textDocument = { uri = vim.uri_from_bufnr(bufnr) },
  position = { line = 4, character = 18 },
  context = { triggerKind = 1 },
}, function(e, result) err = e; response = result end)

if not vim.wait(3000, function() return response ~= nil or err ~= nil end, 25) then
  fail('completion did not respond within 3s')
end
if err then fail('completion returned error: ' .. vim.inspect(err)) end

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
