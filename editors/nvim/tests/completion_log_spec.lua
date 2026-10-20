-- Drives rock.completion_log through a fake nvim-cmp so we cover the
-- real event-subscription path without rock-lsp or nvim-cmp installed.
--
-- Catches three regressions that bit the live logger:
--   1. Subscribing to a non-existent cmp event ('complete') silently
--      no-ops; the spec asserts the four real events are subscribed.
--   2. Per-keystroke 'change' was reverted once for perf — but with
--      cmp.sync bypassed it's cheap again and required to capture
--      accurate at-accept state. Spec asserts 'change' is subscribed.
--   3. menu_closed fires BEFORE confirm_done in cmp, so the naive
--      state machine wrote a spurious dismiss row for every accept.
--      Spec asserts an accept produces ONE row (not dismiss+accept).
--   4. confirm_done fires after textEdit mutates the buffer, so prefix
--      read at that point is wrong. Spec asserts the row's prefix
--      matches what the user had TYPED, not the inserted label.

local function fail(msg)
  io.stderr:write('FAIL: ' .. msg .. '\n')
  os.exit(1)
end
local function assert_eq(actual, expected, what)
  if actual ~= expected then
    fail(('%s: expected %s, got %s'):format(what, vim.inspect(expected), vim.inspect(actual)))
  end
end

vim.notify = function() end
vim.notify_once = function() end

-- ---- Fake cmp -------------------------------------------------------
local handlers = {}
local entries_seq = {}

local function fake_entry(label, sort_text, kind, command)
  return {
    completion_item = { label = label, sortText = sort_text, kind = kind, command = command },
    get_completion_item = function(self) return self.completion_item end,
    source = { name = 'rock-lsp' },
  }
end

local fake_view = { get_entries = function(_) return entries_seq end }
local fake_cmp = {
  event = {
    on = function(_, name, fn)
      handlers[name] = handlers[name] or {}
      table.insert(handlers[name], fn)
    end,
  },
  core = { view = fake_view },
  get_entries = function() return entries_seq end,
}
package.loaded['cmp'] = fake_cmp

local function emit(name, evt)
  for _, fn in ipairs(handlers[name] or {}) do fn(evt) end
end

-- ---- Set up logger -------------------------------------------------
local tmp_dir = vim.fn.tempname()
vim.fn.mkdir(tmp_dir, 'p')
local log_path = tmp_dir .. '/log.jsonl'

local state = require('rock.state')
require('rock.completion_log').setup({
  completion_log = true,
  completion_log_path = log_path,
  -- Speed the deferred post-dismiss word read up for the spec.
  completion_log_post_word_ms = 120,
})

-- All four real cmp events must be subscribed.
for _, name in ipairs({ 'menu_opened', 'change', 'menu_closed', 'confirm_done' }) do
  if not handlers[name] then fail('logger did not subscribe to ' .. name) end
end
if handlers['complete'] then
  fail('logger subscribed to bogus "complete" event')
end

-- ---- Open a .rock buffer with the prefix the user "typed" ----------
vim.bo.filetype = 'rock'
local fname = tmp_dir .. '/main.rock'
local f = assert(io.open(fname, 'w'))
-- Trailing space keeps "    let abc = mob_k" usable as if user is mid-typing;
-- without it nvim_win_set_cursor clamps col past EOL outside insert mode.
f:write('fn main() {\n    let abc = mob_k \n}\n'); f:close()
-- Git repo around the buffer so accept rows can stamp provenance.
vim.fn.system({ 'git', '-C', tmp_dir, 'init', '-q' })
vim.fn.system({ 'git', '-C', tmp_dir, 'add', 'main.rock' })
vim.fn.system({ 'git', '-C', tmp_dir, '-c', 'user.email=spec@rock', '-c', 'user.name=spec',
  'commit', '-q', '-m', 'seed' })
local git_head = vim.trim(vim.fn.system({ 'git', '-C', tmp_dir, 'rev-parse', 'HEAD' }))
vim.cmd('edit ' .. vim.fn.fnameescape(fname))
vim.bo.filetype = 'rock'
-- Cursor just after "mob_k" (between 'k' and the trailing space), col 19.
vim.api.nvim_win_set_cursor(0, { 2, 19 })

