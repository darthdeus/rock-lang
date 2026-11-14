-- Run with the emitted uint_shift.rock Lua file as the first argument.
-- Reports ns/iteration, result sink, final traces and machine-code bytes.
dofile(arg[1])
local jit = require('jit')
assert(jit.status())
jit.flush()
for i = 1, 200 do shift_mix(i, 1000) end
collectgarbage('collect')
local start = os.clock()
local sink = 0
for i = 1, 20000 do sink = sink + shift_mix(i, 1000) end
local elapsed = os.clock() - start
local util = require('jit.util')
local traces, bytes = 0, 0
for i = 1, 10000 do
 local code = util.tracemc(i)
 if code then traces = traces + 1; bytes = bytes + #code end
end
print(string.format('%.5f %.0f %d %d', elapsed * 1e9 / 20000000, sink, traces, bytes))
