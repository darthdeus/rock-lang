-- :RockLspDoctor -- one-shot health check.
--
-- Reads what state.lua already knows (so most checks are free) plus
-- fires a couple of lightweight probes (rock/stats, rock/modules).
-- Renders a floating window with PASS/WARN/FAIL plus diagnosis per row.

local state = require('rock.state')
local ui = require('rock.ui')
local uv = vim.uv or vim.loop

local M = {}

local FLOAT_BUF = nil
local FLOAT_WIN = nil
local SERVER_NAME = 'rock'

-- Returns { status = 'PASS'|'WARN'|'FAIL', msg, detail }
local function check_binary()
  local b = state.get().binary
  if not b.path then return { status = 'FAIL', msg = 'no binary resolved' } end
  if not b.exists then
    return { status = 'FAIL', msg = 'binary missing',
      detail = b.path .. ' does not exist; run cargo build --bin rock' }
  end
  -- fs_access returns true on x permission for the current user.
  local ok = uv.fs_access and uv.fs_access(b.path, 'X')
  if ok == false then
    return { status = 'WARN', msg = 'binary present but not executable by current user',
      detail = b.path }
  end
  return { status = 'PASS', msg = ('binary ok  id=%s  %.1fMB'):format(
    b.sha8 or '-', (b.size or 0) / 1024 / 1024) }
end

local function check_worktree()
  local b = state.get().binary
  if b.mismatch then
    return { status = 'WARN', msg = 'worktree mismatch',
      detail = ('cwd under %s but binary built from %s'):format(
        tostring(b.worktree_expected), tostring(b.worktree_actual)) }
  end
  return { status = 'PASS', msg = 'worktree matches binary origin' }
end

local function check_client()
  local s = state.get().client
  if s.state == 'attached' then
    return { status = 'PASS', msg = ('client attached  id=%s  restarts=%d'):format(
      tostring(s.id or '-'), s.restart_count) }
  end
  if s.state == 'crashed' then
    local ex = s.last_exit or {}
    return { status = 'FAIL', msg = 'client crashed',
      detail = ('exit=%s sig=%s'):format(tostring(ex.code), tostring(ex.signal)) }
  end
  return { status = 'WARN', msg = ('client state: %s'):format(s.state or '-') }
end

local function check_restarts()
  local n = state.get().client.restart_count
  if n > 5 then
    return { status = 'WARN', msg = ('%d restarts this session'):format(n),
      detail = 'frequent restarts may indicate a server-side bug' }
  end
  return { status = 'PASS', msg = ('%d restarts this session'):format(n) }
end

