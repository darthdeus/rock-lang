use crate::common::{emit_module, function_body, run_luajit};

#[test]
fn a_local_record_read_preserves_loop_bounds_until_publication() {
    let lua = emit_module(
        r#"
struct Report { offset: int }
@nomangle fn fill_report(xs: [int], offset: int) -> Report {
    let report = Report { offset }
    for i in 0..xs.len() { xs[i] = i + report.offset }
    return report
}
"#,
    );
    let body = function_body(&lua, "fill_report");
    assert!(!body.contains("__vec_checked_set"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local xs = {{[0] = 3, 0,0,0}}
local report = fill_report(xs, 7)
assert(report.offset == 7)
assert(xs[1] == 7 and xs[2] == 8 and xs[3] == 9)
print('ok')
"#
        )),
        "ok"
    );
}
