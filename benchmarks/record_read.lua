-- Run with emitted record_read.rock Lua as the first argument.
-- Reports ns/element, result sink and machine-code bytes; LuaJIT stays on.
dofile(arg[1])
local jit = require('jit')
local util = require('jit.util')
assert(jit.status())
local n, reps = 4096, 10000
local xs = {_data={}, len=n}
for i=1,n do xs._data[i]=0 end
local report = {offset=7}
for _, name in ipairs({'private', 'foreign'}) do
    local function fill()
        if name == 'private' then return fill_report(xs, 7) end
        return fill_foreign_report(xs, report)
    end
    jit.flush()
    for i=1,200 do fill() end
    collectgarbage('collect')
    local start = os.clock()
    local sink = 0
    for i=1,reps do sink = sink + fill().offset + xs._data[n] end
    local elapsed = os.clock() - start
    local bytes = 0
    for i=1,10000 do
        local code = util.tracemc(i)
        if code then bytes = bytes + #code end
    end
    assert(sink == reps * (n + 13))
    print(string.format('%s %.5f %.0f %d', name, elapsed*1e9/(n*reps), sink, bytes))
end
