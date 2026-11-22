local ffi = require("ffi")
ffi.cdef[[
    int luaJIT_rock_explain(int costs);
    int luaJIT_rock_sites(int mode, const char *path);
]]
local sites = ffi.C.luaJIT_rock_sites
jit.off()
jit.opt.start("hotloop=3", "hotexit=2")
local sink, iterations = {}, 72
local function boxed(i)
    return {i}
end
local function exercise()
    for i = 1, iterations do
        local box = boxed(i)
        if i % 4 == 0 then box = {i, i + 1} end
        sink[i] = box
    end
end
local function interpreted()
    for i = 1, 10 do sink[i] = {i} end
end
jit.off(interpreted)
assert(ffi.C.luaJIT_rock_explain(tonumber(arg[2]) or 0) == 1)
assert(sites(1, nil) == 1)
jit.on()
exercise()
assert(sites(2, nil) == 1)
for _ = 1, 5 do exercise() end
interpreted()
assert(sites(1, nil) == 1)
jit.off()
local target = {value = false}
local address = tonumber(tostring(target):match("0x(%x+)$"), 16)
local header = ffi.cast("uint8_t *", address)
local function is_black()
    -- GC64's mark byte follows the eight-byte next-object pointer.
    return bit.band(header[8], 4) ~= 0
end
for _ = 1, 1000 do
    if is_black() then break end
    collectgarbage("step", 0)
end
assert(is_black())
collectgarbage("stop")
target.value = {}
assert(not is_black(), "table store skipped the GC write barrier")

local function warmed(n, take)
    local sum = 0
    for i = 1, n do
        sum = sum + i
        if take then return sum end
    end
    return sum
end
local function distinct(n)
    local sum = 0
    for i = 1, n do sum = sum + i % 31 end
    return sum
end
jit.opt.start("hotloop=2", "hotexit=100")
jit.on()
assert(warmed(100, false) == 5050)
local calls = 0
local function observe()
    calls = calls + 1
    assert(warmed(100, true) == 1)
end
jit.off(observe)
jit.attach(observe, "trace")
assert(distinct(30) == 465)
jit.attach(observe)
assert(calls > 0)

assert(sites(0, arg[1]) == 1)
assert(sink[iterations][2] == iterations + 1)
