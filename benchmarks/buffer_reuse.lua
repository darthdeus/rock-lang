-- Run with emitted buffer_reuse.rock Lua as the first argument.
-- Reports ns/element, sink and machine-code bytes with LuaJIT enabled.
dofile(arg[1])
local jit = require('jit')
local util = require('jit.util')
assert(jit.status())
local rows, reps = 1024, 4000
local n = rows * 10
for _, name in ipairs({'reuse', 'allocate'}) do
    local old = {ptr=ffi.new('float[?]', n), len=name == 'reuse' and n or 0}
    jit.flush()
    for i=1,200 do reuse_rows(old, rows) end
    collectgarbage('collect')
    local start = os.clock()
    local sink = 0
    for i=1,reps do
        local buf = reuse_rows(old, rows)
        sink = sink + tonumber(buf.ptr[n-1])
    end
    local elapsed = os.clock() - start
    local bytes = 0
    for i=1,10000 do
        local code = util.tracemc(i)
        if code then bytes = bytes + #code end
    end
    assert(sink == reps * (rows + 8))
    print(string.format('%s %.5f %.0f %d', name, elapsed*1e9/(n*reps), sink, bytes))
end
