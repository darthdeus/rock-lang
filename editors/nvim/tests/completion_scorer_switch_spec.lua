-- Live `rock/setFuzzyScorer` request round-trip + behavioral proof.
-- Default scorer is `hybrid`. We:
--  1. Query the active scorer (no params) — expect hybrid.
--  2. Switch to numeric — expect the response to confirm.
--  3. Re-query — expect numeric.
--  4. Run a completion at a prefix where numeric and hybrid pick
--     different top items, assert the active scorer's expected top.
--  5. Switch back to hybrid, repeat the completion, assert the other
--     top.
--
-- The behavioral case uses `mh` against {MAX_HP, mob_hit, mob_health_cur}:
-- - hybrid: MAX_HP wins (length tiebreak among acronym matches).
-- - numeric: mob_hit wins (the original tuned weights favored
--   contiguous runs and never gave MAX_HP enough boost to outrank
--   the longer snake-case match).
--
-- This spec is the canary for both the wire-level switch and any
-- future scorer-tuning work — if the divergence stops being
-- observable, the scorers have drifted to be equivalent on this
-- shape and the spec gives clear feedback.

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
  f:write('[package]\nname = "scorer"\nversion = "0.1.0"\n'); f:close()
  f = assert(io.open(root .. '/main.rock', 'w'))
  -- Three camelCase identifiers all roughly matching prefix `pSE` as
  -- an acronym. The scorers diverge on this shape:
  -- - numeric: case-insensitive walk hits the lowercase `s` and
  --   `e` early in `parseSimpleEnum` and gets a contig bonus,
  --   ranking it ahead of the proper camel-cap match.
  -- - hybrid (fzf DP): finds the optimal alignment hitting only
  --   capital S and E, picking the shortest such match (payStubEntry).
  f:write([[
fn parseSimpleEnum() -> int { 0 }
fn parseStringExpression() -> int { 0 }
fn payStubEntry() -> int { 0 }

fn main() {
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
]], { bin })

child:cmd('edit ' .. vim.fn.fnameescape(root .. '/main.rock'))
local cid = child:wait_for_lsp_attach('rock', 5000)

-- Helper: send rock/setFuzzyScorer; returns the response.
-- Pass args as a single-element list (table) to avoid Lua's
-- trailing-nil-drops-from-vararg behaviour: `{cid, nil}` becomes a
-- one-element list, and the child sees `scorer = nil` for the cid.
local function set_scorer(scorer_or_nil)
  local arg = { cid = cid, scorer = scorer_or_nil }
  return child:exec_lua([[
    local a = ...
    local c = vim.lsp.get_client_by_id(a.cid)
    if not c then return { __err = 'no client for id ' .. tostring(a.cid) } end
    local params = a.scorer and { scorer = a.scorer } or vim.empty_dict()
    local result, err
    c:request('rock/setFuzzyScorer', params, function(e, r)
      err = e; result = r
    end)
    vim.wait(2000, function() return result ~= nil or err ~= nil end, 25)
    if err then return { __err = vim.inspect(err) } end
    return result
  ]], { arg })
end

-- (1) Query default — expect hybrid.
local r = set_scorer(nil)
if not r or r.scorer ~= 'hybrid' then
  fail('default scorer should be hybrid; got ' .. vim.inspect(r))
end

-- (2) Switch to numeric.
r = set_scorer('numeric')
if not r or r.scorer ~= 'numeric' then
  fail('switch to numeric did not stick; got ' .. vim.inspect(r))
end

-- (3) Re-query — confirm.
r = set_scorer(nil)
if not r or r.scorer ~= 'numeric' then
  fail('after switch, query reports wrong scorer; got ' .. vim.inspect(r))
end

-- Helper: fire a completion via buf_request, return the top label.
-- We use the response-level path here (not the pum) because nvim's
-- built-in `fuzzy` completeopt re-ranks items client-side and would
-- mask the server-side scorer change. The popup ranking is covered
-- separately by completion_popup_ranking_spec.lua.
local function top_for_prefix(prefix)
  -- Drop a request at the function body line (line 5 0-based, char
  -- after the prefix we'll send). We don't actually need to type —
  -- we just place the cursor where the prefix would land. Insert
  -- the prefix into the buffer first so the server sees it.
  child:rpc('nvim_win_set_cursor', 0, { 5, 0 })
  child:cmd('normal! cc')
  child:exec_lua([[
    local p = ...
    vim.api.nvim_buf_set_lines(0, 4, 5, false, { '    ' .. p })
  ]], { prefix })
  -- Position cursor at end of prefix on line 5 (0-based 4).
  child:rpc('nvim_win_set_cursor', 0, { 5, 4 + #prefix })
  -- Request completion at that position.
  local result = child:exec_lua([[
    local prefix = ...
    local res, err
    vim.lsp.buf_request(0, 'textDocument/completion', {
      textDocument = { uri = vim.uri_from_bufnr(0) },
      position = { line = 4, character = 4 + #prefix },
      context = { triggerKind = 1 },
    }, function(e, r) err = e; res = r end)
    vim.wait(3000, function() return res ~= nil or err ~= nil end, 25)
    if err then return { __err = vim.inspect(err) } end
    if type(res) == 'table' and res.items then res = res.items end
    return res
  ]], { prefix })
  if not result or result.__err then
    return nil, 'request failed: ' .. vim.inspect(result)
  end
  -- Filter to just our three test idents (the response also has
  -- `main` and stdlib symbols).
  local wanted = { parseSimpleEnum=true, parseStringExpression=true, payStubEntry=true }
  local filtered = {}
  for _, it in ipairs(result) do
    if wanted[it.label] then table.insert(filtered, it) end
  end
  -- Honor server's sortText.
  table.sort(filtered, function(a, b)
    return (a.sortText or a.label) < (b.sortText or b.label)
  end)
  return filtered[1] and filtered[1].label, filtered
end

-- (4) Active = numeric; expect parseSimpleEnum at top for `pSE`.
-- The numeric subseq walk catches the lowercase `s`+`e` at indices
-- 3,4 of `parseSimpleEnum` and gets a contig bonus, beating the
-- proper camel-cap match.
local top, items = top_for_prefix('pSE')
if not top then
  fail('numeric: ' .. tostring(items))
end
if top ~= 'parseSimpleEnum' then
  fail(('numeric: expected top=parseSimpleEnum for prefix pSE, got %s; items=%s')
    :format(top, vim.inspect(items)))
end

-- (5) Switch to hybrid; same prefix; expect payStubEntry at top.
r = set_scorer('hybrid')
if not r or r.scorer ~= 'hybrid' then
  fail('switch back to hybrid did not stick; got ' .. vim.inspect(r))
end

top, items = top_for_prefix('pSE')
if not top then
  fail('hybrid: ' .. tostring(items))
end
if top ~= 'payStubEntry' then
  fail(('hybrid: expected top=payStubEntry for prefix pSE, got %s; items=%s')
    :format(top, vim.inspect(items)))
end

child:stop()
print('OK: scorer switch round-trips and behavior changes as expected')
os.exit(0)