-- last_completion mimics what client.lua stores after an LSP response.
state.set_completion({
  at = os.time(),
  latency_ms = 7,
  position = { line = 1, character = 13 },  -- where LSP was queried (start of "m")
  uri = vim.uri_from_fname(fname),
  trigger_kind = 2,                          -- TriggerCharacter
  trigger_char = '.',
  item_count = 4,
  is_incomplete = false,
  items = {
    { label = 'MobKind::Imp',  sortText = '0~Imp',  kind = 13 },
    { label = 'MobKind::Orc',  sortText = '0~Orc',  kind = 13 },
    { label = 'mob_kind_data', sortText = '5~mkd',  kind = 6  },
    { label = 'mob_count',     sortText = '5~mc',   kind = 6  },
  },
})

-- ---- 1. Accept path: menu_opened, change, menu_closed, confirm_done ---
-- The picked entry carries the server recorder's accept command
-- (ROCK_LSP_RECORD tagging); its record_id must land in the row as the
-- join key between this client log and the server corpus.
entries_seq = {
  fake_entry('MobKind::Imp',  '0~Imp', 13),
  fake_entry('mob_kind_data', '5~mkd', 6, {
    command = 'rock.completionAccepted',
    arguments = { 42, 2, 'mob_kind_data' },
  }),
  fake_entry('mob_count',     '5~mc',  6),
}
emit('menu_opened', {})
emit('change')
-- User picks mob_kind_data; cmp fires menu_closed FIRST, then mutates
-- the buffer (we simulate that), then schedules confirm_done.
emit('menu_closed', {})
-- Simulate the textEdit applying: buffer now contains "mob_kind_data".
vim.api.nvim_buf_set_lines(0, 1, 2, false, { '    let abc = mob_kind_data' })
vim.api.nvim_win_set_cursor(0, { 2, 25 })  -- end of "mob_kind_data"
emit('confirm_done', { entry = entries_seq[2] })

-- Wait for vim.defer_fn (the dismiss timer) + vim.schedule (the write).
-- The accept should have invalidated the pending dismiss, so only ONE
-- row should appear.
local function read_rows()
  local fh = io.open(log_path, 'r'); if not fh then return {} end
  local out = {}
  for line in fh:lines() do table.insert(out, vim.json.decode(line)) end
  fh:close(); return out
