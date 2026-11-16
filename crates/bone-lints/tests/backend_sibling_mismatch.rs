//! backend-sibling-mismatch lint over bone HIR, run through `lint_workspace`.

use crate::support::lint_only;
use bone_lints::Finding;

/// Each source is its own module, `<sib0>`, `<sib1>`, ...
fn findings(srcs: &[&str]) -> Vec<Finding> {
    let names: Vec<String> = (0..srcs.len()).map(|i| format!("<sib{i}>")).collect();
    let files: Vec<(&str, &str)> = names
        .iter()
        .zip(srcs)
        .map(|(name, src)| (name.as_str(), *src))
        .collect();
    lint_only(&files, bone_lints::BACKEND_SIBLING_MISMATCH)
}

/// The backtick-quoted words (a repeat dropped) of each message, space-joined and sorted:
/// `probe(int) lua js` for a lua decl missing on js, `probe(int) lua int` for a
/// return differing from the lua sibling's `int`.
fn summary(srcs: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = findings(srcs)
        .iter()
        .map(|f| {
            let mut words: Vec<&str> = f.error.message.split('`').skip(1).step_by(2).collect();
            words.dedup();
            words.join(" ")
        })
        .collect();
    out.sort();
    out
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
        ["probe(float) js lua", "probe(int) lua js"]
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
        [
            "make_window(string) js lua",
            "make_window(string, bool) lua js"
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
        ["probe(int) js float", "probe(int) lua int"]
    );
}

#[test]
fn the_finding_points_at_the_unmatched_decl() {
    let lua = "inline(lua) fn make_window(title: string, full: bool) -> int = \"return 1\"";
    let js = "\n\ninline(js) fn make_window(title: string) -> int = \"return 1;\"";
    let fs = findings(&[lua, js]);
    // `line` is 0-based, the unit suppression matches against.
    let declared_for = |backend: &str| {
        fs.iter()
            .find(|f| {
                f.error
                    .message
                    .contains(&format!("declared for `{backend}`"))
            })
            .unwrap_or_else(|| panic!("{backend}-side finding: {fs:?}"))
    };
    let js_side = declared_for("js");
    assert_eq!(js_side.file().as_str(), "<sib1>");
    assert_eq!(js_side.line, 2, "reports on the js decl's own line");
    let lua_side = declared_for("lua");
    assert_eq!(lua_side.file().as_str(), "<sib0>");
    assert_eq!(lua_side.line, 0);
}
