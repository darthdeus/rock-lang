-- The production shape (15 args) in its own process: the corrupted slot
-- lands in a pointer, so the callee faults on dereference and this script
-- dies with SIGSEGV instead of reporting. Exit 0 means the shape survived.
-- Usage: luajit [-joff] crash.lua <libpath>
local ffi = require("ffi")
ffi.cdef[[
int wrapped15(const char *shader_key, const unsigned char *vertices,
    uint32_t vertex_bytes, int32_t first, int32_t count, int32_t stride,
    const char *layout_wire, const char *payload_digest,
    const char *binding_wire, int32_t blend, int32_t depth,
    uint32_t color_write, uint32_t instances,
    const char *uniform_values, const char *array_values);
]]
local lib = ffi.load(arg[1] or "./libstackargs.so")
local pre = ffi.cast("const unsigned char*", ffi.new("float[1024]"))

local function shim(p0, p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14)
    return lib.wrapped15(p0, p1, p2, p3, p4, p5, p6, p7, p8, p9,
                         p10, p11, p12, p13, p14) ~= 0
end

local hits = 0
local a, b, c, d, e, f, g, h = 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5
for i = 1, 200000 do
    local uni = "U:" .. i
    local arr = "A:" .. (i * 3)
    local pay = "P:" .. (i % 97)
    a = a + i * 0.5; b = b - a * 0.25; c = c + b; d = d * 1.0001
    e = e + c * 0.5; f = f - d; g = g + e * 0.125; h = h + f
    if shim("SHADER", pre, 4096, 0, 36, 44, "LAYOUT", pay, "BINDING",
            3, 0, 15, 1, uni, arr) then hits = hits + 1 end
end
if a + b + c + d + e + f + g + h == 0 then print("") end
print(("wrapped15: %d/200000 garbled, no fault"):format(hits))

if hits ~= 0 then os.exit(1) end
