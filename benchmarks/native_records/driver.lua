local file, mode, work = arg[1], arg[2], arg[3]
local n, reps = assert(tonumber(arg[4])), assert(tonumber(arg[6]))
local metric = arg[5]
assert(jit.status(), "JIT must be enabled")
dofile(file)
if metric == "verify" then
    verify_records(n)
    return
end

local trial, digest = record_trial(mode, work, n)
local function batch(count)
    local sink = 0
    for i = 1, count do sink = sink + trial() end
    return sink
end

local phase, active, phases = "warm", {}, {warm = {}, sample = {}}
local util, vmdef
local function location(fn, pc)
    local info = util.funcinfo(fn, pc)
    return info.linedefined or 0, info.currentline or 0
end

local function trace(event, id, fn, pc, other, extra)
    local events = phases[phase]
    if event == "start" then
        local definition, line = location(fn, pc)
        active[id] = {definition, line, other or 0, extra or 0}
    elseif event == "stop" then
        local start = assert(active[id])
        start[5] = assert(util.traceinfo(id)).linktype
        events[#events + 1] = {"compiled", id, unpack(start)}
    elseif event == "abort" then
        local start = assert(active[id])
        local template = vmdef.traceerr[other]
        if template == "NYI: bytecode %s" then
            extra = vmdef.bcnames:sub(extra * 6 + 1, extra * 6 + 6):match("%S+")
        elseif type(extra) == "function" then
            extra = util.funcinfo(extra).loc
        end
        local ok, reason = pcall(string.format, template, extra)
        local _, abort_line = location(fn, pc)
        events[#events + 1] = {"abort", id, start[1], start[2], start[3], start[4],
                              ok and reason or template, abort_line}
    elseif event == "flush" then
        events[#events + 1] = {"flush"}
    end
end

local exits = {}
local function texit(id, exit)
    local by_trace = exits[id]
    if not by_trace then
        by_trace = {}
        exits[id] = by_trace
    end
    by_trace[exit] = (by_trace[exit] or 0) + 1
end

-- Setup can run a different loop. Start each workload with a fresh trace cache.
jit.flush()
if metric == "trace" then
    util, vmdef = require("jit.util"), require("jit.vmdef")
    jit.attach(trace, "trace")
elseif metric == "dump" then
    require("jit.dump").start("Tbi", assert(arg[7]))
end

local warmup = work == "bulk" and (mode == "native" and 128 or 12) or 24
batch(warmup)
collectgarbage("collect")

local sink, result, offheap = 0, 0, 0
if metric == "time" then
    local start = os.clock()
    sink = batch(reps)
    result = (os.clock() - start) * 1e9 / (n * reps)
elseif metric == "heap" then
    local stats = __ROCK_NB_OFFHEAP_STATS
    collectgarbage("stop")
    local before_lua, before_native = collectgarbage("count"), stats.allocated_bytes
    local before_steps, before_collections = stats.forced_steps, stats.forced_collections
    sink = batch(reps)
    result = (collectgarbage("count") - before_lua) * 1024 / (n * reps)
    offheap = (stats.allocated_bytes - before_native) / (n * reps)
    collectgarbage("restart")
    assert(stats.forced_steps == before_steps and stats.forced_collections == before_collections,
           "native allocator collected during the heap sample")
elseif metric == "trace" then
    phase = "sample"
    jit.attach(texit, "texit")
    sink = batch(reps)
    jit.attach(texit)
    jit.attach(trace)
    for _, name in ipairs({"warm", "sample"}) do
        for _, event in ipairs(phases[name]) do
            print(name .. "\t" .. table.concat(event, "\t"))
        end
    end
    for id, by_trace in pairs(exits) do
        for exit, count in pairs(by_trace) do
            print(string.format("exit\t%d\t%d\t%d", id, exit, count))
        end
    end
else
    assert(metric == "dump")
    sink = batch(reps)
    require("jit.dump").off()
end
assert(jit.status())
print(string.format("result\t%.9f\t%.9f\t%.17g\t%.17g", result, offheap, sink, digest()))
