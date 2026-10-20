-- :RockDump -- write a JSON snapshot of current state to /tmp.
--
-- Useful for bug reports and session diffs. Includes:
--   - binary info (path/id/mtime/mismatch)
--   - client info (state/uptime/restarts/capabilities)
--   - buffers map
--   - stats and modules (last polled values)
--   - per-method latency samples (raw + summary)
--   - full event ring (summarized payloads)
--   - log tail
--
-- Written to /tmp/rock-nvim-<unix-ts>.json; path + size toasted on success.

local state = require('rock.state')

local M = {}

local function uptime_s(t)
  return t and (os.time() - t) or nil
end

local function summarize_timing(samples)
  local perf = require('rock.perf')
  return perf.summarize(samples)
end

-- buffers/stats/modules are keyed by bufnr / client_id. After a long
-- session those integers get large while only one or two entries
-- exist, so vim.json.encode sees an "excessively sparse array" and
-- the whole snapshot fails to encode (field report: ":RockDump …
-- Cannot serialise table: excessively sparse array"). Stringify the
-- keys so the encoder emits a JSON object instead of attempting an
-- array. A map keyed by handle is semantically an object anyway.
local function stringify_keys(t)
  if type(t) ~= 'table' then return t end
  local out = {}
  for k, v in pairs(t) do
    out[tostring(k)] = v
  end
  return out
end

local function build_snapshot()
  local s = state.get()
  local timings = {}
  for method, t in pairs(s.request_timings or {}) do
    timings[method] = {
      samples = t.samples,
      summary = summarize_timing(t.samples),
    }
  end
  -- Strip the sorted copy from each summary; JSON doesn't need it and
  -- it's just a duplicate of samples.
  for _, v in pairs(timings) do
    if v.summary then v.summary.sorted = nil end
  end

  return {
    generated_at = os.date('%Y-%m-%d %H:%M:%S'),
    generated_at_unix = os.time(),
    nvim = {
      version = tostring(vim.version()),
      cwd = vim.fn.getcwd(),
    },
    binary = s.binary,
    client = {
      id = s.client.id,
      state = s.client.state,
      uptime_s = uptime_s(s.client.started_at),
      restart_count = s.client.restart_count,
      last_exit = s.client.last_exit,
      capabilities = s.client.capabilities,
    },
    buffers = stringify_keys(s.buffers),
    stats = stringify_keys(s.stats),
    modules = stringify_keys(s.modules),
    last_completion = s.last_completion,
    request_timings = timings,
    events = s.events,
    log_tail = s.log,
  }
end

function M.write(path)
  path = path or ('/tmp/rock-nvim-' .. os.time() .. '.json')
  local snap = build_snapshot()
  local ok, encoded = pcall(vim.json.encode, snap)
  if not ok then
    vim.notify('rock.nvim: failed to encode snapshot: ' .. tostring(encoded),
      vim.log.levels.ERROR, { title = 'rock.nvim' })
    return nil
  end
  local fd, err = (vim.uv or vim.loop).fs_open(path, 'w', 420)  -- 0644
  if not fd then
    vim.notify('rock.nvim: open failed: ' .. tostring(err),
      vim.log.levels.ERROR, { title = 'rock.nvim' })
    return nil
  end
  (vim.uv or vim.loop).fs_write(fd, encoded, 0)
  ;(vim.uv or vim.loop).fs_close(fd)
  local st = (vim.uv or vim.loop).fs_stat(path)
  vim.notify(('rock.nvim: wrote %s  (%.1f KB)'):format(path, (st and st.size or 0) / 1024),
    vim.log.levels.INFO, { title = 'rock.nvim' })
  return path
end

return M
