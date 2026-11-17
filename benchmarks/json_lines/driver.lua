local emitted = assert(arg[1], "usage: luajit driver.lua EMITTED.lua validate|records time|heap|trace|profile INPUT.jsonl")
local mode = assert(arg[2], "missing mode")
local metric = assert(arg[3], "missing metric")
local input_path = assert(arg[4], "missing JSONL input")
assert(mode == "validate" or mode == "records", "mode must be validate or records")
assert(metric == "time" or metric == "heap" or metric == "trace" or metric == "profile",
  "metric must be time, heap, trace or profile")
assert(jit.status(), "LuaJIT must be enabled")

dofile(emitted)
local workload = mode == "validate" and json_lines_validate or json_lines_records
assert(type(workload) == "function", "emitted benchmark function is missing")

local input = assert(io.open(input_path, "rb"), "cannot open JSONL input")
local rows = {}
local input_bytes = 0
for line in input:lines() do
  rows[#rows + 1] = line
  input_bytes = input_bytes + #line + 1
end
input:close()
assert(#rows > 0, "JSONL input is empty")
assert(#rows >= 3, "JSONL input must have at least three documents")

local lines = { _data = rows, len = #rows }

local trace = { starts = 0, stops = 0, aborts = 0, abort_reasons = {} }
local trace_callback
if metric == "trace" then
  local vmdef = require("jit.vmdef")
  local util = require("jit.util")
  trace_callback = function(event, _, func, pc, code, extra)
    if event == "start" then
      trace.starts = trace.starts + 1
    elseif event == "stop" then
      trace.stops = trace.stops + 1
    elseif event == "abort" then
      trace.aborts = trace.aborts + 1
      local template = vmdef.traceerr[code]
      local reason = template or ("trace abort " .. tostring(code))
      if template then
        local ok, formatted = pcall(string.format, template, extra)
        if ok then reason = formatted end
      end
      local location = "unknown:0"
      if func then
        local info = util.funcinfo(func, pc)
        if info then
          local source = (info.source or "unknown"):gsub("^@", ""):gsub("^.*[/\\]", "")
          location = source .. ":" .. tostring(info.currentline or info.linedefined or 0)
        end
      end
      reason = reason .. " @ " .. location
      trace.abort_reasons[reason] = (trace.abort_reasons[reason] or 0) + 1
    end
  end
  jit.attach(trace_callback, "trace")
end

local warm_checksum
for _ = 1, 3 do warm_checksum = workload(lines) end
assert(warm_checksum ~= nil, "warmup did not return a checksum")

local profile = { samples = 0, vm_states = {}, functions = {} }
if metric == "profile" then
  local profiler = require("jit.profile")
  local function sampled_name(thread)
    local stack = profiler.dumpstack(thread, "fl", 1)
    return stack:gsub("^.*[/\\]", "")
  end
  profiler.start("i1", function(thread, samples, state)
    profile.samples = profile.samples + samples
    profile.vm_states[state] = (profile.vm_states[state] or 0) + samples
    local name = sampled_name(thread)
    profile.functions[name] = (profile.functions[name] or 0) + samples
  end)
  local checksum = 0
  for _ = 1, 200 do checksum = checksum + workload(lines) end
  profiler.stop()
  profile.checksum = checksum
  profile.docs = #rows * 200
elseif metric == "time" then
  local checksum = 0
  local started = os.clock()
  for _ = 1, 20 do checksum = checksum + workload(lines) end
  local elapsed = os.clock() - started
  profile.checksum = checksum
  profile.docs = #rows * 20
  profile.ns_per_doc = elapsed * 1000000000 / profile.docs
  profile.mb_per_second = input_bytes * 20 / 1000000 / elapsed
elseif metric == "heap" then
  collectgarbage("collect")
  collectgarbage("stop")
  local before = collectgarbage("count")
  local checksum = workload(lines)
  local growth = (collectgarbage("count") - before) * 1024
  collectgarbage("restart")
  profile.checksum = checksum
  profile.docs = #rows
  profile.heap_bytes_per_input_mb = growth / (input_bytes / 1000000)
else
  local checksum = 0
  for _ = 1, 20 do checksum = checksum + workload(lines) end
  profile.checksum = checksum
  profile.docs = #rows * 20
end

if trace_callback then jit.attach(trace_callback) end

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

local function json_encode(value)
  local kind = type(value)
  if kind == "nil" then return "null" end
  if kind == "boolean" then return value and "true" or "false" end
  if kind == "number" then return string.format("%.17g", value) end
  if kind == "string" then return json_escape(value) end
  local keys = {}
  for key in pairs(value) do keys[#keys + 1] = key end
  table.sort(keys)
  local items = {}
  for _, key in ipairs(keys) do
    items[#items + 1] = json_escape(key) .. ":" .. json_encode(value[key])
  end
  return "{" .. table.concat(items, ",") .. "}"
end

io.write(json_encode({
  mode = mode,
  metric = metric,
  docs = profile.docs,
  input_bytes = input_bytes,
  checksum = profile.checksum,
  ns_per_doc = profile.ns_per_doc,
  mb_per_second = profile.mb_per_second,
  heap_bytes_per_input_mb = profile.heap_bytes_per_input_mb,
  trace = trace,
  profile = {
    samples = profile.samples,
    vm_states = profile.vm_states,
    functions = profile.functions,
  },
}), "\n")
