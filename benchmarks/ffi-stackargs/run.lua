-- Driver for stackargs.c. Usage: luajit [-joff|-jv] run.lua <libpath>
-- Verifies every argument round-trips; a mismatch prints the arg index,
-- expected value, and received value (hex, to spot half-a-double garbage).
local ffi = require("ffi")

ffi.cdef[[
void capture_i32(int32_t *out,
    int32_t a1, int32_t a2, int32_t a3, int32_t a4, int32_t a5,
    int32_t a6, int32_t a7, int32_t a8, int32_t a9, int32_t a10,
    int32_t a11, int32_t a12);
void capture_mesh_shape(int32_t *out, uint64_t *out_tex,
    const float *data,
    int32_t count, int32_t stride, int32_t o3, int32_t o4, int32_t o5,
    int32_t o6, int32_t o7, int32_t o8, int32_t o9, int32_t o10,
    uint64_t tex);
void capture_u8(uint8_t *out,
    uint8_t b1, uint8_t b2, uint8_t b3, uint8_t b4, uint8_t b5,
    uint8_t b6, uint8_t b7, uint8_t b8, uint8_t b9, uint8_t b10);
uint64_t mesh_exact(const float *data,
    int32_t count, int32_t stride, int32_t pos_offset, int32_t uv_offset,
    int32_t color_offset, int32_t pivot_y_offset, int32_t effects_offset,
    int32_t outline_size_offset, int32_t outline_color_offset,
    int32_t entity_id_offset, uint64_t tex);
int stackargs_last_bad(void);
int wrapped10(const char *shader_key, const unsigned char *vertices,
    uint32_t vertex_bytes, int32_t first, int32_t count, int32_t stride,
    const char *layout_wire, const char *payload_digest,
    const char *binding_wire, int32_t blend);
]]

local libpath = arg[1] or "./libstackargs.so"
local lib = ffi.load(libpath)

print(("luajit %s, jit=%s, %s %s"):format(
    jit.version, tostring(jit.status()), jit.os, jit.arch))

local failures = 0

local function check(name, iter, got, expected)
    for i = 1, #expected do
        local g, e = tonumber(got[i - 1]), expected[i]
        if g ~= e then
            failures = failures + 1
            print(("FAIL %s iter=%d arg#%d expected=%d got=%d (0x%X)")
                :format(name, iter, i, e, g, g))
            return false
        end
    end
    return true
end

-- Hot-loop each shape so the FFI call gets trace-compiled when jit is
-- on (hotloop threshold is 56; 400 iters is plenty). Verify EVERY
-- iteration -- interpreter and JIT-compiled calls take different
-- argument-setup paths (lj_ccall.c vs lj_asm_<arch>.h) and both matter.
local ITERS = 400

local function shape_i32()
    local out = ffi.new("int32_t[12]")
    local exp = { 101, 202, 303, 404, 505, 606, 707, 808, 909, 1010, 1111, 1212 }
    for i = 1, ITERS do
        lib.capture_i32(out, 101, 202, 303, 404, 505, 606, 707, 808,
            909, 1010, 1111, 1212)
        if not check("capture_i32", i, out, exp) then return end
    end
    -- Tight phase: nothing but the FFI call in the loop body, so the
    -- call itself is guaranteed onto a compiled trace when jit is on.
    for _ = 1, ITERS do
        lib.capture_i32(out, 101, 202, 303, 404, 505, 606, 707, 808,
            909, 1010, 1111, 1212)
    end
    check("capture_i32/tight", -1, out, exp)
end

local function shape_mesh()
    local out = ffi.new("int32_t[11]")
    local out_tex = ffi.new("uint64_t[1]")
    local data = ffi.new("float[1]", 42)
    -- offsets mirror the original call site: count/stride then 8 layout
    -- offsets; tex is a u64 tail after an odd run of int32 stack args.
    local exp = { 4, 10, 0, 2, 4, 8, 9, 12, 13, 14, 42 }
    for i = 1, ITERS do
        lib.capture_mesh_shape(out, out_tex, data,
            4, 10, 0, 2, 4, 8, 9, 12, 13, 14, 7777777777ULL)
        local ok = check("capture_mesh_shape", i, out, exp)
        if tonumber(out_tex[0]) ~= 7777777777 then
            failures = failures + 1
            print(("FAIL capture_mesh_shape iter=%d tex expected=7777777777 got=%s")
                :format(i, tostring(out_tex[0])))
            ok = false
        end
        if not ok then return end
    end
    for _ = 1, ITERS do
        lib.capture_mesh_shape(out, out_tex, data,
            4, 10, 0, 2, 4, 8, 9, 12, 13, 14, 7777777777ULL)
    end
    check("capture_mesh_shape/tight", -1, out, exp)
