use crate::common::{emit_module, function_body, run_luajit};

fn assert_bounds_check(lua: &str, name: &str, expected: bool) {
    let body = function_body(lua, name);
    assert_eq!(body.contains("__nb_checked"), expected, "{name}:\n{body}");
}

fn run_bounds_error(lua: &str, probe: &str) {
    let program = format!(
        r#"{lua}
local function expect_bounds_error(f, ...)
    local ok, err = pcall(f, ...)
    assert(not ok and tostring(err):find('out of bounds'))
end
{probe}
print('ok')
"#
    );
    assert_eq!(run_luajit(&program), "ok");
}

#[test]
fn constructor_lengths_bound_integer_byte_and_aliased_buffer_fills() {
    for (setup, length) in [
        ("let buf = native_buffer_int(count)", "buf.len"),
        ("let buf = native_buffer_u8(count)", "buf.len"),
        (
            "let original = native_buffer_int(count); let buf = original",
            "original.len",
        ),
    ] {
        let lua = emit_module(&format!(
            r#"
@nomangle fn fill(count: int) -> int {{
    {setup}
    for i in 0..count {{ buf[i] = i }}
    return {length}
}}
"#
        ));
        assert_bounds_check(&lua, "fill", false);
        assert_eq!(
            run_luajit(&format!(
                "{lua}\nassert(fill(0) == 0)\nassert(fill(17) == 17)\nprint('ok')"
            )),
            "ok"
        );
    }
}

#[test]
fn constructor_bounds_do_not_survive_mutation_or_foreign_calls() {
    for setup in [
        "let alias = buf; shrink(alias)",
        "shrink(buf)",
        "count += 1",
        "if count > 0 { buf = native_buffer_int(0) }",
    ] {
        let lua = emit_module(&format!(
            r#"
inline(lua) fn shrink(buf: NativeBuffer<int>) = "buf.len = 0"
@nomangle fn changed(count: int) -> int {{
    let buf = native_buffer_int(count)
    {setup}
    for i in 0..count {{ buf[i] = i }}
    return 1
}}
"#
        ));
        assert_bounds_check(&lua, "changed", true);
        run_bounds_error(&lua, "expect_bounds_error(changed, 4)");
    }
}

#[test]
fn a_recomputed_capacity_does_not_follow_mutated_inputs() {
    let lua = emit_module(
        r#"
struct Size { width: int }
inline(lua) fn grow(size: Size) = "size.width = size.width + 1"
@nomangle fn changed_size(size: Size) -> int {
    let buf = native_buffer_int(size.width)
    grow(size)
    for i in 0..size.width { buf[i] = i }
    return 1
}
"#,
    );
    assert_bounds_check(&lua, "changed_size", true);
    run_bounds_error(&lua, "expect_bounds_error(changed_size, {width=4})");
}

#[test]
fn reexecuted_constructor_does_not_resize_an_old_alias() {
    let lua = emit_module(
        r#"
@nomangle fn old_buffer() -> int {
    let buf = native_buffer_int(1)
    for count in 1..3 {
        let old = buf
        buf = native_buffer_int(count)
        for i in 0..count { old[i] = i }
    }
    return 1
}
"#,
    );
    assert_bounds_check(&lua, "old_buffer", true);
    run_bounds_error(&lua, "expect_bounds_error(old_buffer)");
}

#[test]
fn repeated_product_uses_unchanged_operand_snapshots() {
    let lua = emit_module(
        r#"
@nomangle fn fill_product(width: int, height: int) -> NativeBufferU8 {
    let buf = native_buffer_u8(width * height)
    for i in 0..width * height { buf[i] = i % 256 }
    return buf
}
"#,
    );
    assert_bounds_check(&lua, "fill_product", false);
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
for _, dims in ipairs({{{{0, 3}}, {{1, 1}}, {{1.5, 2}}, {{7, 11}}, {{64, 64}}}}) do
    local buf = fill_product(dims[1], dims[2])
    assert(buf.len == dims[1] * dims[2])
    for i = 0, buf.len - 1 do assert(buf.ptr[i] == i % 256) end
end
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}

#[test]
fn repeated_products_do_not_follow_changed_operands_or_callbacks() {
    for setup in [
        "width += 1",
        "height += 1",
        "if width > 0 { width += 1 }",
        "shrink(buf)",
    ] {
        let lua = emit_module(&format!(
            r#"
inline(lua) fn shrink(buf: NativeBuffer<int>) = "buf.len = 0"
@nomangle fn changed_product(width: int, height: int) -> int {{
    let buf = native_buffer_int(width * height)
    {setup}
    for i in 0..width * height {{ buf[i] = i }}
    return 1
}}
"#
        ));
        assert_bounds_check(&lua, "changed_product", true);
        run_bounds_error(&lua, "expect_bounds_error(changed_product, 2, 3)");
    }
}

#[test]
fn repeated_products_do_not_rebind_old_buffers_in_a_loop() {
    let lua = emit_module(
        r#"
@nomangle fn old_product(height: int) -> int {
    let buf = native_buffer_int(height)
    for width in 1..3 {
        let old = buf
        buf = native_buffer_int(width * height)
        for i in 0..width * height { old[i] = i }
    }
    return 1
}
"#,
    );
    assert_bounds_check(&lua, "old_product", true);
    run_bounds_error(&lua, "expect_bounds_error(old_product, 3)");
}

