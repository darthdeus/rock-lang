-- Completion-acceptance logger for rock.nvim.
--
-- Appends one JSONL row per completion event (accept | dismiss) to
-- $XDG_DATA_HOME/nvim/rock/completion-log.jsonl. Pairs nvim-cmp's chosen
-- entry with the raw LSP response (state.last_completion) and a snapshot
-- of cmp's displayed order so we can later answer:
--   * how often is the server's #1 the user's pick?
--   * which comparator step (locality / sort_text / length) actually
--     correlates with acceptance?
--
-- Restricted to rock-filetype buffers (other servers have their own
-- ranking quirks and we don't want noise).
--
-- Opt-in via `setup({ completion_log = true })` in init.lua.
--
-- Event ordering note (cmp internals, confirmed against
-- nvim-cmp/lua/cmp/core.lua and view.lua):
--
--   view:close()         -- emits 'menu_closed' SYNC, also closes the
--                        --   entries view (so get_entries() goes empty)
--   feedkeys → textEdit  -- buffer mutates; prefix gets replaced by label
--   vim.schedule_wrap →
--   emit('confirm_done') -- fires LATER, after the buffer is mutated
--
-- Implications:
--   * We must capture the menu snapshot + buffer prefix BEFORE
--     menu_closed, because by confirm_done the view is empty and the
--     buffer text already contains the accepted label.
--   * We can't tell at menu_closed time whether an accept is coming;
--     instead we stash the snapshot in `pending_close` and defer the
--     dismiss-write so confirm_done can claim it first.

local M = {}

local state = require('rock.state')

local LOG_PATH = nil
local FULL_WHEN_RANK_GT = 10
local TOP_N = 10
local CONTEXT_ABOVE_LINES = 20  -- lines of code above the cursor in each row
local DISMISS_DEFER_MS = 80  -- wait this long before committing a dismiss
-- After a committed dismiss, wait this long, then read what the word at
-- the anchor BECAME (`post_word` -- the typed_out signal).
-- Overridable via setup opts for specs.
local POST_WORD_MS = 600
local TIMELINE_TOP_N = 8   -- labels kept per timeline entry
local TIMELINE_CAP = 40    -- refresh entries kept per popup

-- Suppress repeat write-error toasts.
local notified_write_error = false

-- Last written rows, kept for the <C-g>c bug capture (episode.json).
local recent_rows = {}
local RECENT_ROWS_CAP = 10

-- Live snapshot while a menu is open.
-- {
--   opened_at_hr,
--   entries     = { {label, source, kind, sort_text}, ... },  -- visible order
--   buf         = { uri, line, col, line_text, prefix },      -- pre-confirm
-- }
local live_menu = nil

-- Snapshot moved here at menu_closed. confirm_done consumes it (accept);
-- otherwise after DISMISS_DEFER_MS it gets written as dismiss.
local pending_close = nil
local pending_seq = 0  -- defeats races between deferred fns and new menus

local function iso_ts()
  return os.date('%Y-%m-%dT%H:%M:%S')
end

local function ensure_dir(path)
  vim.fn.mkdir(vim.fs.dirname(path), 'p')
end

local function write_row(row)
  if not LOG_PATH then return end
  table.insert(recent_rows, row)
  if #recent_rows > RECENT_ROWS_CAP then table.remove(recent_rows, 1) end
  local ok, encoded = pcall(vim.json.encode, row)
  if not ok then return end
  -- Disk write off the insert-mode hot path.
  vim.schedule(function()
    local f, err = io.open(LOG_PATH, 'a')
    if not f then
      if not notified_write_error then
        notified_write_error = true
        vim.notify(
          ('rock.nvim: completion_log write failed: %s'):format(tostring(err)),
          vim.log.levels.WARN, { title = 'rock.nvim' })
      end
      return
    end
    f:write(encoded); f:write('\n'); f:close()
  end)
end

-- record_id/index from the server recorder's accept command (attached to
-- items while ROCK_LSP_RECORD is active; rock-lsp completion_record.rs).
-- The join key between this client log and the server corpus.
local function accept_command_info(entry)
  local item = entry and entry.completion_item
  if not item and entry and entry.get_completion_item then
    local ok, got = pcall(function() return entry:get_completion_item() end)
    if ok then item = got end
  end
  local cmd = item and item.command
  if cmd and cmd.command == 'rock.completionAccepted' and type(cmd.arguments) == 'table' then
    return cmd.arguments[1], cmd.arguments[2]
  end
  return nil, nil
end

