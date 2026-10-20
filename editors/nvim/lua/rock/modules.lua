-- Floating window showing the server's loaded modules (rock/modules).
--
-- Fetches fresh on open rather than polling -- module set changes
-- infrequently and a per-module compile-time snapshot is cheap but not
-- free for the server to build on large workspaces.

local state = require('rock.state')

local M = {}

local FLOAT_BUF = nil
local FLOAT_WIN = nil
local SERVER_NAME = 'rock'

-- Fan out across every attached rock client and collect responses
-- into { [client_id] = result }. on_done(map, err) fires once after
-- the last response. Same shape as state.modules.
local function fetch(on_done)
  local clients = vim.lsp.get_clients({ name = SERVER_NAME })
  if not clients or #clients == 0 then
    on_done(nil, 'no rock LSP client attached')
    return
  end
  local pending = #clients
  local results = {}
  local first_err = nil
  for _, c in ipairs(clients) do
    local cid = c.id
    c:request('rock/modules', vim.empty_dict(), function(err, result)
      if err then
        first_err = first_err or tostring(err)
      else
        results[cid] = result
        state.set_modules(cid, result)
      end
      pending = pending - 1
      if pending == 0 then on_done(results, first_err) end
    end)
  end
end

local function short(s, n) return #s > n and (s:sub(1, n - 1) .. '...') or s end

local function fmt_us(us)
  -- JSON null -> vim.NIL (truthy userdata); bone always sends this field null,
  -- so guard on the type rather than falsiness.
  if type(us) ~= 'number' or us == 0 then return '-' end
  if us < 1000 then return ('%d us'):format(us) end
  if us < 1000000 then return ('%.1f ms'):format(us / 1000) end
  return ('%.2f s'):format(us / 1000000)
end

local function fmt_ago(ms)
  if type(ms) ~= 'number' or ms == 0 then return '-' end
  if ms < 1000 then return ('%dms ago'):format(ms) end
  if ms < 60000 then return ('%.1fs ago'):format(ms / 1000) end
  return ('%dm ago'):format(math.floor(ms / 60000))
end

local function build_lines(data, err)
  local lines = {}
  local function push(s) table.insert(lines, s) end
  if err then
    push(' error fetching rock/modules:')
    push('   ' .. err)
    if not data then return lines end
    push('')
  end
  if not data or next(data) == nil then
    push(' (no data yet)')
    return lines
  end

  -- data is { [client_id] = response }. Render each client as its
  -- own block so multi-client setups don't conflate workspaces.
  local cids = {}
  for cid in pairs(data) do table.insert(cids, cid) end
  table.sort(cids)
  for _, cid in ipairs(cids) do
    local d = data[cid]
    push((' CLIENT %d  total=%d across %d contexts'):format(
      cid, d.total_modules or 0, #(d.contexts or {})))
    push('')
    for _, ctx in ipairs(d.contexts or {}) do
      push((' [%s]  %s   (%d modules)'):format(
        ctx.kind or '-', short(ctx.root or '-', 80), ctx.module_count or 0))
      push(' ' .. string.rep('-', 110))
      push((' %-60s %-10s %-12s %s'):format('module', 'compile', 'age', 'flags'))
      local mods = ctx.modules or {}
      if #mods == 0 then push(' (none)') end
      for _, m in ipairs(mods) do
        local flags = {}
        if m.stale then table.insert(flags, 'STALE') end
        if m.is_stdlib then table.insert(flags, 'stdlib') end
        if m.is_builtin then table.insert(flags, 'builtin') end
        push((' %-60s %-10s %-12s %s'):format(
          short(m.id or '-', 60),
          fmt_us(m.last_compile_us),
          fmt_ago(m.ago_ms),
          table.concat(flags, ' ')))
      end
      push('')
    end
  end
  push(' keys:  r  refetch   q/<Esc>  close')
  return lines
end

function M.is_open()
  return FLOAT_WIN and vim.api.nvim_win_is_valid(FLOAT_WIN)
end

function M.close()
  if M.is_open() then pcall(vim.api.nvim_win_close, FLOAT_WIN, true) end
  FLOAT_WIN = nil
end

local function set_lines(data, err)
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then return end
  local lines = build_lines(data, err)
  vim.api.nvim_set_option_value('modifiable', true, { buf = FLOAT_BUF })
  vim.api.nvim_buf_set_lines(FLOAT_BUF, 0, -1, false, lines)
  vim.api.nvim_set_option_value('modifiable', false, { buf = FLOAT_BUF })
end

local function do_refetch()
  set_lines(nil, 'fetching...')
  fetch(function(result, err) vim.schedule(function() set_lines(result, err) end) end)
end

function M.open()
  if M.is_open() then M.close(); return end
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then
    FLOAT_BUF = require('rock.ui').ensure_named_buffer('rock://modules')
    vim.keymap.set('n', 'q', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', '<esc>', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', 'r', do_refetch, { buffer = FLOAT_BUF, silent = true })
  end

  set_lines(state.get().modules, nil)

  local w = math.min(130, math.floor(vim.o.columns * 0.95))
  local h = math.floor(vim.o.lines * 0.85)
  FLOAT_WIN = vim.api.nvim_open_win(FLOAT_BUF, true, {
    relative = 'editor', width = w, height = h,
    row = math.floor((vim.o.lines - h) / 2),
    col = math.floor((vim.o.columns - w) / 2),
    style = 'minimal', border = 'rounded',
    title = ' rock.nvim: loaded modules ', title_pos = 'center',
  })
  vim.api.nvim_set_option_value('wrap', false, { win = FLOAT_WIN })

  do_refetch()
end

function M.toggle()
  if M.is_open() then M.close() else M.open() end
end

return M
