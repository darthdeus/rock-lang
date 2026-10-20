-- Poll every attached rock-lsp client for `rock/stats` and
-- `rock/modules`, keyed by client_id. The panel and dump iterate
-- the resulting per-client maps.
--
-- Why poll every client, not just clients[1]: nvim sometimes spawns
-- one rock-lsp per workspace_folder (e.g. when two packages are
-- both attached). The previous "clients[1] only" wiring made every
-- multi-client session into a half-blind dump where one workspace's
-- stats overwrote the other's depending on attach order. Surfaced
-- by a dump where client.id=2 tracked one package but the stats
-- reported the other's 44-source workspace. Detecting the
-- multi-context publish-authority bug class requires seeing every
-- client's workspace at once.

local state = require('rock.state')

local M = {}

local TIMER = nil
local MODULES_TIMER = nil
local DOCTOR_TIMER = nil
local STATS_INTERVAL_MS = 5000
local MODULES_INTERVAL_MS = 10000
local DOCTOR_INTERVAL_MS = 30000
local SERVER_NAME = 'rock'

-- Fan out a custom request across every attached rock client. The
-- callback receives (client_id, result) so state can key on it.
local function request_all_clients(method, cb)
  local clients = vim.lsp.get_clients({ name = SERVER_NAME })
  if not clients or #clients == 0 then return end
  for _, c in ipairs(clients) do
    local client_id = c.id
    c:request(method, vim.empty_dict(), function(err, result)
      if err then return end     -- Older binary without the handler: stay quiet.
      if type(result) == 'table' then cb(client_id, result) end
    end)
  end
end

function M.start()
  if not TIMER then
    TIMER = (vim.uv or vim.loop).new_timer()
    TIMER:start(2000, STATS_INTERVAL_MS, vim.schedule_wrap(function()
      request_all_clients('rock/stats', state.set_stats)
    end))
  end
  if not MODULES_TIMER then
    MODULES_TIMER = (vim.uv or vim.loop).new_timer()
    MODULES_TIMER:start(3000, MODULES_INTERVAL_MS, vim.schedule_wrap(function()
      request_all_clients('rock/modules', state.set_modules)
    end))
  end
  if not DOCTOR_TIMER then
    DOCTOR_TIMER = (vim.uv or vim.loop).new_timer()
    -- First run soon after startup so the panel shows doctor rows on
    -- open; then every 30s. doctor.run() writes into state itself.
    DOCTOR_TIMER:start(1500, DOCTOR_INTERVAL_MS, vim.schedule_wrap(function()
      local cs = vim.lsp.get_clients({ name = SERVER_NAME })
      if not cs or #cs == 0 then return end
      require('rock.doctor').run(nil)
    end))
  end
end

function M.stop()
  if TIMER then pcall(TIMER.stop, TIMER); pcall(TIMER.close, TIMER); TIMER = nil end
  if MODULES_TIMER then pcall(MODULES_TIMER.stop, MODULES_TIMER)
    pcall(MODULES_TIMER.close, MODULES_TIMER); MODULES_TIMER = nil end
  if DOCTOR_TIMER then pcall(DOCTOR_TIMER.stop, DOCTOR_TIMER)
    pcall(DOCTOR_TIMER.close, DOCTOR_TIMER); DOCTOR_TIMER = nil end
end

return M