local function entry_summary(entry)
  if not entry then return nil end
  local item = entry.completion_item
  if not item then
    local ok, got = pcall(function() return entry:get_completion_item() end)
    if ok then item = got end
  end
  local source_name = entry.source and entry.source.name or nil
  return {
    label = item and item.label or nil,
    source = source_name,
    kind = item and item.kind or nil,
    sort_text = item and item.sortText or nil,
  }
end

-- Cheap: cmp.core.view:get_entries() bypasses the cmp.sync(1000ms) wait
-- on cmp.get_entries(); entry.completion_item is a cached field, not a
-- network roundtrip.
local function snapshot_menu(cmp)
  local ok, entries = pcall(function()
    return cmp.core and cmp.core.view and cmp.core.view:get_entries()
  end)
  if not ok or type(entries) ~= 'table' then return nil end
  local out = {}
  for i, e in ipairs(entries) do out[i] = entry_summary(e) end
  return out
end

local function rank_of(list, label)
  if not list or not label then return nil end
  for i, it in ipairs(list) do
    if it and it.label == label then return i end
  end
  return nil
end

local function head(list, n)
  if not list then return nil end
  if #list <= n then return list end
  return vim.list_slice(list, 1, n)
end

-- git HEAD per directory, cached for the session (provenance for replay:
-- the buffer text is authoritative, the sha says what the REST of the
-- workspace looked like). false caches "not a repo" so we don't re-spawn.
local git_head_cache = {}
local function git_head_for(dir)
  if not dir or dir == '' then return nil end
  local cached = git_head_cache[dir]
  if cached ~= nil then return cached or nil end
  local out = vim.fn.system({ 'git', '-C', dir, 'rev-parse', 'HEAD' })
  local head = (vim.v.shell_error == 0) and vim.trim(out) or false
  git_head_cache[dir] = head
  return head or nil
end

-- Read prefix + line from the *current* buffer & cursor. Cheap; called
-- on each cmp 'change' (a few times per popup, not per keystroke
-- universally — cmp emits 'change' on filter refresh).
local function buf_snapshot()
  local bufnr = vim.api.nvim_get_current_buf()
  local row, col = unpack(vim.api.nvim_win_get_cursor(0))  -- row 1-based, col 0-based byte
  local lines = vim.api.nvim_buf_get_lines(bufnr, row - 1, row, false)
  local line = lines[1] or ''
  if col > #line then col = #line end
  -- Walk left through identifier chars for the prefix.
  local i = col
  while i > 0 do
    local c = line:sub(i, i)
    if not c:match('[%w_]') then break end
    i = i - 1
  end
  local prefix = line:sub(i + 1, col)
  -- N lines preceding the cursor (excluding the current line). Joined
  -- with \n so JSONL stays one row per event; consumers can split.
  local above_start = math.max(0, (row - 1) - CONTEXT_ABOVE_LINES)
  local above = vim.api.nvim_buf_get_lines(bufnr, above_start, row - 1, false)
  -- Full buffer, kept as a lines table (concat deferred to the accept
  -- write) so replay can re-run completion on the exact pre-accept text.
  local all_lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local buf_name = vim.api.nvim_buf_get_name(bufnr)
  return {
    bufnr = bufnr,
    uri = vim.uri_from_bufnr(bufnr),
    dir = buf_name ~= '' and vim.fs.dirname(buf_name) or nil,
    line = row - 1,           -- 0-based to match LSP
    col = col,                -- 0-based byte
    line_text = line,
    prefix = prefix,
    -- Episode anchor: where the identifier being completed starts.
    -- Popups here (or nearby, after edits) are the same struggle.
    anchor = { line = row - 1, col = i },
    context_above = table.concat(above, '\n'),
    buffer_lines = all_lines,
  }
end

-- The identifier at `buf.anchor` in the CURRENT buffer -- read after a
-- dismiss settles, revealing what the user typed the word out to be.
-- Fail-open (nil) when the buffer or line is gone.
local function word_at_anchor(buf)
  if not (buf and buf.anchor and buf.bufnr) then return nil end
  if not vim.api.nvim_buf_is_valid(buf.bufnr) then return nil end
  local lines = vim.api.nvim_buf_get_lines(buf.bufnr, buf.anchor.line, buf.anchor.line + 1, false)
  local line = lines[1]
  if not line then return nil end
  local j = buf.anchor.col + 1  -- 1-based scan start
  local e = j
  while e <= #line and line:sub(e, e):match('[%w_]') do e = e + 1 end
  return line:sub(j, e - 1)
end

local function in_rock_buffer()
  return vim.bo.filetype == 'rock'
end

