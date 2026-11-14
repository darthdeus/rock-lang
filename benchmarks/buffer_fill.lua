-- Run with the emitted buffer_fill.rock Lua file as the first argument.
-- Reports ns/element, result sink, final traces and machine-code bytes.
dofile(arg[1])
local jit = require('jit')
local util = require('jit.util')
assert(jit.status())
local n, reps = 4096, 10000
local workloads = {
    {'ints', function() return fill_ints(n) end},
    {'bytes', function() return fill_bytes(64, 64) end},
    {'recomputed', function() return fill_bytes_recomputed(64, 64) end},
}
for _, workload in ipairs(workloads) do
    jit.flush()
    local f = workload[2]
    for i = 1, 200 do f() end
    collectgarbage('collect')
    local start = os.clock()
    local sink = 0
    for i = 1, reps do
        local buf = f()
        sink = sink + tonumber(buf.ptr[n - 1])
    end
    local elapsed = os.clock() - start
    local traces, bytes = 0, 0
    for i = 1, 10000 do
        local code = util.tracemc(i)
        if code then traces = traces + 1; bytes = bytes + #code end
    end
    print(string.format('%s %.5f %.0f %d %d', workload[1], elapsed * 1e9 / (n * reps), sink, traces, bytes))
end