end

local function shape_u8()
    local out = ffi.new("uint8_t[10]")
    local exp = { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 }
    for i = 1, ITERS do
        lib.capture_u8(out, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10)
        if not check("capture_u8", i, out, exp) then return end
    end
    for _ = 1, ITERS do
        lib.capture_u8(out, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10)
    end
    check("capture_u8/tight", -1, out, exp)
end

local function shape_exact()
    -- Values from the real rect_batch call site of the era (stride-14
    -- layout: pos 0, uv 2, color 4, py 8, eff 9, os 10, oc 11, eid 13).
    local data = ffi.new("float[1]", 5)
    local expected = 4 * 3 + 14 * 9 + 0 * 27 + 2 * 81 + 4 * 243
        + 8 * 729 + 9 * 2187 + 10 * 6561 + 11 * 19683 + 13 * 59049
        + 5 * 177147 + 7777777777
    local function one(iter)
        local got = tonumber(lib.mesh_exact(data,
            4, 14, 0, 2, 4, 8, 9, 10, 11, 13, 7777777777ULL))
        if got ~= expected then
            failures = failures + 1
            print(("FAIL mesh_exact iter=%d expected=%d got=%d (0x%X)")
                :format(iter, expected, got, got))
            return false
        end
        return true
    end
    for i = 1, ITERS do
        if not one(i) then return end
    end
    local acc = 0ULL
    for _ = 1, ITERS do
        acc = acc + lib.mesh_exact(data,
            4, 14, 0, 2, 4, 8, 9, 10, 11, 13, 7777777777ULL)
    end
    if tonumber(acc / ITERS) ~= expected then
        failures = failures + 1
        print(("FAIL mesh_exact/tight avg expected=%d got=%s")
            :format(expected, tostring(acc / ITERS)))
    end
end

-- Shape 5: a direct FFI call is not what Rock emits. Every `inline(lua)`
-- shim wraps the call in a Lua function and consumes the result in the
-- same expression, and that is the shape that corrupts -- hoisting the
-- `~= 0` out to the caller is clean. Needs ~100k iterations and live
-- values across the call; corruption first appears near iteration 84k
-- and affects a window of calls rather than all of them.
local WRAPPED_ITERS = 200000

local function shape_wrapped()
    local pre = ffi.cast("const unsigned char*", ffi.new("float[1024]"))
    local function shim(p0, p1, p2, p3, p4, p5, p6, p7, p8, p9)
        return lib.wrapped10(p0, p1, p2, p3, p4, p5, p6, p7, p8, p9) ~= 0
    end
    local hits, first_iter = 0, nil
    local a, b, c, d, e, f, g, h = 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5
    for i = 1, WRAPPED_ITERS do
        local uni = "U:" .. i
        local arr = "A:" .. (i * 3)
        local pay = "P:" .. (i % 97)
        if uni == arr then print("") end
        a = a + i * 0.5; b = b - a * 0.25; c = c + b; d = d * 1.0001
        e = e + c * 0.5; f = f - d; g = g + e * 0.125; h = h + f
        if shim("SHADER", pre, 4096, 0, 36, 44, "LAYOUT", pay, "BINDING", 3) then
            hits = hits + 1
            if first_iter == nil then first_iter = i end
        end
    end
    -- Keep the accumulators live so they cannot be dead-code eliminated.
    if a + b + c + d + e + f + g + h == 0 then print("") end
    if hits ~= 0 then
        failures = failures + 1
        print(("FAIL wrapped10 %d/%d calls garbled, first at iter=%d, argmask=0x%X")
            :format(hits, WRAPPED_ITERS, first_iter, lib.stackargs_last_bad()))
    end
end

-- Affected LuaJIT versions raise "NYI: cannot call this C function"
-- for some of these shapes (issue #888) -- report that as a finding,
-- not a crash.
for _, s in ipairs({ { "capture_i32", shape_i32 },
                     { "capture_mesh_shape", shape_mesh },
                     { "capture_u8", shape_u8 },
                     { "mesh_exact", shape_exact },
                     { "wrapped10", shape_wrapped } }) do
    local ok, err = pcall(s[2])
    if not ok then
        failures = failures + 1
        print(("ERROR %s: %s"):format(s[1], tostring(err)))
    end
end

if failures == 0 then
    print("PASS: all shapes, all args, " .. ITERS .. " iters each")
    os.exit(0)
else
    print("FAILURES: " .. failures)
    os.exit(1)
end