end
if not vim.wait(500, function() return #read_rows() >= 1 end, 10) then
  fail('expected at least 1 row, got 0')
end
-- Give the dismiss timer a chance to fire so we'd see a spurious row.
vim.wait(200, function() return false end, 20)

local rows = read_rows()
assert_eq(#rows, 1, 'accept produced exactly one row (not dismiss + accept)')

local r = rows[1]
assert_eq(r.event, 'accept', 'event')
assert_eq(r.lang, 'rock', 'lang')
-- The buffer-state capture must come from BEFORE the textEdit:
assert_eq(r.prefix, 'mob_k', 'prefix at accept time (not post-insertion label)')
assert_eq(r.line_text, '    let abc = mob_k ', 'line_text pre-insertion')
-- context_above should include the line ABOVE the current one
-- ("fn main() {") and exclude the current line itself.
assert_eq(r.context_above, 'fn main() {', 'context_above contains preceding line(s)')
assert_eq(r.trigger_kind, 2, 'trigger_kind surfaced (TriggerCharacter)')
assert_eq(r.trigger_char, '.', 'trigger_char surfaced')
assert_eq(r.chosen.label, 'mob_kind_data', 'chosen.label')
assert_eq(r.chosen.rank_in_lsp, 3, 'rank_in_lsp')
assert_eq(r.chosen.record_id, 42, 'record_id from the accept command (server join key)')
assert_eq(r.chosen.server_index, 2, 'server_index from the accept command')
-- cmp block must NOT be nil (the bug: view was closed by confirm time).
if not r.cmp then fail('cmp block is nil — pending_close not consumed') end
assert_eq(r.cmp.menu_count, 3, 'cmp.menu_count from pending_close snapshot')
assert_eq(r.chosen.rank_in_cmp, 2, 'rank_in_cmp')
-- Replay corpus needs the FULL pre-insertion buffer on accept rows: the
-- textEdit already applied by confirm_done, so this must come from the
-- pre-close snapshot, not the live buffer.
assert_eq(r.buffer_text, 'fn main() {\n    let abc = mob_k \n}',
  'buffer_text is the full pre-insertion buffer')
assert_eq(r.git_head, git_head, 'git_head stamps workspace provenance')
-- Episode fields: the word anchor and the per-refresh timeline behind
-- chars-to-top3.
assert_eq(r.anchor.line, 1, 'anchor.line (0-based)')
assert_eq(r.anchor.col, 14, 'anchor.col = identifier start of "mob_k"')
if type(r.timeline) ~= 'table' then fail('timeline missing on accept row') end
assert_eq(#r.timeline, 2, 'one timeline entry per refresh (menu_opened + change)')
assert_eq(r.timeline[1].prefix, 'mob_k', 'timeline entry prefix')
assert_eq(r.timeline[1].top[1], 'MobKind::Imp', 'timeline entry top labels')
assert_eq(type(r.timeline[2].t), 'number', 'timeline t is ms since open')

-- ---- 2. Dismiss path -> typed_out: user finishes the word by hand ---
entries_seq = {
  fake_entry('alpha', '0~a', 6),
  fake_entry('beta',  '0~b', 6),
}
vim.api.nvim_buf_set_lines(0, 1, 2, false, { '    let x = al ' })
vim.api.nvim_win_set_cursor(0, { 2, 14 })  -- cursor between 'l' and trailing space
emit('menu_opened', {})
emit('menu_closed', {})
-- No confirm_done; the user keeps typing the identifier by hand. The
-- deferred post_word read (completion_log_post_word_ms) must see it.
vim.api.nvim_buf_set_lines(0, 1, 2, false, { '    let x = alpha_beta ' })
vim.api.nvim_win_set_cursor(0, { 2, 22 })
if not vim.wait(800, function() return #read_rows() >= 2 end, 10) then
  fail('dismiss row never landed; got ' .. #read_rows() .. ' rows')
end
local rows2 = read_rows()
assert_eq(#rows2, 2, 'one dismiss row added')
assert_eq(rows2[2].event, 'dismiss', 'row2 event')
assert_eq(rows2[2].prefix, 'al', 'row2 prefix')
assert_eq(rows2[2].anchor.col, 12, 'row2 anchor at "al" start')
-- typed_out signal: what the word at the anchor BECAME after the popup
-- closed -- the ground truth the accept log can never see.
assert_eq(rows2[2].post_word, 'alpha_beta', 'post_word read at the anchor')
-- A typed-out candidate must be replayable: it carries the popup-time
-- buffer (pre-close snapshot, NOT the post-typing state).
assert_eq(rows2[2].buffer_text, 'fn main() {\n    let x = al \n}',
  'typed-out dismiss carries the popup-time buffer')

-- ---- 2b. Dismiss with no follow-up typing stays light ---------------
vim.api.nvim_buf_set_lines(0, 1, 2, false, { '    let x = al ' })
vim.api.nvim_win_set_cursor(0, { 2, 14 })
emit('menu_opened', {})
emit('menu_closed', {})
if not vim.wait(800, function() return #read_rows() >= 3 end, 10) then
  fail('flicker-dismiss row never landed; got ' .. #read_rows() .. ' rows')
end
local rows3 = read_rows()
assert_eq(rows3[3].event, 'dismiss', 'row3 event')
assert_eq(rows3[3].post_word, 'al', 'post_word == prefix when nothing more typed')
-- Flicker dismisses are ~90% of rows; carrying the buffer there would
-- bloat the log for no replay value.
assert_eq(rows3[3].buffer_text, nil, 'unchanged dismiss carries no buffer_text')
assert_eq(rows3[3].git_head, nil, 'dismiss rows carry no git_head')

-- ---- 2c. Episode snapshot for the bug-capture hotkey -----------------
-- capture.lua dumps this as episode.json: the last written rows (the
-- struggle so far) plus any in-flight popup.
local snap = require('rock.completion_log').episode_snapshot()
if type(snap) ~= 'table' or type(snap.recent) ~= 'table' then
  fail('episode_snapshot missing recent rows: ' .. vim.inspect(snap))
end
assert_eq(snap.recent[1].event, 'accept', 'recent ring holds written rows in order')
assert_eq(snap.recent[#snap.recent].event, 'dismiss', 'latest row last')

-- ---- 3. Non-rock buffer is filtered out ----------------------------
vim.bo.filetype = 'lua'
emit('menu_opened', {})
emit('menu_closed', {})
emit('confirm_done', { entry = fake_entry('nope', '', 6) })
vim.bo.filetype = 'rock'
vim.wait(400, function() return false end, 20)
assert_eq(#read_rows(), 3, 'non-rock events ignored')

print('PASS  completion_log_spec')
os.exit(0)
