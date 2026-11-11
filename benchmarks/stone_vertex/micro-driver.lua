local fixture = arg[1]
if not fixture then error("usage: luajit micro-driver.lua FIXTURE [calls] [samples] [repetitions] [warmup-runs]") end
local calls = tonumber(arg[2]) or 2048
local samples = tonumber(arg[3]) or 7
local repetitions = tonumber(arg[4]) or 20
local warmup_runs = tonumber(arg[5]) or 8
local ffi = require("ffi")
local util = require("jit.util")
local vmdef = require("jit.vmdef")
local clock = os.clock

dofile(fixture)
if not jit or not jit.status() then error("LuaJIT is not enabled") end
jit.on()

local current_case = nil
local phase = nil
local stats = {}
local function ensure_stats(name)
  if not stats[name] then
    stats[name] = {
      traces = {},
      warmup_abort_reasons = {},
      measure_abort_reasons = {},
    }
  end
  return stats[name]
end
local function bump(map, key)
  key = tostring(key or "unknown")
  map[key] = (map[key] or 0) + 1
end

jit.attach(function(what, trace, fn, pc, code, extra)
  if what == "stop" and current_case then
    local info = util.traceinfo(trace)
    if info then
      local target = ensure_stats(current_case).traces
      target[#target + 1] = {
        id = trace,
        linktype = info.linktype,
        link = info.link,
        nk = info.nk,
        nins = info.nins,
        nexits = info.nexit,
      }
    end
  elseif what == "abort" and current_case and phase then
    local template = vmdef.traceerr[code] or ("trace abort " .. tostring(code))
    local reason = template
    if extra ~= nil and template:find("%s", 1, true) then
      local ok, formatted = pcall(string.format, template, tostring(extra))
      if ok then reason = formatted end
    end
    local result = ensure_stats(current_case)
    if phase == "warmup" then
      bump(result.warmup_abort_reasons, reason)
    else
      bump(result.measure_abort_reasons, reason)
    end
  end
end, "trace")

local function median(values)
  table.sort(values)
  local n = #values
  if n % 2 == 1 then return values[(n + 1) / 2] end
  return (values[n / 2] + values[n / 2 + 1]) / 2
end
local function json_escape(value)
  return '"' .. tostring(value):gsub('[%z\1-\31\\"]', function(ch)
    if ch == "\\" then return "\\\\" end
    if ch == '"' then return '\\"' end
    if ch == "\n" then return "\\n" end
    if ch == "\r" then return "\\r" end
    if ch == "\t" then return "\\t" end
    return string.format("\\u%04x", string.byte(ch))
  end) .. '"'
end
local function is_array(value)
  local count, maximum = 0, 0
  for key in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return false end
    count = count + 1
    if key > maximum then maximum = key end
  end
  return count == maximum
end
local function json_encode(value)
  local kind = type(value)
  if kind == "nil" then return "null" end
  if kind == "boolean" then return value and "true" or "false" end
  if kind == "number" then
    if value ~= value or value == math.huge or value == -math.huge then return "null" end
    return string.format("%.17g", value)
  end
  if kind == "string" then return json_escape(value) end
  if kind ~= "table" then return json_escape(tostring(value)) end
  local pieces = {}
  if is_array(value) then
    for index = 1, #value do pieces[#pieces + 1] = json_encode(value[index]) end
    return "[" .. table.concat(pieces, ",") .. "]"
  end
  for key, item in pairs(value) do
    pieces[#pieces + 1] = json_escape(key) .. ":" .. json_encode(item)
  end
  return "{" .. table.concat(pieces, ",") .. "}"
end

local output = {
  source_map = writer_source_map,
  settings = { calls = calls, samples = samples, repetitions = repetitions, warmup_runs = warmup_runs },
  workloads = {},
}
for _, case in ipairs(writer_cases) do
  current_case = case.name
  local case_calls = math.min(calls, case.max_calls or calls)
  local record = ensure_stats(case.name)
  phase = "warmup"
  for _ = 1, warmup_runs do case.run(case_calls) end

  phase = "measure"
  local values = {}
  for sample = 1, samples do
    local started = clock()
    for _ = 1, repetitions do case.run(case_calls) end
    local elapsed = clock() - started
    local ns_per_call = elapsed * 1000000000 / (case_calls * repetitions)
    values[sample] = ns_per_call * 6 / case.vertices_per_call
  end
  table.sort(values)
  output.workloads[#output.workloads + 1] = {
    name = case.name,
    vertices_per_call = case.vertices_per_call,
    calls = case_calls,
    calls_per_sample = case_calls * repetitions,
    min_ns_per_quad = values[1],
    median_ns_per_quad = median(values),
    samples_ns_per_quad = values,
    trace_nk = record.traces,
    warmup_abort_reasons = record.warmup_abort_reasons,
    measure_abort_reasons = record.measure_abort_reasons,
  }
end
phase = nil
current_case = nil
io.write(json_encode(output), "\n")
