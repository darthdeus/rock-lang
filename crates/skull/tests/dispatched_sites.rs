use crate::common::{bound_callee_name, emit_module, function_body, inline_off_guard, ws_of_files};

#[test]
fn dispatched_operations_emit_only_the_helpers_they_need() {
    let _inline = inline_off_guard();
    let lua = emit_module(
        r#"
struct P { x: int, y: int }
newtype Id(int)
nobox value V { a: float, b: float, c: int, s: string }
fn f_float(x: float) -> string { return "v={x}" }
fn f_int(x: int) -> string { return "v={x}" }
fn two(a: float, b: float) -> string { return "{a} and {b}" }
fn show_value(v: V) -> string { return "{v}" }
fn show_p(p: P) -> string { return "{p}" }
fn show_id(i: Id) -> string { return "{i}" }
fn spec_p(p: P) -> string { return "{p:.3}" }
fn spec_float(x: float) -> string { return "{x:.2f}" }
fn pct(x: float) -> string { return "{x:%}" }
fn pct_plain(x: float) -> string { return "{x:f}" }
fn bin(x: int) -> string { return "{x:b}" }
fn bin_plain(x: int) -> string { return "{x:x}" }
fn grouped(x: int) -> string { return "{x:,}" }
fn grouped_plain(x: int) -> string { return "{x:d}" }
fn pct_wide(x: float) -> string { return "{x:10%}" }
fn bin_wide(x: int) -> string { return "{x:10b}" }
fn grouped_wide(x: int) -> string { return "{x:10,}" }
fn eq_struct(a: P, b: P) -> bool { return a == b }
fn ne_struct(a: P, b: P) -> bool { return a != b }
fn eq_scalar(a: int, b: int) -> bool { return a == b }
fn eq_value(a: V, b: V) -> bool { return a == b }
fn main() {
    print(show_p(P { x: 1, y: 2 }))
    print(show_id(Id(3)))
}
"#,
    );
    let cases: &[(&str, &[(&str, usize)])] = &[
        ("f_float", &[("__rock_format_float", 1)]),
        ("f_int", &[]),
        ("two", &[("__rock_format_float", 2)]),
        (
            "show_value",
            &[("__rock_format_float", 2), ("__rock_quote_string", 1)],
        ),
        ("show_p", &[("inspect", 1)]),
        ("show_id", &[]),
        ("spec_p", &[("inspect_formatted", 1), ("__rock_fmt_pad", 1)]),
        ("spec_float", &[]),
        ("pct", &[("__rock_fmt_percent", 1)]),
        ("pct_plain", &[]),
        ("bin", &[("__rock_fmt_binary", 1)]),
        ("bin_plain", &[]),
        ("grouped", &[("__rock_fmt_grouping", 1)]),
        ("grouped_plain", &[]),
        (
            "pct_wide",
            &[("__rock_fmt_percent", 1), ("__rock_fmt_pad", 1)],
        ),
        (
            "bin_wide",
            &[("__rock_fmt_binary", 1), ("__rock_fmt_pad", 1)],
        ),
        (
            "grouped_wide",
            &[("__rock_fmt_grouping", 1), ("__rock_fmt_pad", 1)],
        ),
        ("eq_struct", &[("__op_eq", 1)]),
        ("ne_struct", &[("__op_ne", 1)]),
        ("eq_scalar", &[]),
        ("eq_value", &[]),
    ];
    let helpers = [
        "__rock_format_float",
        "__rock_quote_string",
        "inspect",
        "inspect_formatted",
        "__rock_fmt_percent",
        "__rock_fmt_binary",
        "__rock_fmt_grouping",
        "__rock_fmt_pad",
        "__op_eq",
        "__op_ne",
    ];
    for &(caller, expected) in cases {
        let body = function_body(&lua, caller);
        for helper in helpers {
            let callee = bound_callee_name(&lua, caller, helper);
            let count = body.matches(&format!("{callee}(")).count();
            let want = expected
                .iter()
                .find(|(name, _)| *name == helper)
                .map_or(0, |(_, n)| *n);
            assert_eq!(count, want);
        }
    }
}

#[test]
fn an_ambiguous_dispatched_helper_rejects_emission() {
    let ws = ws_of_files(&[
        (
            "dependency.rock",
            r#"@nomangle inline(lua) fn inspect_formatted(obj: any, precision: float) -> string = "return 'impostor'""#,
        ),
        (
            "<test>",
            "struct P { x: int }\nfn main() { print(\"{P { x: 1 }:.2}\") }",
        ),
    ]);
    let mid = crate::common::module_id(&ws, "<test>");
    let error = match skull::mir_emit::emit_full(&ws, mid) {
        Ok(_) => panic!("an ambiguous helper emitted successfully"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("compiler-generated-entry-unresolved")
    );
}
