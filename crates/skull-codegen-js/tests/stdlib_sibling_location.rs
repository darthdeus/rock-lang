//! The lua/js SIBLING CONTRACT: one function, two bodies, in two files, with
//! nothing in the compiler binding them together. This file holds every check
//! that they still agree -- where the JS body lives and its arity. JS reports
//! neither a surplus argument nor a missing one, so every drift in this class
//! is silent by construction.
//!
//! A stdlib-owned fn keeps its JS body in `stdlib/js.rock`, which is loaded
//! last: an `inline(js)` fn added to a lua-emitting file shifts every
//! `__SITE__` below it and breaks Lua byte-identity.

use skull_parser::ast::{FunctionBody, ItemKind};
use ustr::Ustr;

/// The home this rule points at has to exist and be loaded.
#[test]
fn the_stdlib_js_sibling_file_is_loaded() {
    let names: Vec<&str> = rock_stdlib::stdlib_files().iter().map(|f| f.name).collect();
    assert!(
        names.contains(&"js.rock"),
        "stdlib/js.rock is not in the load list: {names:?}"
    );
    assert_eq!(
        names.last(),
        Some(&"js.rock"),
        "js.rock must load LAST: an earlier slot would shift `__SITE__` lines in the files \
         after it, which is the whole reason the JS siblings are not co-located"
    );
}

fn sibling_facts(
    src: &str,
    id: &str,
    want_js: bool,
) -> std::collections::HashMap<String, (usize, std::collections::BTreeSet<String>)> {
    let ast = skull_parser::parse_module_clean(src, Ustr::from(id)).expect("parse");
    assert!(ast.recovery_errors.is_empty(), "{id} must parse cleanly");
    let mut out = std::collections::HashMap::new();
    for item in &ast.items {
        let ItemKind::Function(f) = &item.kind else {
            continue;
        };
        let is_js = matches!(
            f.body,
            FunctionBody::ExternJs | FunctionBody::Inline(bone_core::hir::FnBackend::Js, _)
        );
        let is_lua = matches!(
            f.body,
            FunctionBody::Inline(bone_core::hir::FnBackend::Lua, _)
        );
        if is_js != want_js || (!is_js && !is_lua) {
            continue;
        }
        let anns: std::collections::BTreeSet<String> = item
            .annotations
            .iter()
            .map(|a| a.name.name.to_string())
            // `@inline` is an optimization annotation, not a boundary or ABI
            // discriminator. A sibling carrying it and one not carrying it
            // must therefore behave identically here.
            // `@allow` is a lint suppression -- it scopes a diagnostic over the
            // body it sits on, so it is expected to differ between two bodies
            // written in different languages (`long_inline_lua` cannot apply to
            // a JS body) and says nothing about how a caller emits the call.
            .filter(|n| n != "inline" && n != "allow")
            .collect();
        out.insert(f.name.name.to_string(), (f.params.len(), anns));
    }
    out
}

#[test]
fn a_sibling_pair_agrees_on_arity_and_annotations() {
    let mut stdlib_lua = std::collections::HashMap::new();
    let mut stdlib_js = std::collections::HashMap::new();
    for f in rock_stdlib::stdlib_files() {
        stdlib_lua.extend(sibling_facts(f.contents, f.name, false));
        stdlib_js.extend(sibling_facts(f.contents, f.name, true));
    }
    let paired: Vec<&String> = stdlib_js
        .keys()
        .filter(|name| stdlib_lua.contains_key(*name))
        .collect();
    assert!(
        paired.len() >= 10,
        "only {} sibling pairs found -- the scan is broken, not the source",
        paired.len()
    );
    let mut drifted: Vec<String> = Vec::new();
    for name in paired {
        let (lua_arity, lua_annotations) = &stdlib_lua[name];
        let (js_arity, js_annotations) = &stdlib_js[name];
        if lua_arity != js_arity {
            drifted.push(format!("{name} arity lua={lua_arity} js={js_arity}"));
        }
        if lua_annotations != js_annotations {
            let lua_only: Vec<&String> = lua_annotations.difference(js_annotations).collect();
            let js_only: Vec<&String> = js_annotations.difference(lua_annotations).collect();
            drifted.push(format!("{name} lua-only={lua_only:?} js-only={js_only:?}"));
        }
    }
    assert!(
        drifted.is_empty(),
        "these lua/js siblings disagree, so the JS body binds its params or takes its \
         ABI differently from its luajit twin: {drifted:#?}"
    );
}