-- Runtime kill switch (for A/B perf testing without restarting nvim).
-- :lua vim.g.rock_completion_log_disabled = true   -- pause
-- :lua vim.g.rock_completion_log_disabled = false  -- resume
-- Also :RockCompletionLog on|off|toggle.
local function disabled()
  return vim.g.rock_completion_log_disabled == true
    or vim.g.rock_completion_log_disabled == 1
end

-- Build the row from a captured (menu, buf) pair, not from live state.
local function build_row(event, menu)
  local s = state.get()
  local lc = s.last_completion or {}
  local lsp_top = {}
  if type(lc.items) == 'table' then
    for i = 1, math.min(#lc.items, TOP_N) do
      local it = lc.items[i]
      lsp_top[i] = {
        label = it and it.label, sort_text = it and it.sortText, kind = it and it.kind,
      }
    end
  end
  local time_to_select_ms = nil
  if menu and menu.opened_at_hr then
    time_to_select_ms = ((vim.uv or vim.loop).hrtime() - menu.opened_at_hr) / 1e6
  end
  local buf = menu and menu.buf or {}
  return {
    v = 1,
    ts = iso_ts(),
    event = event,
    uri = buf.uri or lc.uri,
    lang = 'rock',
    pos = (buf.line ~= nil) and { line = buf.line, character = buf.col } or lc.position,
    prefix = buf.prefix or '',
    anchor = buf.anchor,
    timeline = menu and menu.timeline or nil,
    line_text = buf.line_text or '',
    context_above = buf.context_above or '',
    request_latency_ms = lc.latency_ms,
    time_to_select_ms = time_to_select_ms,
    -- Was the popup auto-triggered (typing through a trigger char,
    -- often `.` or `::`) or invoked manually (<C-Space>)? Recorded
    -- on the LSP side in client.lua. LSP triggerKind: 1=Invoked,
    -- 2=TriggerCharacter, 3=TriggerForIncompleteCompletions.
    trigger_kind = lc.trigger_kind,
    trigger_char = lc.trigger_char,
    lsp = {
      item_count = lc.item_count,
      is_incomplete = lc.is_incomplete,
      top = lsp_top,
    },
    cmp = menu and {
      menu_count = menu.entries and #menu.entries or 0,
      top = head(menu.entries, TOP_N),
    } or nil,
  }
end

-- One timeline entry per popup refresh: when + typed prefix + displayed
-- top labels. The per-refresh history behind the chars-to-top3 metric.
local function timeline_entry(menu, buf, entries)
  local top = {}
  for k = 1, math.min(#(entries or {}), TIMELINE_TOP_N) do
    top[k] = entries[k] and entries[k].label or ''
  end
  local t = 0
  if menu and menu.opened_at_hr then
    t = math.floor(((vim.uv or vim.loop).hrtime() - menu.opened_at_hr) / 1e6)
  end
  return { t = t, prefix = buf and buf.prefix or '', top = top }
end

local function fresh_menu(cmp)
  local menu = {
    opened_at_hr = (vim.uv or vim.loop).hrtime(),
    entries = snapshot_menu(cmp),
    buf = buf_snapshot(),
  }
  menu.timeline = { timeline_entry(menu, menu.buf, menu.entries) }
  return menu
end

local function on_menu_opened(cmp)
  if disabled() or not in_rock_buffer() then return end
  live_menu = fresh_menu(cmp)
end

local function on_change(cmp)
  if disabled() or not in_rock_buffer() then return end
  if not live_menu then
    -- First refresh before menu_opened sometimes; treat as open.
    live_menu = fresh_menu(cmp)
    return
  end
  live_menu.entries = snapshot_menu(cmp)
  live_menu.buf = buf_snapshot()
  if #live_menu.timeline < TIMELINE_CAP then
    table.insert(live_menu.timeline,
      timeline_entry(live_menu, live_menu.buf, live_menu.entries))
  end
end

-- Fires SYNC before textEdit applies. Hand the snapshot off to
-- pending_close, then schedule a deferred dismiss-write. If
-- confirm_done arrives within DISMISS_DEFER_MS, it consumes
-- pending_close first and the deferred write sees nothing.
local function on_menu_closed()
  if disabled() then live_menu = nil; pending_close = nil; return end
  if not in_rock_buffer() then live_menu = nil; pending_close = nil; return end
  if not live_menu or not live_menu.entries or #live_menu.entries == 0 then
    live_menu = nil
    return
  end
  pending_seq = pending_seq + 1
  local my_seq = pending_seq
  pending_close = live_menu
  live_menu = nil
  vim.defer_fn(function()
    if pending_seq == my_seq and pending_close then
      local menu = pending_close
      pending_close = nil
      -- Committed as a dismiss; wait for the user's next keystrokes to
      -- reveal what the word BECAME before writing (post_word -- the
      -- typed_out signal).
      vim.defer_fn(function()
        local row = build_row('dismiss', menu)
        row.post_word = word_at_anchor(menu.buf)
        -- A changed word means the ranker had its chance and lost; that
        -- case must be replayable, so it carries the popup-time buffer.
        -- Unchanged flicker dismisses (~90% of rows) stay light.
        if row.post_word and menu.buf and row.post_word ~= menu.buf.prefix
           and menu.buf.buffer_lines then
          row.buffer_text = table.concat(menu.buf.buffer_lines, '\n')
        end
        write_row(row)
      end, POST_WORD_MS)
    end
  end, DISMISS_DEFER_MS)
end

local function on_confirm_done(evt)
  if disabled() or not in_rock_buffer() then return end
  local chosen = entry_summary(evt and evt.entry)
  if not chosen or not chosen.label then return end
  -- Claim the pending dismiss before its timer fires.
  local menu = pending_close
  pending_close = nil
  pending_seq = pending_seq + 1  -- invalidate the deferred dismiss

  local row = build_row('accept', menu)
  local rank_lsp = rank_of(row.lsp.top, chosen.label)
  local rank_cmp = rank_of(row.cmp and row.cmp.top, chosen.label)
  if not rank_lsp then
    local items = (state.get().last_completion or {}).items or {}
    for i, it in ipairs(items) do
      if it and it.label == chosen.label then rank_lsp = i; break end
    end
  end
  local record_id, server_index = accept_command_info(evt and evt.entry)
  row.chosen = {
    label = chosen.label,
    source = chosen.source,
    kind = chosen.kind,
    sort_text = chosen.sort_text,
    rank_in_lsp = rank_lsp,
    rank_in_cmp = rank_cmp,
    record_id = record_id,
    server_index = server_index,
  }
  -- Accept rows only: the full pre-accept buffer (replay input) + git HEAD
  -- (workspace provenance). Dismisses are ~90% of rows and never replayed.
  if menu and menu.buf and menu.buf.buffer_lines then
    row.buffer_text = table.concat(menu.buf.buffer_lines, '\n')
    row.git_head = git_head_for(menu.buf.dir)
  end
  -- Buried picks: swap top-N for full lists so we can debug ranking.
  if rank_lsp and rank_lsp > FULL_WHEN_RANK_GT then
    local items = (state.get().last_completion or {}).items or {}
    local full = {}
    for i, it in ipairs(items) do
      full[i] = { label = it.label, sort_text = it.sortText, kind = it.kind }
    end
    row.lsp.top = full
    if menu and menu.entries then row.cmp.top = menu.entries end
  end
  write_row(row)
end

function M.setup(opts)
  local ok, cmp = pcall(require, 'cmp')
  if not ok then
    vim.notify(
      'rock.nvim: completion_log enabled but nvim-cmp not found; logging disabled.',
      vim.log.levels.WARN, { title = 'rock.nvim' })
    return
  end

  LOG_PATH = (opts and opts.completion_log_path)
    or (vim.fn.stdpath('data') .. '/rock/completion-log.jsonl')
  ensure_dir(LOG_PATH)
  POST_WORD_MS = (opts and opts.completion_log_post_word_ms) or POST_WORD_MS

  -- Subscribe to 'change' too: now that we bypass cmp.sync (the 1s wait
  -- that caused insert lag), snapshot_menu is microseconds. Capturing
  -- the buffer prefix + entries on every filter refresh is the only way
  -- to get an accurate at-accept snapshot, because by confirm_done the
  -- view is closed and the buffer has been mutated.
  M._cmp = cmp
  cmp.event:on('menu_opened', function() on_menu_opened(cmp) end)
  cmp.event:on('change', function() on_change(cmp) end)
  cmp.event:on('confirm_done', on_confirm_done)
  cmp.event:on('menu_closed', on_menu_closed)
end

-- Snapshot for the bug capture (capture.lua -> episode.json): the last
-- written rows (the struggle so far) plus any in-flight popup state.
-- Read synchronously at capture time; cmp events mutate this constantly.
function M.episode_snapshot()
  local function menu_view(m)
    if not m then return nil end
    return {
      timeline = m.timeline,
      prefix = m.buf and m.buf.prefix or nil,
      anchor = m.buf and m.buf.anchor or nil,
      uri = m.buf and m.buf.uri or nil,
    }
  end
  return {
    live = menu_view(live_menu),
    pending = menu_view(pending_close),
    recent = recent_rows,
  }
end

-- Exposed for tests.
M._internal = {
  rank_of = rank_of,
  buf_snapshot = buf_snapshot,
}

return M
