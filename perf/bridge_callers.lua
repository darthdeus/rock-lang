-- Counts calls of generated value helpers (`_vt`, `_lct`) by calling line
-- during the allocation window. The wrappers change JIT behavior, so a run
-- with this hook explains another run's allocation counts but measures none.
local start = tonumber(os.getenv("ROCK_BRIDGE_WINDOW_START"))
local length = tonumber(os.getenv("ROCK_BRIDGE_WINDOW_LENGTH"))
local output = os.getenv("ROCK_BRIDGE_CALLERS")
local counts, counting, frame = {}, false, 0
local getinfo = debug.getinfo
local function dump()
    local file = assert(io.open(output, "w"))
    for helper, lines in pairs(counts) do
        for line, n in pairs(lines) do file:write(helper, "\t", line, "\t", n, "\n") end
    end
    file:close()
end
local function count(name, inner)
    local lines = {}
    counts[name] = lines
    return function(...)
        if counting then
            local line = getinfo(2, "l").currentline
            lines[line] = (lines[line] or 0) + 1
        end
        return inner(...)
    end
end
setmetatable(_G, { __newindex = function(globals, key, value)
    if type(key) == "string" and type(value) == "function" then
        if key == "__rock_allocation_activation_frame_begin" then
            local inner = value
            value = function()
                frame = frame + 1
                counting = frame >= start and frame < start + length
                if frame == start + length then dump() end
                return inner()
            end
        elseif key:match("_%x%x%x%x%x%x%x%x_vt$") or key:match("_%x%x%x%x%x%x%x%x_lct$") then
            value = count(key, value)
        end
    end
    rawset(globals, key, value)
end })
