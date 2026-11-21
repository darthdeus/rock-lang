if os.getenv("ROCK_PERF_CAPTURE_SITES") == "1" then
    local ffi = require("ffi")
    ffi.cdef[[int luaJIT_rock_sites(int mode, const char *path);]]
    local sites = ffi.C.luaJIT_rock_sites
    local previous = _G.__rock_allocation_activation_dump
    assert(sites(3, nil) == 1)
    _G.__rock_allocation_activation_dump = function()
        assert(sites(0, assert(os.getenv("PERF_OUT")) .. ".sites.tsv") == 1)
        if previous then previous() end
    end
    jit.off(_G.__rock_allocation_activation_dump)
end

local deterministic = os.getenv("ROCK_PERF_DETERMINISTIC") == "1"
local clock_frame
if deterministic then
    local ffi = require("ffi")
    ffi.cdef[[
        int luaJIT_rock_deterministic(void);
        void luaJIT_rock_clock_frame(uint32_t frame);
    ]]
    local found, probe = pcall(function() return ffi.C.luaJIT_rock_deterministic end)
    assert(found and probe() == 1, "expected the deterministic measurement LuaJIT")
    clock_frame = ffi.C.luaJIT_rock_clock_frame
    clock_frame(0)
end

if os.getenv("PERF_FIXED_WORK") == "1" then
    jit.off()
    local previous = _G.__rock_allocation_activation_frame_begin
    local frame = 0
    _G.__rock_allocation_activation_frame_begin = function()
        frame = frame + 1
        if clock_frame then clock_frame(frame) end
        if frame == 3 then
            jit.flush()
            collectgarbage("collect")
            if deterministic then collectgarbage("stop") end
            jit.on()
        elseif deterministic and frame % 32 == 3 then
            collectgarbage("collect")
            collectgarbage("stop")
        end
        if previous then previous() end
    end
    jit.off(_G.__rock_allocation_activation_frame_begin)
end
