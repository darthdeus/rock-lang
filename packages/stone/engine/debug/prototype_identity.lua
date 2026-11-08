-- IDs are local to one loaded artifact: root first, then child prototypes in
-- negative KGC-slot order, matching the static bytecode census.
local util = require("jit.util")

return function(chunk)
    local ids, count = {}, 0
    local function walk(proto)
        local info = assert(util.funcinfo(proto), "prototype information unavailable")
        assert(info.proto, "prototype identity unavailable")
        ids[info.proto] = count
        count = count + 1
        for slot = -1, -(info.gcconsts or 0), -1 do
            local child = util.funck(proto, slot)
            if type(child) == "proto" then walk(child) end
        end
    end
    walk(chunk)
    return {
        count = count,
        id = function(info) return info.proto and ids[info.proto] end,
        describe = function(func)
            if not func then return nil end
            local info = util.funcinfo(func)
            local id = info and info.proto and ids[info.proto]
            if id == nil then return nil end
            return { function_id = id, source = info.source or "",
                definition_line = info.linedefined or 0, bytecodes = info.bytecodes or 0 }
        end,
    }
end
