local util = require("jit.util")
local names = require("jit.vmdef").bcnames
local bit = require("bit")
local band, shr = bit.band, bit.rshift

-- Only the global environment is tracked. Values read from one of its fields
-- are ordinary values; dynamic access or escape of the environment is unknown.
local function references(fn, result)
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
            if input[b] and (op == "TGETV" or op == "TSETV") then unknown(op) end
            if op:sub(1, 4) == "TSET" and input[a] then unknown(op) end
            if op:sub(-1) == "V" and input[c] then unknown(op) end
        elseif op:sub(1, 4) == "CALL" or op == "ITERC" or op == "ITERN" then
            -- Calls use a contiguous temporary frame. Passing the environment
            -- to rawget, a helper or a vararg call requires further analysis.
            for reg in pairs(input) do
                if reg >= a then unknown(op); state[reg] = nil end
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
            -- A nested function may capture any live environment alias.
            if next(input) then unknown(op) end
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
        if type(k) == "proto" then references(k, result) end
        if type(k) == "string" then result.strings[k] = true end
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
        local result = { globals = {}, fields = {}, unresolved = {}, strings = {} }
        references(chunk(), result)
        emit("G", index, result.globals)
        emit("K", index, result.fields)
        if next(result.unresolved) then
            -- Preserve the previous conservative string census for reflection
            -- we cannot resolve, and report that limit instead of calling it exact.
            emit("C", index, result.strings)
            emit("U", index, result.unresolved)
        end
    end
end
