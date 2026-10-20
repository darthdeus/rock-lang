-- Right-side sidebar that shows live rock.nvim state.
--
-- Auto-opens on first .rock buffer. Refreshes whenever state changes
-- (via subscription) plus a 1000ms timer for uptime/relative-time.

local state = require('rock.state')
local client = require('rock.client')

local M = {}

local WIDTH = 50
local SIDE = 'left'  -- 'right' | 'left'
local FILTER_MODES = { 'all', 'no-progress', 'requests-only', 'notifications-only' }
local FILTER_IDX = 1
local FILTER_SUBSTR = nil  -- nil or a method-substring filter
local PANEL_BUF = nil
local PANEL_WIN = nil
local TIMER = nil
local UNSUB = nil
local RENDER_PENDING = false  -- coalesce notify() bursts into one render/tick
local LINE_TO_EVENT = {}  -- map buffer line -> event id, for expand

local function pad(s, n)
  s = tostring(s)
  if #s >= n then return s:sub(1, n) end
  return s .. string.rep(' ', n - #s)
end

local function rel_time(t)
  if not t then return '-' end
  local d = os.time() - t
  if d < 60 then return d .. 's ago' end
  if d < 3600 then return math.floor(d / 60) .. 'm ago' end
  return math.floor(d / 3600) .. 'h ago'
end

local function uptime(t)
  if not t then return '-' end
  local d = os.time() - t
  local h = math.floor(d / 3600)
  local m = math.floor((d % 3600) / 60)
  local s = d % 60
  if h > 0 then return ('%dh%02dm'):format(h, m) end
  if m > 0 then return ('%dm%02ds'):format(m, s) end
  return ('%ds'):format(s)
end

local function fmt_ago(ms)
  -- JSON null decodes to vim.NIL (truthy userdata), not nil -- bone always
  -- sends ago_ms null, so guard on the type, not on falsiness.
  if type(ms) ~= 'number' then return '-' end
  if ms < 1000 then return ('%dms'):format(ms) end
  if ms < 60000 then return ('%.1fs'):format(ms / 1000) end
  return ('%dm'):format(math.floor(ms / 60000))
end

local function short_path(p, max)
  if not p then return '-' end
  max = max or WIDTH - 4
  if #p <= max then return p end
  return '...' .. p:sub(-(max - 3))
end

local function format_payload_oneline(p)
  if p == nil then return '' end
  if type(p) ~= 'table' then return tostring(p) end
  local ok, encoded = pcall(vim.fn.json_encode, p)
  if not ok then return '<unencodable>' end
  if #encoded > 40 then encoded = encoded:sub(1, 40) .. '...' end
  return encoded
end

local function collect_lines()
  local s = state.get()
  local lines = {}
  local hl = {}  -- { {line_idx, hl_group, col_start, col_end} }
  LINE_TO_EVENT = {}

  local function push(line) table.insert(lines, line); return #lines - 1 end
  local function section(title)
    push('')
    local idx = push(title)
    table.insert(hl, { idx, 'Title', 0, -1 })
  end

  push('  [s] swap side  [q] close  [r] restart  [?] help')
  table.insert(hl, { #lines - 1, 'Comment', 0, -1 })

  section(' BINARY')
  push('  ' .. short_path(s.binary.path, WIDTH - 4))
  if s.binary.exists then
    push(('  mtime %s (%s)  size %s'):format(
      s.binary.mtime and os.date('%H:%M:%S', s.binary.mtime) or '-',
      rel_time(s.binary.mtime),
      s.binary.size and string.format('%.1fM', s.binary.size / 1024 / 1024) or '-'
    ))
    push('  id ' .. (s.binary.sha8 or '-'))
  else
    local idx = push('  (binary does not exist)')
    table.insert(hl, { idx, 'ErrorMsg', 0, -1 })
  end
  if s.binary.mismatch then
    local idx = push('  MISMATCH')
    table.insert(hl, { idx, 'ErrorMsg', 0, -1 })
    push('    cwd expects: ' .. short_path(s.binary.worktree_expected, WIDTH - 17))
    push('    binary from: ' .. short_path(s.binary.worktree_actual, WIDTH - 17))
  end

  section(' CLIENT')
  local cstate_hl = 'DiagnosticOk'
  if s.client.state == 'crashed' then cstate_hl = 'DiagnosticError'
  elseif s.client.state == 'detached' then cstate_hl = 'Comment'
  elseif s.client.state == 'starting' then cstate_hl = 'DiagnosticWarn' end
  local idx = push(('  state: %s   up %s   restarts %d'):format(
    s.client.state or '-',
    uptime(s.client.started_at),
    s.client.restart_count
  ))
  table.insert(hl, { idx, cstate_hl, 9, 9 + #tostring(s.client.state or '-') })
  push(('  id %s'):format(tostring(s.client.id or '-')))
  if s.client.last_exit then
    push(('  last exit: code=%d sig=%d (%s)'):format(
      s.client.last_exit.code or -1,
      s.client.last_exit.signal or 0,
      rel_time(s.client.last_exit.at)
    ))
  end

  -- ERRORS: silent server errors captured from window/logMessage and
  -- window/showMessage notifications with Error/Warning severity. Shown
  -- prominently above everything else except BINARY/CLIENT because a
  -- silent error is usually the load-bearing signal for "why is it
  -- broken". 'E' in panel clears.
  if s.errors and #s.errors > 0 then
    section((' ERRORS (%d, newest first)  [E] clear'):format(#s.errors))
    local shown = math.min(#s.errors, 8)
    for i = 1, shown do
      local e = s.errors[i]
      local label = (e.severity == 1) and 'ERR' or 'WARN'
      local hlgrp = (e.severity == 1) and 'DiagnosticError' or 'DiagnosticWarn'
      local msg = e.message or ''
      if #msg > WIDTH - 20 then msg = msg:sub(1, WIDTH - 23) .. '...' end
      local idx = push(('  %s [%s] %s'):format(e.wall, label, msg))
      table.insert(hl, { idx, hlgrp, 0, -1 })
    end
    if #s.errors > shown then push(('    (+%d older)'):format(#s.errors - shown)) end
  end

  -- DOCTOR summary: counts + worst-severity rows. Non-PASS rows render
  -- in full so the bad thing is visible at a glance; PASS rows roll up.
  if s.doctor then
    local counts = { PASS = 0, WARN = 0, FAIL = 0 }
    for _, r in ipairs(s.doctor) do counts[r.status] = (counts[r.status] or 0) + 1 end
    section((' DOCTOR  %d pass / %d warn / %d fail  (:RockLspDoctor for detail)'):format(
      counts.PASS or 0, counts.WARN or 0, counts.FAIL or 0))
    for _, r in ipairs(s.doctor) do
      if r.status == 'WARN' or r.status == 'FAIL' then
        local hlgrp = (r.status == 'FAIL') and 'DiagnosticError' or 'DiagnosticWarn'
        local idx = push(('  [%s] %s'):format(r.status, r.msg))
        table.insert(hl, { idx, hlgrp, 0, -1 })
        if r.detail then push('        ' .. r.detail) end
      end
    end
  end

  section((' BUFFERS (%d)'):format(vim.tbl_count(s.buffers)))
  for bufnr, info in pairs(s.buffers) do
    local marker = info.attached and '*' or '.'
    local name = info.name and vim.fn.fnamemodify(info.name, ':t') or ('buf ' .. bufnr)
    push(('  %s %-3d %s'):format(marker, bufnr, name))
  end

  -- CAPTURES: bug-diagnosis snapshots from <C-g>c. Newest first, most
  -- recent ~5 shown. Format is tmux-copy-friendly: each line stands alone.
  if s.captures and #s.captures > 0 then
    section((' CAPTURES (%d, newest first)'):format(#s.captures))
    local shown = math.min(#s.captures, 5)
    for i = 1, shown do
      local c = s.captures[i]
      push(('  %s  %s:%d:%d'):format(c.wall or '-', c.basename or '-',
        c.cursor and c.cursor.row or 0, c.cursor and (c.cursor.col + 1) or 0))
      local tp = c.typed_prefix or ''
      if #tp > WIDTH - 12 then tp = tp:sub(-(WIDTH - 15)) .. '...' end
      push(('    prefix |%s|'):format(tp))
      if c.last_completion then
        push(('    last completion: items=%d  lat=%.1fms  age=%ds'):format(
          c.last_completion.item_count or 0,
          c.last_completion.latency_ms or 0,
          c.last_completion.age_s or 0))
      end
      push('    ' .. (c.dir or '-'))
    end
    if #s.captures > shown then push(('    (+%d more)'):format(#s.captures - shown)) end
  end

  -- PERF: compact per-method latency. Full detail: <leader>rt / :RockPerf.
  do
    local perf = require('rock.perf')
    local list = {}
    for method, t in pairs(s.request_timings or {}) do
      local stats = perf.summarize(t.samples)
      if stats then table.insert(list, { method = method, stats = stats }) end
    end
    if #list > 0 then
      table.sort(list, function(a, b) return a.stats.n > b.stats.n end)
      section(' PERF  (<leader>rt for detail)')
      push(('  %-22s %5s %6s %6s'):format('method', 'n', 'p50', 'p95'))
      local shown = math.min(#list, 10)
      for i = 1, shown do
        local e = list[i]
        local name = e.method:gsub('textDocument/', 'td/'):sub(1, 22)
        push(('  %-22s %5d %6.1f %6.1f'):format(
          name, e.stats.n, e.stats.p50, e.stats.p95))
      end
      if #list > shown then push(('    (+%d more)'):format(#list - shown)) end
    end
  end

  -- MODULES: per-client, per-context counts. Each rock-lsp process
  -- has its own workspace so they render as separate sub-sections.
  -- Full detail: <leader>rm / :RockModules.
  local module_client_ids = {}
  for cid in pairs(s.modules or {}) do table.insert(module_client_ids, cid) end
  table.sort(module_client_ids)
  for _, cid in ipairs(module_client_ids) do
    local m = s.modules[cid]
    if m then
      section((' MODULES client=%d  (%d total, %d contexts)'):format(
        cid, m.total_modules or 0, #(m.contexts or {})))
      for _, ctx in ipairs(m.contexts or {}) do
        local root_tail = ctx.root and vim.fn.fnamemodify(ctx.root, ':t') or '-'
        push(('  [%s] %4d  %s'):format(
          (ctx.kind or '-'):sub(1, 7), ctx.module_count or 0, root_tail))
        local mods = ctx.modules or {}
        local recent = {}
        for _, md in ipairs(mods) do table.insert(recent, md) end
        -- ago_ms is vim.NIL (JSON null) on bone; `or` won't fall through a
        -- truthy userdata, so coerce non-numbers to a sort-last sentinel.
        local function ago_key(x) return type(x) == 'number' and x or 1e12 end
        table.sort(recent, function(a, b) return ago_key(a.ago_ms) < ago_key(b.ago_ms) end)
        local shown = math.min(#recent, 3)
        for i = 1, shown do
          local md = recent[i]
          local name = vim.fn.fnamemodify(md.id or '-', ':t'):sub(1, 30)
          local flag = md.stale and ' STALE' or ''
          push(('     . %-30s %s ago%s'):format(
            name, fmt_ago(md.ago_ms), flag))
        end
      end
    end
  end

  -- STATS: same fan-out — one block per client. Each rock-lsp's
  -- compile counters, RSS, etc. show separately.
  local stats_client_ids = {}
  for cid in pairs(s.stats or {}) do table.insert(stats_client_ids, cid) end
  table.sort(stats_client_ids)
  for _, cid in ipairs(stats_client_ids) do
    local st = s.stats[cid]
    if st then
      section((' STATS client=%d (rock/stats)'):format(cid))
      local keys = {}
      for k in pairs(st) do table.insert(keys, k) end
      table.sort(keys)
      for _, k in ipairs(keys) do
        local v = st[k]
        local rendered
        if type(v) == 'table' then
          local n, total = 0, 0
          for _, vv in pairs(v) do
            n = n + 1
            if type(vv) == 'number' then total = total + vv end
          end
          rendered = ('%d keys / %d total'):format(n, total)
        else
          rendered = tostring(v)
        end
        push(('  %-22s %s'):format(k, rendered))
      end
    end
  end

  if s.last_completion then
    local lc = s.last_completion
    section(' LAST COMPLETION  (<leader>rc for detail)')
    local trig = lc.trigger_char and ("'" .. lc.trigger_char .. "'") or (lc.trigger_kind or '-')
    local lat = lc.latency_ms and ('%.1fms'):format(lc.latency_ms) or '-'
    push(('  %s  items=%d%s  %s  trig=%s'):format(
      lc.wall, lc.item_count, lc.is_incomplete and '+' or '', lat, trig))
    if lc.items then
      for i, item in ipairs(lc.items) do
        if i > 4 then break end
        local label = type(item) == 'table' and (item.label or '?') or tostring(item)
        push(('    %d. %s'):format(i, label))
      end
    end
  end

  if #s.log > 0 then
    section((' LOG (%d, tail)'):format(#s.log))
    local n = math.min(#s.log, 10)
    for i = #s.log - n + 1, #s.log do
      local line = s.log[i]
      if #line > WIDTH - 4 then line = line:sub(1, WIDTH - 4) .. '...' end
      push('  ' .. line)
    end
  end

  local mode = FILTER_MODES[FILTER_IDX]
  local function passes(ev)
    if mode == 'no-progress' then
      if ev.method == '$/progress' then return false end
      if ev.method == 'window/workDoneProgress/create' then return false end
    elseif mode == 'requests-only' then
      if ev.dir ~= '->req' and ev.dir ~= '<-resp' and ev.dir ~= '<-req' then return false end
    elseif mode == 'notifications-only' then
      if ev.dir ~= '->notif' and ev.dir ~= '<-notif' then return false end
    end
    if FILTER_SUBSTR and FILTER_SUBSTR ~= '' and not ev.method:find(FILTER_SUBSTR, 1, true) then
      return false
    end
    return true
  end
  -- Walk events newest -> oldest. We still visit every entry to compute
  -- total_passing for the section header, so this is O(n) in the ring
  -- size, same as before. The win: we skip the `table.insert` into kept[]
  -- for all but the 40 most recent passing events, which removes the
  -- allocation/GC pressure that dominated on $/progress bursts.
  local DISPLAY = 40
  local kept = {}
  local total_passing = 0
  for i = #s.events, 1, -1 do
    local ev = s.events[i]
    if passes(ev) then
      total_passing = total_passing + 1
      if #kept < DISPLAY then kept[#kept + 1] = ev end
    end
  end
  local filter_desc = mode
  if FILTER_SUBSTR and FILTER_SUBSTR ~= '' then
    filter_desc = filter_desc .. (" /%s/"):format(FILTER_SUBSTR)
  end
  section((' EVENTS (%d of %d, %s)  [f] cycle  [/] substr'):format(
    total_passing, #s.events, filter_desc))
  for i = 1, #kept do
    local ev = kept[i]
    local dir_hl = 'Normal'
    if ev.dir == '->req' then dir_hl = 'DiagnosticInfo'
    elseif ev.dir == '<-resp' then dir_hl = 'DiagnosticHint'
    elseif ev.dir == '<-notif' then dir_hl = 'Comment'
    elseif ev.dir == '->notif' then dir_hl = 'Comment'
    elseif ev.dir == 'local' then dir_hl = 'DiagnosticWarn' end
    local line = ('  %s %s %s %s'):format(
      ev.wall, pad(ev.dir, 7), pad(ev.method, 22), format_payload_oneline(ev.payload)
    )
    if #line > WIDTH - 1 then line = line:sub(1, WIDTH - 1) end
    local li = push(line)
    LINE_TO_EVENT[li + 1] = ev.id
    table.insert(hl, { li, dir_hl, 11, 18 })
  end

  return lines, hl
end

local HL_NS = vim.api.nvim_create_namespace('rock.nvim.panel')

local function render()
  if not PANEL_BUF or not vim.api.nvim_buf_is_valid(PANEL_BUF) then return end
  local lines, hl = collect_lines()
  vim.api.nvim_set_option_value('modifiable', true, { buf = PANEL_BUF })
  vim.api.nvim_buf_set_lines(PANEL_BUF, 0, -1, false, lines)
  vim.api.nvim_buf_clear_namespace(PANEL_BUF, HL_NS, 0, -1)
  for _, h in ipairs(hl) do
    local line_idx, group, cs, ce = h[1], h[2], h[3], h[4]
    -- nvim_buf_set_extmark replaces the deprecated buf_add_highlight.
    -- ce == -1 means "to end of line"; extmark takes that as end_col = nil
    -- with end_line = line_idx + 1 start-of-line.
    local extmark_opts = { hl_group = group }
    if ce == -1 then
      extmark_opts.end_line = line_idx + 1
      extmark_opts.end_col = 0
    else
      extmark_opts.end_col = ce
    end
    pcall(vim.api.nvim_buf_set_extmark, PANEL_BUF, HL_NS, line_idx, cs, extmark_opts)
  end
  vim.api.nvim_set_option_value('modifiable', false, { buf = PANEL_BUF })
end

local function expand_event_under_cursor()
  if not PANEL_WIN or not vim.api.nvim_win_is_valid(PANEL_WIN) then return end
  local row = vim.api.nvim_win_get_cursor(PANEL_WIN)[1]
  local ev_id = LINE_TO_EVENT[row]
  if not ev_id then
    vim.notify('no event on this line', vim.log.levels.INFO)
    return
  end
  local ev
  for _, e in ipairs(state.get().events) do
    if e.id == ev_id then ev = e; break end
  end
  if not ev then return end
  local ok, json = pcall(vim.fn.json_encode, ev.payload)
  if not ok then json = vim.inspect(ev.payload) end
  -- Pretty-print by inserting newlines after commas (cheap).
  local pretty = vim.inspect(ev.payload)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.split(
    ('# %s %s %s\n\n%s'):format(ev.wall, ev.dir, ev.method, pretty), '\n'
  ))
  vim.api.nvim_set_option_value('filetype', 'lua', { buf = buf })
  vim.api.nvim_set_option_value('modifiable', false, { buf = buf })
  local w = math.floor(vim.o.columns * 0.7)
  local h = math.floor(vim.o.lines * 0.7)
  vim.api.nvim_open_win(buf, true, {
    relative = 'editor',
    width = w, height = h,
    row = math.floor((vim.o.lines - h) / 2),
    col = math.floor((vim.o.columns - w) / 2),
    style = 'minimal', border = 'rounded',
    title = ' event #' .. ev_id .. ' ', title_pos = 'center',
  })
  vim.keymap.set('n', 'q', '<cmd>close<cr>', { buffer = buf, silent = true })
end

local function setup_panel_keymaps(buf)
  local opts = { buffer = buf, silent = true, nowait = true }
  vim.keymap.set('n', 'q', M.close, opts)
  vim.keymap.set('n', 'r', function() client.restart() end, opts)
  vim.keymap.set('n', 'c', function() state.clear_events(); render() end, opts)
  vim.keymap.set('n', 'e', expand_event_under_cursor, opts)
  vim.keymap.set('n', '<cr>', expand_event_under_cursor, opts)
  vim.keymap.set('n', 's', M.swap_side, opts)
  vim.keymap.set('n', 'E', function()
    state.clear_errors()
    render()
  end, opts)
  vim.keymap.set('n', 'f', function()
    FILTER_IDX = (FILTER_IDX % #FILTER_MODES) + 1
    render()
  end, opts)
  vim.keymap.set('n', '/', function()
    vim.ui.input({ prompt = 'filter method (empty = clear): ', default = FILTER_SUBSTR or '' },
      function(input)
        FILTER_SUBSTR = (input and input ~= '') and input or nil
        render()
      end)
  end, opts)
  vim.keymap.set('n', '?', function()
    vim.notify(
      'rock.nvim panel\n  q  close\n  r  restart LSP\n  c  clear events\n  E  clear errors\n  e / <CR>  expand event\n  s  swap panel side\n  f  cycle event filter\n  /  filter events by method substring',
      vim.log.levels.INFO, { title = 'rock.nvim' }
    )
  end, opts)
end

function M.is_open()
  return PANEL_WIN and vim.api.nvim_win_is_valid(PANEL_WIN)
end

local function float_config()
  local h = math.max(10, vim.o.lines - 4)
  local col = (SIDE == 'left') and 1 or (vim.o.columns - WIDTH - 2)
  return { relative = 'editor', width = WIDTH, height = h, row = 1, col = col,
           style = 'minimal', border = 'rounded',
           title = ' rock.nvim ', title_pos = 'center', noautocmd = true }
end

function M.open()
  if M.is_open() then return end
  -- Place the panel away from the current window.
  local cur_col = vim.api.nvim_win_get_position(vim.api.nvim_get_current_win())[2]
  SIDE = cur_col < math.floor(vim.o.columns / 2) and 'right' or 'left'
  if not PANEL_BUF or not vim.api.nvim_buf_is_valid(PANEL_BUF) then
    PANEL_BUF = require('rock.ui').ensure_named_buffer('rock://panel')
    vim.api.nvim_set_option_value('filetype', 'rock_panel', { buf = PANEL_BUF })
    setup_panel_keymaps(PANEL_BUF)
  end
  PANEL_WIN = vim.api.nvim_open_win(PANEL_BUF, false, float_config())
  vim.api.nvim_set_option_value('wrap', false, { win = PANEL_WIN })
  vim.api.nvim_set_option_value('number', false, { win = PANEL_WIN })
  vim.api.nvim_set_option_value('relativenumber', false, { win = PANEL_WIN })
  vim.api.nvim_set_option_value('signcolumn', 'no', { win = PANEL_WIN })

  if not UNSUB then
    -- Hot LSP streams ($/progress, incremental completion) can fire notify()
    -- hundreds of times per tick. Coalesce into one render per vim tick via
    -- a dirty flag instead of queueing a scheduled render per event.
    UNSUB = state.subscribe(function()
      if RENDER_PENDING then return end
      RENDER_PENDING = true
      vim.schedule(function()
        RENDER_PENDING = false
        if M.is_open() then render() end
      end)
    end)
  end
  if not TIMER then
    TIMER = (vim.uv or vim.loop).new_timer()
    TIMER:start(1000, 1000, vim.schedule_wrap(function()
      if M.is_open() then
        -- If a subscriber already queued a render for this tick, let it
        -- run instead of rendering twice.
        if RENDER_PENDING then return end
        render()
      end
    end))
  end
  render()
end

function M.close()
  if M.is_open() then
    pcall(vim.api.nvim_win_close, PANEL_WIN, true)
  end
  PANEL_WIN = nil
  if TIMER then pcall(TIMER.stop, TIMER); pcall(TIMER.close, TIMER); TIMER = nil end
  if UNSUB then UNSUB(); UNSUB = nil end
end

function M.toggle()
  if M.is_open() then M.close() else M.open() end
end

function M.set_side(side)
  if side ~= 'left' and side ~= 'right' then return end
  SIDE = side
  if M.is_open() then
    M.close()
    M.open()
  end
end

function M.set_width(w)
  if type(w) ~= 'number' or w < 30 then return end
  WIDTH = w
  if M.is_open() then
    M.close()
    M.open()
  end
end

function M.swap_side()
  M.set_side(SIDE == 'right' and 'left' or 'right')
end

function M.setup_autoopen()
  local g = vim.api.nvim_create_augroup('rock.nvim.panel', { clear = true })
  -- Fire once: auto-open on the first .rock buffer, then stop watching.
  -- Without `once`, every FileType event (including ones triggered by :vs
  -- creating a new window over a .rock buffer) can reopen the panel after
  -- PANEL_WIN was externally invalidated.
  vim.api.nvim_create_autocmd('FileType', {
    group = g,
    pattern = 'rock',
    once = true,
    callback = function()
      if not M.is_open() then vim.schedule(M.open) end
    end,
  })
  -- Reposition the float when the cursor moves to a different window so the
  -- panel always opens away from the active editing area.
  vim.api.nvim_create_autocmd('WinEnter', {
    group = g,
    callback = function()
      if not M.is_open() then return end
      local cur = vim.api.nvim_get_current_win()
      if cur == PANEL_WIN then return end
      local cur_col = vim.api.nvim_win_get_position(cur)[2]
      local desired = cur_col < math.floor(vim.o.columns / 2) and 'right' or 'left'
      if desired ~= SIDE then
        SIDE = desired
        pcall(vim.api.nvim_win_set_config, PANEL_WIN, float_config())
      end
    end,
  })
  -- Refit the float when the terminal is resized.
  vim.api.nvim_create_autocmd('VimResized', {
    group = g,
    callback = function()
      if M.is_open() then
        pcall(vim.api.nvim_win_set_config, PANEL_WIN, float_config())
      end
    end,
  })
  -- Track external closure so M.is_open() stays accurate.
  vim.api.nvim_create_autocmd('WinClosed', {
    group = g,
    callback = function(args)
      if PANEL_WIN and tonumber(args.match) == PANEL_WIN then
        PANEL_WIN = nil
        if TIMER then pcall(TIMER.stop, TIMER); pcall(TIMER.close, TIMER); TIMER = nil end
        if UNSUB then UNSUB(); UNSUB = nil end
      end
    end,
  })
end

return M
