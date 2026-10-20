-- Central observable store for rock.nvim.
-- Holds binary info, client info, per-buffer state, and ring buffers for
-- events and log lines. Subscribers (the panel, statusline) register
-- callbacks via M.subscribe() and receive M.get() snapshots on change.

local M = {}

local EVENT_RING_CAP = 1000
local LOG_RING_CAP = 500
local TIMING_SAMPLES_PER_METHOD = 500
local ERRORS_RING_CAP = 50

-- Bounded arrays use slack-compaction: we let the array overshoot its cap
-- by `SLACK` entries, then drop the oldest SLACK in one shot. This is
-- amortized O(1) per insert instead of the O(n) cost of `table.remove(t, 1)`
-- on every overflow. `$/progress` bursts used to thrash this path.
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

-- Newest-first variant: we prepend, then trim from the tail when oversized.
-- Trim from the tail is always O(k) in the number trimmed, regardless of
-- array size, so no slack trick is needed — but `table.insert(t, 1, v)` is
-- itself O(n). We accept that for small caps (captures=10, errors=50).
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
  -- rock/stats and rock/modules are polled per-client and keyed by
  -- client_id. A single-client session has one entry; multi-client
  -- sessions (e.g. two packages each rooted at their own stick.toml)
  -- previously hid one client's data because only clients[1] was
  -- polled — see /tmp/rock-nvim-1778674707.json for the dump that
  -- surfaced this. Consumers (panel, dump, modules floater) iterate
  -- the map.
  stats = {},               -- { [client_id] = rock/stats response }
  modules = {},             -- { [client_id] = rock/modules response }
  -- Client-observed per-method request latencies. Keyed by method ->
  -- { samples = { ms1, ms2, ... } }.
  request_timings = {},
  -- Insert-mode captures (bug-diagnosis snapshots). Bounded ring.
  captures = {},
  -- Silent server errors: window/logMessage / window/showMessage with
  -- Error or Warning severity. Prominent in panel so they never pass
  -- unnoticed. Bounded ring; 'E' in panel clears.
  errors = {},
  -- Last rock.nvim doctor result list (see rock.doctor). nil if not run yet.
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

-- Drop stats/modules for a client that's no longer attached. Called
-- on LspDetach so the dump doesn't accumulate dead-client entries
-- across long sessions.
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

-- Fire subscribers without structural changes; use after mutating an
-- existing capture entry in place.
function M.add_capture_tick()
  notify()
end

function M.set_doctor(results)
  state.doctor = results
  state.doctor_at = os.time()
  notify()
end

-- Record a server-reported error/warning. Severity follows the LSP
-- MessageType enum: 1=Error, 2=Warning, 3=Info, 4=Log. We only record
-- 1 and 2 here; 3 and 4 stay in the general event ring.
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
