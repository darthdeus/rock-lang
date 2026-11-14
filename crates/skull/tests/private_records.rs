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
    assert!(!body.contains("__vec_checked_set"), "{body}");
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local xs = {{_data={{0,0,0}},len=3}}
local report = fill_report(xs, 7)
assert(report.offset == 7)
assert(xs._data[1] == 7 and xs._data[2] == 8 and xs._data[3] == 9)
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}

#[test]
fn published_record_aliases_keep_getter_bounds_checks() {
    for setup in [
        "arm(report, xs)",
        "let alias = report; arm(alias, xs)",
        "let holder = Holder { report }; arm(holder.report, xs)",
        "let reports = [report]; arm(reports[0], xs)",
        "let reports: [Report] = []; reports.push(report); arm(reports[0], xs)",
        "saved_report = report; arm(saved_report, xs)",
        "let expose = () => arm(report, xs); expose()",
    ] {
        let lua = emit_module(&format!(
            r#"
struct Report {{ offset: int }}
struct Holder {{ report: Report }}
global saved_report: Report? = null
inline(lua) fn arm(report: Report, xs: [int]) = """
    rawset(report, 'offset', nil)
    setmetatable(report, {{__index = function(_, key)
        xs.len = 0
        return 7
    end}})
"""
@nomangle fn published(xs: [int]) -> Report {{
    let report = Report {{ offset: 7 }}
    {setup}
    for i in 0..xs.len() {{ xs[i] = i + report.offset }}
    return report
}}
"#
        ));
        assert!(
            function_body(&lua, "published").contains("__vec_checked_set"),
            "{setup}"
        );
        assert_eq!(
            run_luajit(&format!(
                r#"{lua}
local xs = {{_data={{0,0,0}},len=3}}
local ok, err = pcall(published, xs)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
            ))
            .trim(),
            "ok",
            "{setup}"
        );
    }
}

#[test]
fn new_record_at_same_site_does_not_make_an_escaped_alias_private() {
    let lua = emit_module(
        r#"
struct Report { offset: int }
inline(lua) fn arm(report: Report, xs: [int]) = """
    rawset(report, 'offset', nil)
    setmetatable(report, {__index = function() xs.len = 0; return 7 end})
"""
@nomangle fn old_report(xs: [int]) -> Report {
    let held = Report { offset: 0 }
    for pass in 0..2 {
        let report = Report { offset: 7 }
        if pass == 0 { held = report; arm(held, xs) }
        if pass == 1 {
            for i in 0..xs.len() { xs[i] = held.offset }
        }
    }
    return held
}
"#,
    );
    assert!(function_body(&lua, "old_report").contains("__vec_checked_set"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local xs = {{_data={{0,0,0}},len=3}}
local ok, err = pcall(old_report, xs)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        ))
        .trim(),
        "ok"
    );
}
