local state = require('rock.state')
local uv = vim.uv or vim.loop
local M = {}
local timers = {}
local pending = { ['rock/stats'] = {}, ['rock/modules'] = {} }
local store = { ['rock/stats'] = state.set_stats, ['rock/modules'] = state.set_modules }

function M.request(client, method, reply)
  local id = client.id
  local requests = pending[method]
  if requests[id] then
    if reply then table.insert(requests[id], reply) end
    return
  end

  local waiters = reply and { reply } or {}
  requests[id] = waiters
  local started = uv.hrtime()
  local function finish(err, result, failure)
    if requests[id] ~= waiters then return end
    requests[id] = nil
    if vim.lsp.get_client_by_id(id) ~= client
        or (client.attached_buffers and not next(client.attached_buffers)) then
      failure, result = 'client detached', nil
    end
    if not err and not failure and type(result) == 'table' then store[method](id, result) end
    local elapsed = (uv.hrtime() - started) / 1e6
    for _, cb in ipairs(waiters) do cb(err, result, elapsed, failure) end
  end

  if client:request(method, vim.empty_dict(), function(err, result) finish(err, result) end) == false then
    finish(nil, nil, 'request refused')
  elseif requests[id] == waiters then
    vim.defer_fn(function() finish(nil, nil, 'timed out (>2s)') end, 2000)
  end
end

local function poll(method)
  for _, client in ipairs(vim.lsp.get_clients({ name = 'rock' })) do M.request(client, method) end
end

function M.start()
  if #timers > 0 then return end
  for _, job in ipairs({
    { 2000, 5000, function() poll('rock/stats') end },
    { 3000, 10000, function() poll('rock/modules') end },
    { 1500, 30000, function()
      if #vim.lsp.get_clients({ name = 'rock' }) > 0 then require('rock.doctor').run(nil) end
    end },
  }) do
    local timer = uv.new_timer()
    timers[#timers + 1] = timer
    timer:start(job[1], job[2], vim.schedule_wrap(job[3]))
  end
end

function M.stop()
  for _, timer in ipairs(timers) do
    pcall(timer.stop, timer)
    pcall(timer.close, timer)
  end
  timers = {}
end

return M
