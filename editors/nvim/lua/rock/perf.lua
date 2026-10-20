-- Per-LSP-method latency stats + ASCII histograms.
--
-- Data source: client-side samples in state.request_timings[method].
-- These measure nvim's view of request latency (request sent via RPC ->
-- response received), which is what matters for "does the LSP feel fast
-- in practice". For server-internal compile time, see the STATS panel
-- section (rock/stats).

local state = require('rock.state')

local M = {}

local FLOAT_BUF = nil
local FLOAT_WIN = nil

-- Sample stats. Returns nil for empty input.
function M.summarize(samples)
  local n = #samples
  if n == 0 then return nil end
  local sorted = vim.list_slice(samples)
  table.sort(sorted)
  local sum = 0
  for _, v in ipairs(sorted) do sum = sum + v end
  local mean = sum / n
  local variance = 0
  for _, v in ipairs(sorted) do variance = variance + (v - mean) ^ 2 end
  local stddev = n > 1 and math.sqrt(variance / (n - 1)) or 0
  local function pctile(p)
    if n == 1 then return sorted[1] end
    local idx = math.max(1, math.min(n, math.ceil(n * p)))
    return sorted[idx]
  end
  return {
    n = n,
    min = sorted[1],
    max = sorted[n],
    mean = mean,
    stddev = stddev,
    p50 = pctile(0.50),
    p95 = pctile(0.95),
    p99 = pctile(0.99),
    sum = sum,
    sorted = sorted,
  }
end

-- Render a single-line ASCII histogram into `width` chars. Uses unicode
-- block glyphs for sub-character granularity. Blocks fall back if your
-- terminal font lacks them, which is fine -- they're just visual weight.
local BLOCKS = { ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}',
                 '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}' }

function M.histogram(sorted, width)
  local n = #sorted
  if n < 2 then return string.rep(' ', width) end
  local min, max = sorted[1], sorted[n]
  if max <= min then return string.rep(BLOCKS[#BLOCKS], width) end
  local counts = {}
  for i = 1, width do counts[i] = 0 end
  for _, v in ipairs(sorted) do
    local t = (v - min) / (max - min)
    local b = math.min(width, math.max(1, math.floor(t * width) + 1))
    counts[b] = counts[b] + 1
  end
  local cmax = 0
  for _, c in ipairs(counts) do if c > cmax then cmax = c end end
  if cmax == 0 then return string.rep(' ', width) end
  local out = {}
  for i = 1, width do
    local idx = math.min(#BLOCKS, math.max(1, math.floor(counts[i] / cmax * (#BLOCKS - 1)) + 1))
    out[i] = BLOCKS[idx]
  end
  return table.concat(out)
end

local function method_list_by_count(timings)
  local list = {}
  for method, t in pairs(timings) do
    table.insert(list, { method = method, stats = M.summarize(t.samples) })
  end
  table.sort(list, function(a, b)
    if not a.stats then return false end
    if not b.stats then return true end
    return a.stats.n > b.stats.n
  end)
  return list
end

local function build_lines()
  local timings = state.get().request_timings or {}
  local lines = {}
  local function push(s) table.insert(lines, s) end
  local list = method_list_by_count(timings)
  if #list == 0 then
    push(' no request timings captured yet')
    push('')
    push(' (trigger some LSP activity in a rock buffer first)')
    return lines
  end
  -- Header row: pad to stable columns.
  push(' PER-METHOD LATENCY (client-observed, ms)')
  push('')
  push((' %-30s %5s %6s %6s %6s %6s %6s %6s  %s'):format(
    'method', 'n', 'min', 'mean', 'p50', 'p95', 'p99', 'max', 'hist'))
  push(' ' .. string.rep('-', 106))
  for _, e in ipairs(list) do
    local s = e.stats
    if s then
      local hist = M.histogram(s.sorted, 40)
      push((' %-30s %5d %6.2f %6.2f %6.2f %6.2f %6.2f %6.2f  %s'):format(
        e.method:sub(1, 30), s.n, s.min, s.mean, s.p50, s.p95, s.p99, s.max, hist))
    end
  end
  -- Totals across all methods.
  local grand = 0
  for _, e in ipairs(list) do if e.stats then grand = grand + e.stats.n end end
  push('')
  push((' total requests measured: %d   (capped at 500 samples per method)'):format(grand))
  push(' keys:  r  reset samples   q/<Esc>  close')
  return lines
end

function M.is_open()
  return FLOAT_WIN and vim.api.nvim_win_is_valid(FLOAT_WIN)
end

function M.close()
  if M.is_open() then pcall(vim.api.nvim_win_close, FLOAT_WIN, true) end
  FLOAT_WIN = nil
end

local function refresh_buf()
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then return end
  local lines = build_lines()
  vim.api.nvim_set_option_value('modifiable', true, { buf = FLOAT_BUF })
  vim.api.nvim_buf_set_lines(FLOAT_BUF, 0, -1, false, lines)
  vim.api.nvim_set_option_value('modifiable', false, { buf = FLOAT_BUF })
end

function M.open()
  if M.is_open() then M.close(); return end
  if not FLOAT_BUF or not vim.api.nvim_buf_is_valid(FLOAT_BUF) then
    FLOAT_BUF = require('rock.ui').ensure_named_buffer('rock://perf')
    vim.keymap.set('n', 'q', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', '<esc>', M.close, { buffer = FLOAT_BUF, silent = true })
    vim.keymap.set('n', 'r', function()
      state.clear_request_timings()
      refresh_buf()
    end, { buffer = FLOAT_BUF, silent = true })
  end
  refresh_buf()

  local w = math.min(120, math.floor(vim.o.columns * 0.95))
  local h = math.min(math.max(#build_lines() + 2, 10), math.floor(vim.o.lines * 0.85))
  FLOAT_WIN = vim.api.nvim_open_win(FLOAT_BUF, true, {
    relative = 'editor', width = w, height = h,
    row = math.floor((vim.o.lines - h) / 2),
    col = math.floor((vim.o.columns - w) / 2),
    style = 'minimal', border = 'rounded',
    title = ' rock.nvim: per-method latency ', title_pos = 'center',
  })
  vim.api.nvim_set_option_value('wrap', false, { win = FLOAT_WIN })
  -- Live refresh while open. Cheap: only re-renders this buffer.
  local timer = (vim.uv or vim.loop).new_timer()
  timer:start(1000, 1000, vim.schedule_wrap(function()
    if M.is_open() then refresh_buf() else timer:stop(); timer:close() end
  end))
end

function M.toggle()
  if M.is_open() then M.close() else M.open() end
end

return M
