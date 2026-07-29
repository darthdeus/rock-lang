//! A stdlib function's JS body must live in the stdlib, not in the web engine.
//!
//! `stone/engine/web/js_ffi.rock` is staged only by `rock web`, so an
//! `inline(js)` body there is absent from every JS program that does not use
//! stone -- which is every `--backend js` test. `field_get`/`field_set` sat
//! there and cost 12 corpus tests that had a working implementation the
//! workspace could not see. The failure is silent in the direction that matters:
//! the web smokes stay green, so nothing points at the misplacement.
//!
//! Engine-owned fns keep their siblings in `js_ffi.rock` on purpose (an
//! `inline(js)` fn added to a lua-emitting file shifts every `__SITE__` below it
//! and breaks Lua byte-identity). The rule is about OWNERSHIP, not about the
//! file being off limits: a stdlib-owned fn belongs in `stdlib/js.rock`, which
//! is loaded last and so shifts no line either.

use std::collections::HashSet;

use skull_parser::ast::{FunctionBody, ItemKind};
use ustr::Ustr;

fn fn_names(path: &str, want_js: bool) -> HashSet<String> {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let ast = skull_parser::parse_module(&src, Ustr::from(path)).expect("parse");
    assert!(ast.recovery_errors.is_empty(), "{path} must parse cleanly");
    ast.items
        .iter()
        .filter_map(|item| {
            let ItemKind::Function(f) = &item.kind else { return None };
            let is_js = matches!(
                f.body,
                FunctionBody::ExternJs
                    | FunctionBody::Inline(bone_semantic::hir::FnBackend::Js, _)
            );
            (is_js == want_js).then(|| f.name.name.to_string())
        })
        .collect()
}

fn repo(rel: &str) -> String {
    format!("{}/../../{rel}", env!("CARGO_MANIFEST_DIR"))
}

/// Every `inline(js)`/`extern(js)` body in the web engine must implement an
/// ENGINE fn. One that shares a name with a stdlib fn is a stdlib body in the
/// wrong crate, and is invisible to every stone-less JS program.
#[test]
fn the_web_engine_holds_no_stdlib_js_bodies() {
    let js_ffi = fn_names(&repo("stone/engine/web/js_ffi.rock"), true);
    assert!(!js_ffi.is_empty(), "js_ffi.rock parsed to zero JS bodies -- the scan is vacuous");

    let mut stdlib: HashSet<String> = HashSet::new();
    for f in rock_host::stdlib_loader::stdlib_files() {
        let src = f.contents;
        let ast = skull_parser::parse_module(src, Ustr::from(f.name)).expect("stdlib parse");
        for item in &ast.items {
            if let ItemKind::Function(fun) = &item.kind {
                // A JS body in `stdlib/js.rock` is the RIGHT home, not a clash.
                let is_js = matches!(
                    fun.body,
                    FunctionBody::ExternJs
                        | FunctionBody::Inline(bone_semantic::hir::FnBackend::Js, _)
                );
                if !is_js {
                    stdlib.insert(fun.name.name.to_string());
                }
            }
        }
    }
    assert!(stdlib.contains("field_get"), "stdlib scan missed a known fn -- the scan is wrong");

    let misplaced: Vec<&String> = js_ffi.iter().filter(|n| stdlib.contains(*n)).collect();
    assert!(
        misplaced.is_empty(),
        "these stdlib fns have their JS body in the web engine, so no stone-less JS program \
         can bind them -- move them to `stdlib/js.rock`: {misplaced:?}"
    );
}

/// The home this rule points at has to exist and be loaded, or the advice in the
/// failure above is unfollowable.
#[test]
fn the_stdlib_js_sibling_file_is_loaded() {
    let names: Vec<&str> = rock_host::stdlib_loader::stdlib_files().iter().map(|f| f.name).collect();
    assert!(names.contains(&"js.rock"), "stdlib/js.rock is not in the load list: {names:?}");
    assert_eq!(
        names.last(),
        Some(&"js.rock"),
        "js.rock must load LAST: an earlier slot would shift `__SITE__` lines in the files \
         after it, which is the whole reason the JS siblings are not co-located"
    );
}