local function check_capabilities()
  local caps = state.get().client.capabilities or {}
  local want = {
    'completionProvider', 'hoverProvider', 'semanticTokensProvider',
    'inlayHintProvider', 'definitionProvider', 'referencesProvider',
    'renameProvider', 'signatureHelpProvider', 'codeActionProvider',
  }
  local missing = {}
  for _, k in ipairs(want) do
    if not caps[k] then table.insert(missing, k) end
  end
  if #missing == 0 then
    return { status = 'PASS', msg = 'all expected capabilities advertised' }
  end
  return { status = 'WARN', msg = ('missing %d capabilit%s'):format(
    #missing, #missing == 1 and 'y' or 'ies'),
    detail = table.concat(missing, ', ') }
end

local function check_log()
  local path = vim.lsp.get_log_path()
  if not path or path == '' then
    return { status = 'WARN', msg = 'LSP log path not set' }
  end
  local st = uv.fs_stat(path)
  if not st then
    return { status = 'WARN', msg = 'log file not yet created',
      detail = path }
  end
  return { status = 'PASS', msg = ('log ok  %s  %.1fKB'):format(
    path, st.size / 1024) }
end

-- Async probes: fire rock/stats + rock/modules and report on callback.
local function probe_async(method, on_result)
  local cs = vim.lsp.get_clients({ name = SERVER_NAME })
  if not cs or #cs == 0 then
    on_result({ status = 'FAIL', msg = 'no ' .. SERVER_NAME .. ' client to probe' })
    return
  end
  local start = uv.hrtime()
  local responded = false
  cs[1]:request(method, vim.empty_dict(), function(err, result)
    responded = true
    local elapsed_ms = (uv.hrtime() - start) / 1e6
    if err then
      on_result({ status = 'FAIL', msg = method .. ' error',
        detail = tostring(err) })
      return
    end
    if type(result) ~= 'table' then
      on_result({ status = 'WARN', msg = method .. ' returned non-table',
        detail = tostring(result) })
      return
    end
    on_result({ status = 'PASS', msg = ('%s ok  %.1fms'):format(method, elapsed_ms) })
  end)
  vim.defer_fn(function()
    if not responded then
      on_result({ status = 'FAIL', msg = method .. ' timed out (>2s)' })
    end
  end, 2000)
end

local function status_glyph(s)
  if s == 'PASS' then return '[PASS]' end
  if s == 'WARN' then return '[WARN]' end
  if s == 'FAIL' then return '[FAIL]' end
  return '[----]'
end

local function render_lines(results)
  local lines = { ' rock.nvim doctor' }
  local function push(s) table.insert(lines, s) end
  push('')
  local counts = { PASS = 0, WARN = 0, FAIL = 0 }
  for _, r in ipairs(results) do
    counts[r.status] = (counts[r.status] or 0) + 1
  end
  push((' summary:  %d pass   %d warn   %d fail'):format(
    counts.PASS, counts.WARN, counts.FAIL))
  push('')
  for _, r in ipairs(results) do
    push((' %s  %s'):format(status_glyph(r.status), r.msg))
    if r.detail then push(('          %s'):format(r.detail)) end
  end
  push('')
  push(' q/<Esc>  close   r  rerun')
  return lines
end

function M.run(on_done)
  local results = {}
  -- Sync checks first.
  table.insert(results, check_binary())
  table.insert(results, check_worktree())
  table.insert(results, check_client())
  table.insert(results, check_restarts())
  table.insert(results, check_capabilities())
  table.insert(results, check_log())
  -- Placeholders for async probes; fill in when they return.
  local stats_idx = #results + 1
  table.insert(results, { status = '----', msg = 'rock/stats ...' })
  local modules_idx = #results + 1
  table.insert(results, { status = '----', msg = 'rock/modules ...' })

  local function emit()
    -- Always update state so the panel section stays current, even if
    -- the caller passed no on_done (e.g. periodic background refresh).
    state.set_doctor(results)
    if on_done then on_done(results) end
  end

  emit()  -- render placeholders immediately

  probe_async('rock/stats', function(r)
    results[stats_idx] = r
    vim.schedule(emit)
  end)
  probe_async('rock/modules', function(r)
    results[modules_idx] = r
    vim.schedule(emit)
  end)
end

function M.is_open()
  return FLOAT_WIN and vim.api.nvim_win_is_valid(FLOAT_WIN)
end

function M.close()
  if M.is_open() then pcall(vim.api.nvim_win_close, FLOAT_WIN, true) end
  FLOAT_WIN = nil
end

local function set_lines(results)
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then return end
  vim.api.nvim_set_option_value('modifiable', true, { buf = FLOAT_BUF })
  vim.api.nvim_buf_set_lines(FLOAT_BUF, 0, -1, false, render_lines(results))
  vim.api.nvim_set_option_value('modifiable', false, { buf = FLOAT_BUF })
end

function M.open()
  if M.is_open() then M.close(); return end
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then
    FLOAT_BUF = ui.ensure_named_buffer('rock://doctor')
    vim.keymap.set('n', 'q', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', '<esc>', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', 'r', function() M.run(set_lines) end,
      { buffer = FLOAT_BUF, silent = true })
  end

  local w = math.min(100, math.floor(vim.o.columns * 0.9))
  local h = math.min(24, math.floor(vim.o.lines * 0.7))
  FLOAT_WIN = vim.api.nvim_open_win(FLOAT_BUF, true, {
    relative = 'editor', width = w, height = h,
    row = math.floor((vim.o.lines - h) / 2),
    col = math.floor((vim.o.columns - w) / 2),
    style = 'minimal', border = 'rounded',
    title = ' rock.nvim: doctor ', title_pos = 'center',
  })

  M.run(set_lines)
end

function M.toggle()
  if M.is_open() then M.close() else M.open() end
end

return M
