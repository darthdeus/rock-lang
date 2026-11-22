dofile(assert(arg[1]))
local guard = assert(_G.__stone_jit_churn_guard, "measurement needs a Stone churn guard")
assert(guard.enabled and guard.armed == 0 and guard.blacklisted == 0)

local ffi, util = require("ffi"), require("jit.util")
local thread = coroutine.create(function() end)
local L = ffi.cast("void *", tonumber(string.format("%p", thread), 16))
assert(ffi.C.luaJIT_rock_churn(L, 0, 0, 0) == -1)

local loop = assert(loadstring([[
    local f
    for i = 1, 512 do
        f = function() return i end
    end
    return f()
]], "=churn"))
jit.opt.start("hotloop=1")
local first = 0
local source, pos
local function observe(event, trace, func, pc, other, extra)
    if event == "start" then source, pos = func, pc end
    if event == "abort" and source == loop and guard.armed == 0 then
        local proto = tonumber(string.format("%p", util.funcinfo(source, pos).proto), 16)
        assert(ffi.C.luaJIT_rock_churn(L, proto, pos + 1, 0) == -1)
        -- Repeat a real penalty event without waiting for the exponential
        -- counter to reach the VM's natural blacklist threshold.
        for i = 1, 12 do
            guard.event("start", trace, source, pos, nil, nil)
            guard.event(event, trace, func, pc, other, extra)
            assert(guard.armed == 0)
            first = first + 1
        end
        guard.event("start", trace, source, pos, nil, nil)
    end
    guard.event(event, trace, func, pc, other, extra)
end
jit.attach(observe, "trace")
assert(loop() == 512)
jit.attach(observe)
assert(guard.enabled, guard.failure)
assert(first == 12 and guard.armed == 1 and guard.blacklisted == 1)
assert(#guard.sites == 1 and guard.sites[1].confirmed)
