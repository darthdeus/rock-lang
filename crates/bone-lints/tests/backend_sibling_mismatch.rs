//! backend-sibling-mismatch lint over bone HIR. Same harness as unused_fn:
//! `semantic_pass(&mut [parse(src)])` -> `Workspace`.

use bone_lints::{BackendSiblingIssue, BackendSiblingMismatch, backend_sibling_mismatches};
use bone_semantic::prelude::*;

fn parse_named(src: &str, name: &str) -> skull_parser::Ast {
    skull_parser::parse_module_clean(src, Ustr::from(name)).expect("parse")
}

fn findings(srcs: &[&str]) -> Vec<BackendSiblingMismatch> {
    let mut asts: Vec<skull_parser::Ast> = srcs
        .iter()
        .enumerate()
        .map(|(i, s)| parse_named(s, &format!("<sib{i}>")))
        .collect();
    let ws = semantic_pass(&mut asts).expect("clean program typechecks");
    let mut out = backend_sibling_mismatches(&ws);
    out.sort_by(|a, b| {
        (a.name, &a.signature, a.backend, &a.issue).cmp(&(
            b.name,
            &b.signature,
            b.backend,
            &b.issue,
        ))
    });
    out
}

fn summary(srcs: &[&str]) -> Vec<String> {
    findings(srcs)
        .iter()
        .map(|f| match &f.issue {
            BackendSiblingIssue::MissingOn { backend } => {
                format!(
                    "{}{} on {} missing on {backend}",
                    f.name, f.signature, f.backend
                )
            }
            BackendSiblingIssue::ReturnDiffers {
                other_backend,
                other_ret,
            } => format!(
                "{}{} on {} returns differ vs {other_backend} ({other_ret})",
                f.name, f.signature, f.backend
            ),
        })
        .collect()
}

#[test]
fn flags_a_sibling_pair_whose_param_types_drifted() {
    // The canonical bug: `field_get`'s js sibling kept the pre-split `FieldRef`
    // param after the lua decl moved to `ReadFieldRef`. Each side is unmatched on
    // the other backend, so both decls are reported.
    let src = "inline(lua) fn probe(x: int) -> int = \"return x\"\n\
               inline(js) fn probe(x: float) -> int = \"return x;\"";
    assert_eq!(
        summary(&[src]),
        vec![
            "probe(float) on js missing on lua".to_string(),
            "probe(int) on lua missing on js".to_string(),
        ]
    );
}

#[test]
fn consistent_siblings_are_not_flagged() {
    let cases: &[&str] = &[
        // identical signatures are a clean sibling pair
        "inline(lua) fn probe(x: int) -> int = \"return x\"\n\
               inline(js) fn probe(x: int) -> int = \"return x;\"",
        // Platform-exclusive by construction: one backend declares it, nobody
        // claimed a sibling. Flagging these would fire on every lua-only ffi fn.
        "inline(lua) fn only_native(x: int) -> int = \"return x\"",
        // `probe(float)` is served on BOTH backends by the portable Rock body, so
        // the lua-only `probe(int)` overload is the only concrete decl -- and with
        // one concrete backend per signature there is nothing to drift. Missing
        // this rung false-flags every wrapper-over-raw-extern pair in a host library.
        "inline(lua) fn probe(x: int) -> int = \"return x\"\n\
               inline(js) fn probe(x: int) -> int = \"return x;\"\n\
               fn probe(x: float) -> int {\n    return 0\n}",
        // every signature has both backends
        "inline(lua) fn probe(x: int) -> int = \"return x\"\n\
               inline(lua) fn probe(x: float) -> int = \"return x\"\n\
               inline(js) fn probe(x: int) -> int = \"return x;\"\n\
               inline(js) fn probe(x: float) -> int = \"return x;\"",
        // `os_getenv` renames its param on the js side; only types are the ABI.
        "inline(lua) fn probe(var: string) -> string = \"return var\"\n\
               inline(js) fn probe(v: string) -> string = \"return v;\"",
    ];
    for src in cases {
        assert!(summary(&[src]).is_empty(), "{src}");
    }
}

#[test]
fn flags_an_arity_drift_across_files() {
    // The `host_create_window_ex` shape: the js half never grew the last
    // param. Split across modules, since that is how siblings actually live
    // (stdlib/engine decl vs `js_ffi.rock`).
    let lua = "inline(lua) fn make_window(title: string, full: bool) -> int = \"return 1\"";
    let js = "inline(js) fn make_window(title: string) -> int = \"return 1;\"";
    assert_eq!(
        summary(&[lua, js]),
        vec![
            "make_window(string) on js missing on lua".to_string(),
            "make_window(string, bool) on lua missing on js".to_string(),
        ]
    );
}

#[test]
fn return_type_drift_is_flagged() {
    // Overload resolution keys on params, so a return-type difference does not
    // make two overloads -- it makes two decls that disagree about the same
    // call. Real drift, and the collapse in `select_overload` would pick one
    // arbitrarily.
    let src = "inline(lua) fn probe(x: int) -> int = \"return x\"\n\
               inline(js) fn probe(x: int) -> float = \"return x;\"";
    assert_eq!(
        summary(&[src]),
        vec![
            "probe(int) on js returns differ vs lua (int)".to_string(),
            "probe(int) on lua returns differ vs js (float)".to_string(),
        ]
    );
}

#[test]
fn the_finding_points_at_the_unmatched_decl() {
    let lua = "inline(lua) fn make_window(title: string, full: bool) -> int = \"return 1\"";
    let js = "\n\ninline(js) fn make_window(title: string) -> int = \"return 1;\"";
    let fs = findings(&[lua, js]);
    // `line_range` is 0-based, the unit `filter_suppressed` matches against.
    let js_side = fs
        .iter()
        .find(|f| f.backend == "js")
        .expect("js-side finding");
    assert_eq!(
        js_side.span.line_range.0, 2,
        "reports on the js decl's own line"
    );
    let lua_side = fs
        .iter()
        .find(|f| f.backend == "lua")
        .expect("lua-side finding");
    assert_eq!(lua_side.span.line_range.0, 0);
}
