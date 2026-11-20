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

if os.getenv("PERF_FIXED_WORK") == "1" then
    jit.off()
    local previous = _G.__rock_allocation_activation_frame_begin
    local frame = 0
    _G.__rock_allocation_activation_frame_begin = function()
        frame = frame + 1
        if frame == 3 then
            jit.flush()
            collectgarbage("collect")
            jit.on()
        end
        if previous then previous() end
    end
    jit.off(_G.__rock_allocation_activation_frame_begin)
end
