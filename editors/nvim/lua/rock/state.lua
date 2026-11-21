
local M = {}

local EVENT_RING_CAP = 1000
local LOG_RING_CAP = 500
local TIMING_SAMPLES_PER_METHOD = 500
local ERRORS_RING_CAP = 50

-- Drop old entries in batches to amortize the cost of shifting the array.
local RING_SLACK = 256

local function push_bounded(arr, v, cap)
  arr[#arr + 1] = v
  if #arr > cap + RING_SLACK then
    local drop = #arr - cap
    local n = #arr
    for i = 1, n - drop do arr[i] = arr[i + drop] end
    for i = n - drop + 1, n do arr[i] = nil end
  end
end

-- Tail trimming is proportional to the number removed; these caps are small.
local function prepend_bounded(arr, v, cap)
  table.insert(arr, 1, v)
  while #arr > cap do arr[#arr] = nil end
end

local state = {
  binary = {
    path = nil,             -- absolute, realpath-resolved
    configured_path = nil,  -- what was asked for (may be a script)
    mtime = nil,
    size = nil,
    sha8 = nil,
    worktree_expected = nil, -- repo dir inferred from cwd
    worktree_actual = nil,   -- repo dir inferred from binary path
    mismatch = false,
    exists = false,
  },
  client = {
    id = nil,
    pid = nil,
    started_at = nil,
    state = 'detached',     -- detached | starting | attached | crashed
    restart_count = 0,
    last_exit = nil,        -- { code, signal, at }
    capabilities = {},
  },
  buffers = {},             -- [bufnr] = { attached, stale_tokens, name }
  events = {},              -- ring, newest last
  events_idx = 0,           -- monotonic counter for stable IDs
  log = {},                 -- ring of log lines
  last_completion = nil,    -- { at, pos, trigger, items, latency_ms }
  -- Per-client entries keep multiple workspaces visible.
  stats = {},               -- { [client_id] = rock/stats response }
  modules = {},             -- { [client_id] = rock/modules response }
  request_timings = {},
  captures = {},
  errors = {},
  doctor = nil,
  doctor_at = nil,
}

local CAPTURES_CAP = 10

local subscribers = {}

local function notify()
  for _, cb in ipairs(subscribers) do
    pcall(cb, state)
  end
end

function M.subscribe(cb)
  table.insert(subscribers, cb)
  return function()
    for i, v in ipairs(subscribers) do
      if v == cb then table.remove(subscribers, i); return end
    end
  end
end

function M.get() return state end

function M.set_binary(info)
  state.binary = vim.tbl_extend('force', state.binary, info)
  notify()
end

function M.set_client(info)
  state.client = vim.tbl_extend('force', state.client, info)
  notify()
end

function M.inc_restart()
  state.client.restart_count = state.client.restart_count + 1
  notify()
end

function M.set_buffer(bufnr, info)
  state.buffers[bufnr] = vim.tbl_extend('force', state.buffers[bufnr] or {}, info)
  notify()
end

function M.remove_buffer(bufnr)
  state.buffers[bufnr] = nil
  notify()
end

function M.record_event(direction, method, payload)
  state.events_idx = state.events_idx + 1
  local ev = {
    id = state.events_idx,
    at = vim.uv.hrtime() / 1e6, -- ms since some epoch
    wall = os.date('%H:%M:%S'),
    dir = direction,
    method = method,
    payload = payload,
  }
  push_bounded(state.events, ev, EVENT_RING_CAP)
  notify()
end

function M.clear_events()
  state.events = {}
  notify()
end

function M.record_log(line)
  push_bounded(state.log, line, LOG_RING_CAP)
  notify()
end

function M.set_completion(info)
  state.last_completion = info
  notify()
end

function M.set_stats(client_id, s)
  state.stats[client_id] = s
  notify()
end

function M.set_modules(client_id, m)
  state.modules[client_id] = m
  notify()
end

-- Remove detached clients so snapshots do not accumulate stale entries.
function M.forget_client(client_id)
  state.stats[client_id] = nil
  state.modules[client_id] = nil
  notify()
end

function M.record_request_timing(method, ms)
  local t = state.request_timings[method]
  if not t then
    t = { samples = {} }
    state.request_timings[method] = t
  end
  push_bounded(t.samples, ms, TIMING_SAMPLES_PER_METHOD)
  -- Deliberately skip notify(): per-event re-renders would thrash the
  -- panel on hot request streams. The perf floater reads on demand.
end

function M.clear_request_timings()
  state.request_timings = {}
  notify()
end

function M.add_capture(cap)
  prepend_bounded(state.captures, cap, CAPTURES_CAP)
  notify()
end

function M.set_doctor(results)
  state.doctor = results
  state.doctor_at = os.time()
  notify()
end

-- LSP MessageType 1 and 2 are errors and warnings.
function M.record_error(severity, method, message)
  prepend_bounded(state.errors, {
    at = os.time(),
    wall = os.date('%H:%M:%S'),
    severity = severity,
    method = method,
    message = message,
  }, ERRORS_RING_CAP)
  notify()
end

function M.clear_errors()
  state.errors = {}
  notify()
end

return M