#[test]
fn repeated_record_reads_keep_callback_bounds_checks() {
    let lua = emit_module(
        r#"
struct Mask { width: int, pixels: NativeBufferU8 }
@nomangle fn copy_mask(mask: Mask, height: int) -> NativeBufferU8 {
    let buf = native_buffer_u8(mask.width * height)
    for i in 0..mask.width * height { buf[i] = mask.pixels[i] }
    return buf
}
"#,
    );
    assert!(function_body(&lua, "copy_mask").contains("__nb_checked_set"));
    run_bounds_error(
        &lua,
        r#"
local reads = 0
local mask = setmetatable({pixels = {ptr=ffi.new('uint8_t[24]'), len=24}}, {
    __index = function(_, key)
        assert(key == 'width')
        reads = reads + 1
        return reads == 1 and 2 or 3
    end
})
expect_bounds_error(copy_mask, mask, 3)
assert(reads >= 2)
"#,
    );
}

#[test]
fn reused_or_allocated_buffer_retains_capacity_at_join() {
    let lua = emit_module(
        r#"
@nomangle fn reuse_buffer(old: NativeBuffer<int>, count: int) -> NativeBuffer<int> {
    let buf = if old.len == count { old } else { native_buffer_int(count) }
    for i in 0..count { buf[i] = i }
    return buf
}
"#,
    );
    assert_bounds_check(&lua, "reuse_buffer", false);
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
for _, size in ipairs({{-1, 0, 3, 7, math.huge, 0/0}}) do
    local old = {{ptr=ffi.new('int[7]'), len=size}}
    local buf = reuse_buffer(old, 7)
    assert(buf.len == 7)
    assert((buf.ptr == old.ptr) == (size == 7))
    for i=0,6 do assert(buf.ptr[i] == i) end
end
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}

#[test]
fn capacity_equality_handles_operand_order_and_negative_spelling() {
    for selection in [
        "if count == old.len { old } else { native_buffer_int(count) }",
        "if old.len != count { native_buffer_int(count) } else { old }",
        "if !(old.len == count) { native_buffer_int(count) } else { old }",
    ] {
        let lua = emit_module(&format!(
            r#"
@nomangle fn reuse(old: NativeBuffer<int>, count: int) -> NativeBuffer<int> {{
    let buf = {selection}
    for i in 0..count {{ buf[i] = i }}
    return buf
}}
"#
        ));
        assert_bounds_check(&lua, "reuse", false);
    }
}

#[test]
fn unequal_capacities_and_changed_joins_retain_checks() {
    for (selection, after) in [
        (
            "if old.len != count { old } else { native_buffer_int(count) }",
            "",
        ),
        (
            "if old.len == count { old } else { native_buffer_int(count - 1) }",
            "",
        ),
        (
            "if old.len == count { old } else { native_buffer_int(count) }",
            "shrink(buf)",
        ),
        (
            "if old.len == count { old } else { native_buffer_int(count) }",
            "count += 1",
        ),
    ] {
        let lua = emit_module(&format!(
            r#"
inline(lua) fn shrink(buf: NativeBuffer<int>) = "buf.len = 0"
@nomangle fn changed_reuse(old: NativeBuffer<int>, count: int) -> int {{
    let buf = {selection}
    {after}
    for i in 0..count {{ buf[i] = i }}
    return 1
}}
"#
        ));
        assert_bounds_check(&lua, "changed_reuse", true);
        run_bounds_error(
            &lua,
            r#"
local old = {ptr=ffi.new('int[16]'), len=3}
expect_bounds_error(changed_reuse, old, 7)
"#,
        );
    }
}

#[test]
fn equal_capacity_join_composes_with_stride_and_known_helper_effects() {
    let lua = emit_module(
        r#"
global reuse_fetches: int = 0
fn record_fetch() { reuse_fetches += 1 }
@nomangle fn reuse_rows(old: NativeBuffer<float>, count: float) -> NativeBuffer<float> {
    let rows = count as int
    if rows >= 0 && rows <= 100000 {
        let expected = rows * 10
        let buf = if old.len == expected { old } else { native_buffer_float(expected) }
        for row in 0..rows {
            record_fetch()
            for lane in 0..10 { buf[row * 10 + lane] = (row + lane) as float }
        }
        return buf
    }
    return old
}
"#,
    );
    assert_bounds_check(&lua, "reuse_rows", false);
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
for _, size in ipairs({{0, 20, 30}}) do
    local old = {{ptr=ffi.new('float[30]'), len=size}}
    local buf = reuse_rows(old, 3)
    assert(buf.len == 30 and ((buf.ptr == old.ptr) == (size == 30)))
    for row=0,2 do for lane=0,9 do assert(buf.ptr[row*10+lane] == row+lane) end end
end
local old = {{ptr=ffi.new('float[1]'), len=1}}
for _, count in ipairs({{-1, 100001, math.huge, -math.huge, 0/0}}) do
    assert(reuse_rows(old, count).ptr == old.ptr)
end
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}

#[test]
fn equality_to_a_stale_length_does_not_restore_capacity() {
    let lua = emit_module(
        r#"
inline(lua) fn shrink(buf: NativeBuffer<int>) = "buf.len = 0"
@nomangle fn stale_reuse(old: NativeBuffer<int>, count: int) -> int {
    let previous = old.len
    shrink(old)
    let buf = if previous == count { old } else { native_buffer_int(count) }
    for i in 0..count { buf[i] = i }
    return 1
}
"#,
    );
    assert_bounds_check(&lua, "stale_reuse", true);
    run_bounds_error(
        &lua,
        r#"
local old = {ptr=ffi.new('int[7]'), len=7}
expect_bounds_error(stale_reuse, old, 7)
"#,
    );
}
