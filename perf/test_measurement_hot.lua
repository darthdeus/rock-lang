local count = assert(tonumber(arg[1]))
jit.off()
jit.opt.start("maxmcode=65536", "sizemcode=65536")
local chunks = {}
for i = 1, 96 do
    if count > 0 then
        chunks[i] = assert(loadstring("return function(x) " ..
            ("x = x + 1; "):rep((count + i) % 97) .. "return x end", "=unused"))
    end
    local active = assert(loadstring([[
        local sum = 0
        for i = 1, 11 do sum = sum + i % 3 end
        return sum
    ]], "=active-" .. i))
    chunks[96+i] = i % 2 == 0 and assert(loadstring(string.dump(active))) or active
end
collectgarbage("collect")
collectgarbage("stop")
jit.on()
local totals, compiled = {}, false
local function trace(event, _, func, pc, other)
    if event == "stop" then compiled = true end
    if event == "start" or event == "abort" then
        local name = debug.getinfo(func, "S").source
        if name:sub(1, 8) == "=active-" then
            local reason = event == "abort" and tostring(other) or ""
            local key = name .. ":" .. pc .. ":" .. event .. ":" .. reason
            totals[key] = (totals[key] or 0) + 1
        end
    end
end
jit.attach(trace, "trace")
local sum = 0
for iteration = 1, 1024 do
    for i = 97, 192 do
        sum = sum + chunks[i]()
    end
end
jit.attach(trace)
assert(compiled and jit.status())
assert(sum == 1179648, sum)
local rows = {}
for key, n in pairs(totals) do rows[#rows+1] = key .. "\t" .. n end
table.sort(rows)
for _, row in ipairs(rows) do print(row) end
