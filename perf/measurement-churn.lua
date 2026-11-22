do
    local ffi = require("ffi")
    ffi.cdef[[int luaJIT_rock_churn(void *, uintptr_t, uint32_t, int);]]
    local info = require("jit.util").funcinfo
    local retry, mclimit
    for code, name in pairs(require("jit.vmdef").traceerr) do
        if name == "retry recording" then retry = code end
        if name == "hit mcode limit (retrying)" then mclimit = code end
    end
    assert(retry and mclimit, "trace-error ABI mismatch")
    local function address(object)
        return tonumber(string.format("%p", object), 16)
    end
    local state = {enabled = true, thread = coroutine.create(function() end),
        armed = 0, blacklisted = 0, flushes = 0, root_starts = 0, side_starts = 0,
        root_aborts = 0, side_aborts = 0, sites = {}}
    local L = ffi.cast("void *", address(state.thread))
    local native = ffi.C.luaJIT_rock_churn
    local rows, recording = {}, nil
    local func, pc, side, direct
    state.event = function(event, trace, source, pos, other, extra)
        if event == "start" then
            recording, func, pc = trace, source, pos
            side, direct = other ~= nil and extra ~= -1, other == nil
            if side then state.side_starts = state.side_starts + 1
            else state.root_starts = state.root_starts + 1 end
        elseif event == "flush" then
            rows, recording = {}, nil
            state.flushes = state.flushes + 1
        elseif event == "stop" then
            recording = nil
        elseif event == "abort" and trace == recording then
            recording = nil
            if side then state.side_aborts = state.side_aborts + 1
            else state.root_aborts = state.root_aborts + 1 end
            if not direct or other == retry or other == mclimit then return end
            local site = info(func, pc)
            local proto = site.proto and address(site.proto)
            local status = proto and native(L, proto, pc, 0) or -1
            if status < 0 then
                state.enabled, state.failure = false, "measurement churn ABI mismatch"
                return
            end
            if status == 0 then return end
            local key = proto + pc * 4
            local row = rows[key]
            if status == 2 then
                if row and row.site and not row.site.confirmed then
                    row.site.confirmed = true
                    state.blacklisted = state.blacklisted + 1
                end
                return
            end
            if not row then
                -- Keep the prototype alive so its address cannot be reused.
                row = {func = func, aborts = 0}
                rows[key] = row
            end
            row.aborts = row.aborts + 1
            if row.aborts > 12 then
                if native(L, proto, pc, 1) ~= 1 then
                    state.enabled, state.failure = false, "measurement churn arm failed"
                    return
                end
                if not row.site then
                    row.site = {addr = key, pc = pc, line = site.currentline,
                        loc = site.loc, aborts = row.aborts}
                    state.armed = state.armed + 1
                    state.sites[#state.sites + 1] = row.site
                end
            end
        end
    end
    -- Stone attaches its trace handler with the JIT off, before the reset.
    _G.__stone_jit_churn_guard = state
    local previous = _G.__rock_allocation_activation_dump
    _G.__rock_allocation_activation_dump = function(...)
        assert(state.enabled, state.failure)
        if previous then previous(...) end
    end
    jit.off(_G.__rock_allocation_activation_dump)
end
