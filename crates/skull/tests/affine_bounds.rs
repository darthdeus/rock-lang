use crate::common::{emit_module, function_body, run_luajit};

#[test]
fn bounded_affine_native_buffer_loops_keep_boundary_behavior() {
    let lua = emit_module(include_str!("../../../benchmarks/guard_proofs.rock"));
    let body = function_body(&lua, "guard_stride_proven");
    assert!(!body.contains("__nb_checked_get"));
    assert!(function_body(&lua, "guard_stride").contains("__nb_checked_get"));
    let output = run_luajit(&format!(
        r#"{lua}
local data = ffi.new('int[16]')
for i = 0, 15 do data[i] = i + 1 end
local buf = {{ptr = data, len = 16}}
for _, count in ipairs({{-1, 0, 0.5, 1, 1.9, 4, 5, 100001, math.huge, -math.huge, 0/0}}) do
    local n = math.floor(count)
    local expected = -1
    if n >= 0 and n <= 100000 and buf.len >= n * 4 then
        expected = n * 4 * (n * 4 + 1) / 2
    end
    assert(guard_stride_proven(buf, count) == expected)
end
buf.len = 3
assert(guard_stride_proven(buf, 1) == -1)
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn affine_bounds_that_no_longer_hold_keep_the_check() {
    for (name, source, args) in [
        (
            "stale",
            r#"
@nomangle fn stale(buf: NativeBuffer<int>, count: float, r: float, l: float) -> int {
    let rows = count as int
    let capacity = rows * 4
    rows += 10
    let row = r as int
    let lane = l as int
    if rows >= 0 && rows <= 100000 && row >= 0 && row < rows
        && lane >= 0 && lane < 4 && buf.len >= capacity {
        return buf[row * 4 + lane]
    }
    return -1
}
"#,
            "1, 5, 0",
        ),
        (
            "changed_lane",
            r#"
@nomangle fn changed_lane(buf: NativeBuffer<int>, count: float) -> int {
    let rows = count as int
    let lane = 0
    let sum = 0
    if rows >= 0 && rows <= 100000 && lane >= 0 && lane < 4 && buf.len >= rows * 4 {
        for row in 0..rows {
            sum += buf[row * 4 + lane]
            lane += 8
        }
    }
    return sum
}
"#,
            "2",
        ),
    ] {
        let lua = emit_module(source);
        assert!(
            function_body(&lua, name).contains("__nb_checked_get"),
            "{name}"
        );
        let output = run_luajit(&format!(
            "{lua}\nlocal buf = {{ptr = ffi.new('int[64]'), len = 8}}\n\
             local ok, err = pcall({name}, buf, {args})\n\
             assert(not ok and tostring(err):find('out of bounds'))\nprint('ok')"
        ));
        assert_eq!(output, "ok", "{name}");
    }
}
