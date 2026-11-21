use crate::common::{emit_module, function_body, run_luajit};

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
