-- Floating window showing the server's loaded modules (rock/modules).
--
-- Fetches fresh on open rather than polling -- module set changes
-- infrequently and a per-module compile-time snapshot is cheap but not
-- free for the server to build on large workspaces.

local state = require('rock.state')

local M = {}

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
      push((' %s   (%d modules)'):format(short(ctx.root or '-', 80), ctx.module_count or 0))
      push(' ' .. string.rep('-', 76))
      push((' %-60s %s'):format('module', 'age'))
      local mods = ctx.modules or {}
      if #mods == 0 then push(' (none)') end
      for _, m in ipairs(mods) do
        push((' %-60s %s'):format(short(m.id or '-', 60), fmt_ago(m.ago_ms)))
      end
      push('')
    end
  end
  push(' keys:  r  refetch   q/<Esc>  close')
  return lines
end

local function refetch(show)
  show(build_lines(nil, 'fetching...'))
  fetch(function(result, err)
    vim.schedule(function() show(build_lines(result, err)) end)
  end)
end

return require('rock.ui').float(M, {
  name = 'modules',
  title = 'loaded modules',
  nowrap = true,
  lines = function() return build_lines(state.get().modules, nil) end,
  size = function()
    return math.min(130, math.floor(vim.o.columns * 0.95)),
      math.floor(vim.o.lines * 0.85)
  end,
  refresh = refetch,
  on_open = refetch,
})
