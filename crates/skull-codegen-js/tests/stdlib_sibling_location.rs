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
    assert!(names.contains(&"js.rock"), "stdlib/js.rock is not in the load list: {names:?}");
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
        let ast = skull_parser::parse_module_clean(f.contents, Ustr::from(f.name)).expect("stdlib parse");
        for item in &ast.items {
            let ItemKind::Function(fun) = &item.kind else { continue };
            // `@scalar_abi` params explode into leaves on the JS side, so the two
            // declared arities are legitimately incomparable.
            if item.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi") {
                continue;
            }
            let name = fun.name.name.to_string();
            match fun.body {
                FunctionBody::Inline(bone_semantic::hir::FnBackend::Lua, _) => {
                    lua.insert(name, fun.params.len());
                }
                FunctionBody::ExternJs | FunctionBody::Inline(bone_semantic::hir::FnBackend::Js, _) => {
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
