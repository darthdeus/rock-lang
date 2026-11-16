// FFI stack-argument repro for the LuaJIT arm64 trace argument corruption
// Each fn takes enough args to spill past the 8 GPRs and
// writes what it RECEIVED into out-params, so the Lua side can name the
// exact argument that arrived wrong.
#include <stdint.h>

#define EXPORT __attribute__((visibility("default")))

// Shape 1: out ptr + 12 x int32. x0 = out, x1..x7 = a1..a7,
// stack = a8..a12 (five int32 slots -- packed to 4 bytes on Apple).
EXPORT void capture_i32(int32_t *out,
    int32_t a1, int32_t a2, int32_t a3, int32_t a4, int32_t a5,
    int32_t a6, int32_t a7, int32_t a8, int32_t a9, int32_t a10,
    int32_t a11, int32_t a12) {
    out[0] = a1; out[1] = a2; out[2] = a3; out[3] = a4;
    out[4] = a5; out[5] = a6; out[6] = a7; out[7] = a8;
    out[8] = a9; out[9] = a10; out[10] = a11; out[11] = a12;
}

// Shape 2: the original stone_draw_mesh signature:
// ptr + 10 x int32 + u64 tail. Two out ptrs eat x0/x1, so
// stack = o6..o10 (int32) then tex (u64, natural-aligned on Apple:
// 20 bytes of ints, tex at offset 24 -- the alignment-sensitive case).
EXPORT void capture_mesh_shape(int32_t *out, uint64_t *out_tex,
    const float *data,
    int32_t count, int32_t stride, int32_t o3, int32_t o4, int32_t o5,
    int32_t o6, int32_t o7, int32_t o8, int32_t o9, int32_t o10,
    uint64_t tex) {
    out[0] = count; out[1] = stride; out[2] = o3; out[3] = o4;
    out[4] = o5; out[5] = o6; out[6] = o7; out[7] = o8;
    out[8] = o9; out[9] = o10;
    out[10] = (int32_t)data[0];
    *out_tex = tex;
}

// Shape 3: LuaJIT issue #888 shape -- out ptr + 10 x uint8_t.
// stack = b8..b10, byte-packed on Apple.
EXPORT void capture_u8(uint8_t *out,
    uint8_t b1, uint8_t b2, uint8_t b3, uint8_t b4, uint8_t b5,
    uint8_t b6, uint8_t b7, uint8_t b8, uint8_t b9, uint8_t b10) {
    out[0] = b1; out[1] = b2; out[2] = b3; out[3] = b4; out[4] = b5;
    out[5] = b6; out[6] = b7; out[7] = b8; out[8] = b9; out[9] = b10;
}

// Shape 4: EXACT stone_draw_mesh signature -- same
// register/stack split as the incident (x0 = data, x1..x7 = first 7
// ints, stack = 3 x int32 + u64 tail). Weighted checksum so any
// garbled arg changes the result; weights are powers of 3 so no two
// arg-swaps collide.
EXPORT uint64_t mesh_exact(const float *data,
    int32_t count, int32_t stride, int32_t pos_offset, int32_t uv_offset,
    int32_t color_offset, int32_t pivot_y_offset, int32_t effects_offset,
    int32_t outline_size_offset, int32_t outline_color_offset,
    int32_t entity_id_offset, uint64_t tex) {
    uint64_t sum = (uint64_t)(int64_t)count * 3
        + (uint64_t)(int64_t)stride * 9
        + (uint64_t)(int64_t)pos_offset * 27
        + (uint64_t)(int64_t)uv_offset * 81
        + (uint64_t)(int64_t)color_offset * 243
        + (uint64_t)(int64_t)pivot_y_offset * 729
        + (uint64_t)(int64_t)effects_offset * 2187
        + (uint64_t)(int64_t)outline_size_offset * 6561
        + (uint64_t)(int64_t)outline_color_offset * 19683
        + (uint64_t)(int64_t)entity_id_offset * 59049
        + (uint64_t)(int32_t)data[0] * 177147;
    return sum + tex;
}

// Shape 5: the shape Rock's `inline(lua)` FFI shims actually generate --
// 8 register args, then a stack run of ptr + 4 x int32 + 2 x ptr, called
// through a Lua wrapper whose result is consumed in the same expression
// (`return ns.fn(...) ~= 0`). `wrapped10` truncates at the first stack
// int32, which is the earliest argument that corrupts and the only one
// that does at that width, so it detects without dereferencing a garbled
// pointer. LAST_BAD names the argument for the driver.
static int LAST_BAD = 0;
EXPORT int stackargs_last_bad(void) { return LAST_BAD; }

EXPORT int wrapped10(const char *shader_key, const unsigned char *vertices,
    uint32_t vertex_bytes, int32_t first, int32_t count, int32_t stride,
    const char *layout_wire, const char *payload_digest,
    const char *binding_wire, int32_t blend) {
    int bad = 0;
    if (shader_key[0] != 'S')     bad |= 1 << 0;
    if (vertex_bytes != 4096)     bad |= 1 << 2;
    if (first != 0)               bad |= 1 << 3;
    if (count != 36)              bad |= 1 << 4;
    if (stride != 44)             bad |= 1 << 5;
    if (layout_wire[0] != 'L')    bad |= 1 << 6;
    if (payload_digest[0] != 'P') bad |= 1 << 7;
    if (binding_wire[0] != 'B')   bad |= 1 << 8;
    if (blend != 3)               bad |= 1 << 9;
    if (bad) LAST_BAD = bad;
    return bad;
}

// The full production shape. The corrupted slot lands in a pointer here,
// so the callee faults on dereference -- run it in its own process.
EXPORT int wrapped15(const char *shader_key, const unsigned char *vertices,
    uint32_t vertex_bytes, int32_t first, int32_t count, int32_t stride,
    const char *layout_wire, const char *payload_digest,
    const char *binding_wire, int32_t blend, int32_t depth,
    uint32_t color_write, uint32_t instances,
    const char *uniform_values, const char *array_values) {
    int bad = 0;
    if (blend != 3)             bad |= 1 << 9;
    if (depth != 0)             bad |= 1 << 10;
    if (color_write != 15)      bad |= 1 << 11;
    if (instances != 1)         bad |= 1 << 12;
    if (uniform_values[0] != 'U') bad |= 1 << 13;
    if (array_values[0] != 'A')   bad |= 1 << 14;
    if (bad) LAST_BAD = bad;
    return bad;
}
