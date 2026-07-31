//! The lua/js SIBLING CONTRACT: one function, two bodies, in two files, with
//! nothing in the compiler binding them together. This file holds every check
//! that they still agree -- where the JS body lives, its arity, and its
//! annotations. JS reports neither a surplus argument nor a missing one, and an
//! annotation is a compile-time shape decision, so every drift in this class is
//! silent by construction.
//!
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
    for f in rock_host::stdlib_loader::stdlib_files() {
        let ast = skull_parser::parse_module(f.contents, Ustr::from(f.name)).expect("stdlib parse");
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

/// `name -> (arity, annotations)` for every fn in `src` whose body is
/// `inline(<backend>)` / `extern(js)`.
fn sibling_facts(
    src: &str,
    id: &str,
    want_js: bool,
) -> std::collections::HashMap<String, (usize, std::collections::BTreeSet<String>)> {
    let ast = skull_parser::parse_module(src, Ustr::from(id)).expect("parse");
    assert!(ast.recovery_errors.is_empty(), "{id} must parse cleanly");
    let mut out = std::collections::HashMap::new();
    for item in &ast.items {
        let ItemKind::Function(f) = &item.kind else { continue };
        let is_js = matches!(
            f.body,
            FunctionBody::ExternJs | FunctionBody::Inline(bone_semantic::hir::FnBackend::Js, _)
        );
        let is_lua = matches!(f.body, FunctionBody::Inline(bone_semantic::hir::FnBackend::Lua, _));
        if is_js != want_js || (!is_js && !is_lua) {
            continue;
        }
        let anns: std::collections::BTreeSet<String> = item
            .annotations
            .iter()
            .map(|a| a.name.name.to_string())
            // `@inline` is unreachable on a fn that HAS an inline body: both
            // `classify` (skull-transform) and `compute_call_spec` (skull-mir)
            // return early on `inline_body.is_some()` before they read it. So a
            // sibling carrying it and one not carrying it behave identically,
            // and gating on it would only assert a cosmetic difference.
            .filter(|n| n != "inline")
            .collect();
        out.insert(f.name.name.to_string(), (f.params.len(), anns));
    }
    out
}

/// Every paired lua/js sibling must agree on arity AND annotations.
///
/// `@scalar_abi` is the one that has actually bitten: 23 imgui stubs were
/// missing the annotation their luajit siblings carry (`a60d48aa1`), which made
/// them boxed boundaries a marked `Vec2` could not cross -- and that is a
/// compile-time shape decision, so nothing failed until a whole game stopped
/// building. The check is not limited to it: any annotation that changes how a
/// caller must emit the call has the same silent failure mode.
#[test]
fn a_sibling_pair_agrees_on_arity_and_annotations() {
    // The stdlib pair is assembled from the loader (it is split across files);
    // the engine pair is two named files that own each other's siblings.
    let mut stdlib_lua = std::collections::HashMap::new();
    let mut stdlib_js = std::collections::HashMap::new();
    for f in rock_host::stdlib_loader::stdlib_files() {
        stdlib_lua.extend(sibling_facts(f.contents, f.name, false));
        stdlib_js.extend(sibling_facts(f.contents, f.name, true));
    }
    let imgui_lua_src = std::fs::read_to_string(repo("stone/engine/ui/imgui.rock")).expect("imgui.rock");
    let imgui_js_src =
        std::fs::read_to_string(repo("stone/engine/web/imgui_stubs.rock")).expect("imgui_stubs.rock");

    let pairs: Vec<(&str, _, _)> = vec![
        ("stdlib", stdlib_lua, stdlib_js),
        (
            "imgui",
            sibling_facts(&imgui_lua_src, "<imgui>", false),
            sibling_facts(&imgui_js_src, "<imgui_stubs>", true),
        ),
    ];

    let mut drifted: Vec<String> = Vec::new();
    let mut paired_total = 0usize;
    for (label, lua, js) in &pairs {
        let paired: Vec<&String> = js.keys().filter(|n| lua.contains_key(*n)).collect();
        assert!(
            paired.len() >= 10,
            "{label}: only {} sibling pairs found -- the scan is broken, not the source",
            paired.len()
        );
        paired_total += paired.len();
        for n in paired {
            let (la, laa) = &lua[n];
            let (ja, jaa) = &js[n];
            if la != ja {
                drifted.push(format!("{label}::{n} arity lua={la} js={ja}"));
            }
            if laa != jaa {
                let lua_only: Vec<&String> = laa.difference(jaa).collect();
                let js_only: Vec<&String> = jaa.difference(laa).collect();
                drifted.push(format!("{label}::{n} lua-only={lua_only:?} js-only={js_only:?}"));
            }
        }
    }
    assert!(paired_total >= 60, "expected both pairs to contribute; got {paired_total}");
    assert!(
        drifted.is_empty(),
        "these lua/js siblings disagree, so the JS body binds its params or takes its \
         ABI differently from its luajit twin: {drifted:#?}"
    );
}
