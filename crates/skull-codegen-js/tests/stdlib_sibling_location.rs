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

/// A lua/js sibling pair is one function with two bodies, and nothing binds them
/// together: they sit in different files and the emitter resolves whichever
/// serves the target. JS then drops surplus arguments and passes `undefined` for
/// missing ones, so a param added to one side is silently positional garbage on
/// the other -- never a build error, never a runtime throw.
///
/// `__rock_col_insert` gained a `len` param on the lua side while the JS sibling
/// kept three, so every column insert spliced in the LENGTH instead of the value
/// and read back as a present element.
#[test]
fn a_stdlib_sibling_pair_agrees_on_arity() {
    use std::collections::HashMap;
    let mut lua: HashMap<String, usize> = HashMap::new();
    let mut js: HashMap<String, usize> = HashMap::new();
    for f in rock_stdlib::stdlib_files() {
        let ast =
            skull_parser::parse_module_clean(f.contents, Ustr::from(f.name)).expect("stdlib parse");
        for item in &ast.items {
            let ItemKind::Function(fun) = &item.kind else {
                continue;
            };
            // `@scalar_abi` params explode into leaves on the JS side, so the two
            // declared arities are legitimately incomparable.
            if item
                .annotations
                .iter()
                .any(|a| a.name.name.as_str() == "scalar_abi")
            {
                continue;
            }
            let name = fun.name.name.to_string();
            match fun.body {
                FunctionBody::Inline(bone_core::hir::FnBackend::Lua, _) => {
                    lua.insert(name, fun.params.len());
                }
                FunctionBody::ExternJs | FunctionBody::Inline(bone_core::hir::FnBackend::Js, _) => {
                    js.insert(name, fun.params.len());
                }
                _ => {}
            }
        }
    }
    let paired: Vec<&String> = js.keys().filter(|n| lua.contains_key(*n)).collect();
    assert!(
        paired.len() >= 10,
        "only {} lua/js sibling pairs found -- the scan is broken, not the stdlib",
        paired.len()
    );
    let drifted: Vec<String> = paired
        .iter()
        .filter(|n| lua[**n] != js[**n])
        .map(|n| format!("{n} (lua={}, js={})", lua[*n], js[*n]))
        .collect();
    assert!(
        drifted.is_empty(),
        "these stdlib siblings declare different param counts, so the JS body binds \
         its params to the wrong arguments: {drifted:?}"
    );
}

fn sibling_facts_with(
    src: &str,
    id: &str,
    want_js: bool,
    native_side: bool,
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
        ) || (native_side && matches!(f.body, FunctionBody::Ffi(..)));
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

fn sibling_facts(
    src: &str,
    id: &str,
    want_js: bool,
) -> std::collections::HashMap<String, (usize, std::collections::BTreeSet<String>)> {
    sibling_facts_with(src, id, want_js, false)
}

#[test]
fn a_sibling_pair_agrees_on_arity_and_annotations() {
    let mut stdlib_lua = std::collections::HashMap::new();
    let mut stdlib_js = std::collections::HashMap::new();
    for f in rock_stdlib::stdlib_files() {
        stdlib_lua.extend(sibling_facts(f.contents, f.name, false));
        stdlib_js.extend(sibling_facts(f.contents, f.name, true));
    }
    let pairs: Vec<(&str, _, _, &[&str])> = vec![("stdlib", stdlib_lua, stdlib_js, &[][..])];

    let mut drifted: Vec<String> = Vec::new();
    for (label, lua, js, ignored) in &pairs {
        let paired: Vec<&String> = js.keys().filter(|n| lua.contains_key(*n)).collect();
        assert!(
            paired.len() >= 10,
            "{label}: only {} sibling pairs found -- the scan is broken, not the source",
            paired.len()
        );
        for n in paired {
            let (la, laa) = &lua[n];
            let (ja, jaa) = &js[n];
            if la != ja {
                drifted.push(format!("{label}::{n} arity lua={la} js={ja}"));
            }
            let keep =
                |s: &std::collections::BTreeSet<String>| -> std::collections::BTreeSet<String> {
                    s.iter()
                        .filter(|a| !ignored.contains(&a.as_str()))
                        .cloned()
                        .collect()
                };
            let (laa, jaa) = (keep(laa), keep(jaa));
            if laa != jaa {
                let lua_only: Vec<&String> = laa.difference(&jaa).collect();
                let js_only: Vec<&String> = jaa.difference(&laa).collect();
                drifted.push(format!(
                    "{label}::{n} lua-only={lua_only:?} js-only={js_only:?}"
                ));
            }
        }
    }
    assert!(
        drifted.is_empty(),
        "these lua/js siblings disagree, so the JS body binds its params or takes its \
         ABI differently from its luajit twin: {drifted:#?}"
    );
}
