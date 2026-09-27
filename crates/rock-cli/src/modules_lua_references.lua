local util = require("jit.util")
local names = require("jit.vmdef").bcnames
local bit = require("bit")
local band, shr = bit.band, bit.rshift

local function uleb(s, pos)
    local v, scale = 0, 1
    while true do
        local b = s:byte(pos)
        if not b then return nil end
        pos = pos + 1
        v = v + (b % 128) * scale
        if b < 128 then return v, pos end
        scale = scale * 128
    end
end

-- A two-slot frame (LJ_FR2, BCDUMP_F_FR2) puts a call's first argument at
-- base + 2 instead of base + 1.
local ARG1 = 1
do
    local flags = uleb(string.dump(function() end, true), 5)
    if flags and math.floor(flags / 8) % 2 == 1 then ARG1 = 2 end
end

-- The parent stack slots each nested prototype captures, read from the
-- stripped bytecode dump, which lists prototypes children first in constant
-- order. nil when the dump does not match jit.util's view of the tree.
local function captured_slots(fn)
    local order = {}
    local function walk(f)
        for n = -1, -util.funcinfo(f).gcconsts, -1 do
            local k = util.funck(f, n)
            if type(k) == "proto" then walk(k) end
        end
        order[#order + 1] = f
    end
    walk(fn)
    local ok, s = pcall(string.dump, fn, true)
    if not ok or s:sub(1, 3) ~= "\27LJ" then return nil end
    local flags, pos = uleb(s, 5)
    if not flags then return nil end
    local big_endian = flags % 2 == 1
    local out, index = {}, 0
    while true do
        local len
        len, pos = uleb(s, pos)
        if not len then return nil end
        if len == 0 then break end
        local start = pos
        local nuv = s:byte(pos + 3)
        local nkgc, nkn, nbc
        nkgc, pos = uleb(s, pos + 4)
        if nkgc then nkn, pos = uleb(s, pos) end
        if nkn then nbc, pos = uleb(s, pos) end
        if not nuv or not nbc then return nil end
        pos = pos + nbc * 4
        index = index + 1
        local proto = order[index]
        if not proto then return nil end
        local info = util.funcinfo(proto)
        if info.bytecodes ~= nbc + 1 or info.upvalues ~= nuv then return nil end
        local slots = {}
        for _ = 1, nuv do
            local x, y = s:byte(pos, pos + 1)
            if not y then return nil end
            local uv = big_endian and (x * 256 + y) or (y * 256 + x)
            -- PROTO_UV_LOCAL: a parent stack slot, not a parent upvalue.
            if uv >= 0x8000 then slots[#slots + 1] = uv % 256 end
            pos = pos + 2
        end
        out[proto] = slots
        pos = start + len
    end
    if index ~= #order then return nil end
    return out
end

-- Only the global environment is tracked. Values read from one of its fields
-- are ordinary values. A dynamic index of the environment leaves it tracked;
-- any other use lets it escape, after which every string is a candidate.
local function references(fn, result, captures)
    local code = {}
    for pc = 1, util.funcinfo(fn).bytecodes - 1 do
        local ins, mode = util.funcbc(fn, pc)
        local opid = band(ins, 255)
        local bmode = band(shr(mode, 3), 15)
        local cmode = band(shr(mode, 7), 15)
        local d = shr(ins, 16)
        local c = bmode ~= 0 and band(d, 255) or d
        code[pc] = {
            op = names:sub(opid * 6 + 1, opid * 6 + 6):match("%S+"),
            a = band(shr(ins, 8), 255), b = shr(ins, 24), c = c,
            amode = band(mode, 7), bmode = bmode, cmode = cmode,
            key = cmode == 10 and util.funck(fn, -c - 1) or nil,
            jump = cmode == 13 and pc + d - 0x7fff or nil,
        }
    end
    local targets = {}
    for _, i in pairs(code) do
        if i.jump then targets[i.jump] = true end
        if i.op == "KSTR" then result.values[i.key] = true end
    end
    -- The instruction that last wrote `reg` before `pc` in straight-line
    -- code, or nil when a jump target or a multi-register write intervenes.
    local function last_def(pc, reg)
        for q = pc - 1, 1, -1 do
            if targets[q + 1] then return nil end
            local i = code[q]
            if i.amode == 1 and i.a == reg then return i end
            if i.amode == 2 and reg >= i.a then return nil end
        end
        return nil
    end
    local states, queue = { [1] = {} }, { 1 }
    local function enqueue(pc, state)
        if not code[pc] then return end
        local dest = states[pc]
        local changed = dest == nil
        if not dest then dest = {}; states[pc] = dest end
        for reg in pairs(state) do
            if not dest[reg] then dest[reg] = true; changed = true end
        end
        if changed then queue[#queue + 1] = pc end
    end
    local function unknown(op)
        result.unresolved["unresolved _G flow at " .. op] = true
        result.escapes = true
    end
    local function dynamic(op)
        result.unresolved["dynamic _G key at " .. op] = true
    end
    local next_item = 1
    while next_item <= #queue do
        local pc = queue[next_item]
        next_item = next_item + 1
        local input, state = states[pc], {}
        for reg in pairs(input) do state[reg] = true end
        local i = code[pc]
        local op, a, b, c = i.op, i.a, i.b, i.c
        if i.amode == 1 then state[a] = nil end
        if op == "GGET" then
            result.globals[i.key] = true
            if i.key == "_G" then state[a] = true end
            if i.key == "next" then result.iterates = true end
        elseif op == "GSET" then
            result.globals[i.key] = true
            if input[a] then unknown(op) end
        elseif op == "MOV" or op == "ISTC" or op == "ISFC" then
            state[a] = input[c]
        elseif op == "TGETS" or op == "TSETS" then
            if input[b] then
                result.fields[i.key] = true
                if op == "TGETS" and i.key == "_G" then state[a] = true end
            end
            if op == "TSETS" and input[a] then unknown(op) end
        elseif op == "TGETV" or op == "TSETV" or op == "TGETB" or op == "TSETB" then
            if input[b] and (op == "TGETV" or op == "TSETV") then dynamic(op) end
            if op:sub(1, 4) == "TSET" and input[a] then unknown(op) end
            if op:sub(-1) == "V" and input[c] then unknown(op) end
        elseif op:sub(1, 4) == "CALL" or op == "ITERC" or op == "ITERN" then
            if op == "ITERC" or op == "ITERN" then result.iterates = true end
            -- Calls use a contiguous temporary frame. rawget/rawset on the
            -- environment is a field access; passing it to anything else lets
            -- it escape.
            local t, k, v = a + ARG1, a + ARG1 + 1, a + ARG1 + 2
            local callee = op == "CALL" and input[t] and last_def(pc, a)
            local raw = callee and callee.op == "GGET"
                and ((callee.key == "rawget" and c == 3) or (callee.key == "rawset" and c == 4))
                and callee.key
            if raw then
                local key = last_def(pc, k)
                if input[k] or (raw == "rawset" and input[v]) then
                    unknown(op)
                elseif key and key.op == "KSTR" then
                    result.fields[key.key] = true
                else
                    dynamic(raw)
                end
                for reg in pairs(input) do
                    if reg >= a then state[reg] = nil end
                end
            else
                for reg in pairs(input) do
                    if reg >= a then unknown(op); state[reg] = nil end
                end
            end
        elseif op:sub(1, 3) == "RET" then
            local last = (op == "RET0" and a - 1) or (op == "RET1" and a)
                or (op == "RET" and a + c - 2) or 255
            for reg in pairs(input) do
                if reg >= a and reg <= last then unknown(op) end
            end
        elseif op == "KNIL" then
            for reg = a, c do state[reg] = nil end
        elseif op == "FNEW" then
            -- A nested function sees only the slots it captures; without the
            -- capture list, any live environment alias.
            local slots = captures and captures[util.funck(fn, -c - 1)]
            if slots then
                for _, slot in ipairs(slots) do
                    if input[slot] then unknown(op) end
                end
            elseif next(input) then
                unknown(op)
            end
        elseif not i.jump and op:sub(1, 2) ~= "IS" then
            if (i.amode == 3 and input[a]) or (i.bmode == 3 and input[b])
                or (i.cmode == 3 and input[c]) then unknown(op) end
        end

        if i.jump then
            enqueue(i.jump, state)
            if op ~= "JMP" and op ~= "UCLO" then enqueue(pc + 1, state) end
        elseif op:sub(1, 2) == "IS" then
            enqueue(pc + 1, state)
            enqueue(pc + 2, input)
        elseif op:sub(1, 3) ~= "RET" and op ~= "CALLT" and op ~= "CALLMT" then
            enqueue(pc + 1, state)
        end
    end
    for n = -1, -util.funcinfo(fn).gcconsts, -1 do
        local k = util.funck(fn, n)
        if type(k) == "proto" then references(k, result, captures) end
        if type(k) == "string" then result.strings[k] = true end
        if type(k) == "table" then
            for key, value in pairs(k) do
                if type(key) == "string" then result.template_keys[key] = true end
                if type(value) == "string" then result.values[value] = true end
            end
        end
    end
end

local function emit(tag, index, values)
    for value in pairs(values) do
        io.write(tag, "\t", index, "\t", (value:gsub("[\t\r\n]", " ")), "\n")
    end
end

for index, text in ipairs(bodies) do
    local chunk, err = loadstring(text, "=inline")
    if not chunk then
        emit("E", index, { [tostring(err)] = true })
    else
        local result = {
            globals = {}, fields = {}, unresolved = {}, strings = {},
            values = {}, template_keys = {},
        }
        local fn = chunk()
        references(fn, result, captured_slots(fn))
        emit("G", index, result.globals)
        emit("K", index, result.fields)
        if next(result.unresolved) then
            -- Reflection we cannot resolve keeps a conservative string census
            -- and reports that limit instead of calling it exact. While the
            -- environment stays tracked, only a string that can be a runtime
            -- value can be its dynamic key: a constant loaded into a register,
            -- a table-constructor value, or a constructor key the body can
            -- iterate. A field key on another table is not.
            if result.escapes then
                emit("C", index, result.strings)
                emit("C", index, result.template_keys)
            elseif result.iterates then
                emit("C", index, result.template_keys)
            end
            emit("C", index, result.values)
            emit("U", index, result.unresolved)
        end
    end
end
