local util = require("jit.util")
local names = require("jit.vmdef").bcnames
local wanted = {}
for line in io.lines() do
    local first, pc = line:match("^(%d+) (%d+)$")
    first, pc = tonumber(first), tonumber(pc)
    wanted[first] = wanted[first] or {}
    wanted[first][pc] = true
end

local function visit(proto)
    local info = util.funcinfo(proto)
    for pc in pairs(wanted[info.linedefined] or {}) do
        local word = util.funcbc(proto, pc)
        if word then
            local offset = 6 * (word % 256)
            local op = names:sub(offset + 1, offset + 6):gsub(" +$", "")
            local line = util.funcinfo(proto, pc).currentline
            io.write(info.linedefined, " ", line, " ", pc, " ", info.params, " ", info.stackslots, " ", op, "\n")
        end
    end
    for i = 1, info.gcconsts do
        local child = util.funck(proto, -i)
        if type(child) == "proto" then visit(child) end
    end
end

visit(assert(loadfile(arg[1])))
