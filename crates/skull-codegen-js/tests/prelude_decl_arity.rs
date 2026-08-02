//! Step 0 of docs/js-prelude-to-rock-plan.md: the `extern(js)` decl and the
//! `JS_WEB_RUNTIME` function that implements it are two artifacts in two repo
//! locations with nothing checking they agree. JS tolerates arity mismatch
//! silently (extra args dropped, missing ones `undefined`), so a drifted
//! prelude fn fails in the browser as `undefined` flowing into a GL call --
//! never at build time.
//!
//! This checks arity, not semantics, and only for names present in both places.
//! It is strictly weaker than moving the bodies into Rock, but it is the whole
//! drift class for hours instead of days, and it stays useful as a backstop for
//! whatever never migrates.

use std::collections::HashMap;

use skull_parser::ast::{FunctionBody, ItemKind};
use ustr::Ustr;

/// `js_ffi.rock` decls: name -> declared param count, for the JS-backed bodies
/// the prelude is expected to provide. `@scalar_abi` is skipped: its params
/// legitimately explode into leaf params on the JS side, so declared arity and
/// prelude arity are not comparable.
fn js_decl_arities() -> HashMap<String, usize> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../stone/engine/web/js_ffi.rock");
    let src = std::fs::read_to_string(path).expect("read js_ffi.rock");
    let ast = skull_parser::parse_module_clean(&src, Ustr::from("<js_ffi>")).expect("parse js_ffi.rock");
    assert!(ast.recovery_errors.is_empty(), "js_ffi.rock must parse cleanly: {:?}", ast.recovery_errors);

    let mut out = HashMap::new();
    for item in &ast.items {
        let ItemKind::Function(f) = &item.kind else { continue };
        let is_js = matches!(
            f.body,
            FunctionBody::ExternJs | FunctionBody::Inline(bone_semantic::hir::FnBackend::Js, _)
        );
        if !is_js {
            continue;
        }
        if item.annotations.iter().any(|a| a.name.name.as_str() == "scalar_abi") {
            continue;
        }
        out.insert(f.name.name.to_string(), f.params.len());
    }
    assert!(!out.is_empty(), "found no js decls in js_ffi.rock -- the scan is broken");
    out
}

/// `JS_WEB_RUNTIME` top-level functions: name -> param count.
fn prelude_arities() -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for line in skull_codegen_js::JS_WEB_RUNTIME.lines() {
        let Some(rest) = line.strip_prefix("function ") else { continue };
        let Some(open) = rest.find('(') else { continue };
        let Some(close) = rest[open..].find(')') else { continue };
        let name = rest[..open].trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let params = rest[open + 1..open + close].trim();
        let n = if params.is_empty() { 0 } else { params.split(',').count() };
        out.insert(name.to_string(), n);
    }
    assert!(!out.is_empty(), "found no prelude functions -- the scan is broken");
    out
}

/// Every name declared on the Rock side AND defined in the prelude must agree on
/// arity. Names in only one place are out of scope here: a decl with no prelude
/// entry is legitimately driver-provided, and a prelude fn with no decl is
/// JS-internal plumbing (the `__sw_*` helpers) -- both tracked in the plan doc.
#[test]
fn prelude_and_decl_arities_agree() {
    let decls = js_decl_arities();
    let prelude = prelude_arities();

    let mut mismatches: Vec<String> = decls
        .iter()
        .filter_map(|(name, &want)| {
            let &got = prelude.get(name)?;
            (got != want).then(|| format!("{name}: decl takes {want}, prelude takes {got}"))
        })
        .collect();
    mismatches.sort();

    assert!(
        mismatches.is_empty(),
        "js_ffi.rock decls and JS_WEB_RUNTIME disagree on arity:\n  {}",
        mismatches.join("\n  ")
    );
}

/// `extern(js)` names only -- the decls whose body the prelude still owns.
fn extern_js_decls() -> Vec<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../stone/engine/web/js_ffi.rock");
    let src = std::fs::read_to_string(path).expect("read js_ffi.rock");
    let ast = skull_parser::parse_module_clean(&src, Ustr::from("<js_ffi>")).expect("parse js_ffi.rock");
    ast.items
        .iter()
        .filter_map(|item| {
            let ItemKind::Function(f) = &item.kind else { return None };
            matches!(f.body, FunctionBody::ExternJs).then(|| f.name.name.to_string())
        })
        .collect()
}

/// A `@nomangle inline(js)` fn emits under its BARE name, so a same-named
/// prelude function is a second definition of that name: both emit, and which
/// one a call reaches depends on concatenation order in `assemble_web_program`.
/// This is the check that fails if a stage-1/2 edit rewrites a decl and forgets
/// to delete the prelude entry.
///
/// A MANGLED `inline(js)` twin is the opposite -- the deliberate trampoline
/// idiom, where the mangled Rock body delegates to the bare prelude global, so
/// both must exist. Keyed on `@nomangle` rather than a name list so the idiom
/// stays available without re-editing this test.
#[test]
fn a_nomangle_js_fn_has_no_prelude_twin() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../stone/engine/web/js_ffi.rock");
    let src = std::fs::read_to_string(path).expect("read js_ffi.rock");
    let ast = skull_parser::parse_module_clean(&src, Ustr::from("<js_ffi>")).expect("parse js_ffi.rock");
    let prelude = prelude_arities();

    let mut twins: Vec<String> = ast
        .items
        .iter()
        .filter_map(|item| {
            let ItemKind::Function(f) = &item.kind else { return None };
            let inline_js =
                matches!(f.body, FunctionBody::Inline(bone_semantic::hir::FnBackend::Js, _));
            let nomangle =
                item.annotations.iter().any(|a| a.name.name.as_str() == "nomangle");
            let name = f.name.name.to_string();
            (inline_js && nomangle && prelude.contains_key(&name)).then_some(name)
        })
        .collect();
    twins.sort();
    twins.dedup();

    assert!(
        twins.is_empty(),
        "these emit a bare name that JS_WEB_RUNTIME also defines:\n  {}",
        twins.join("\n  ")
    );
}

/// The arity check is only worth anything while it still compares names. It
/// weakens as stages 1-2 migrate bodies out of the prelude, so the floor tracks
/// the REMAINING `extern(js)` surface rather than a fixed count: every extern
/// decl the prelude defines must be compared, and most of them still should be.
/// When this reaches zero the prelude owns no declared body and the whole file
/// retires (plan stage 5).
#[test]
fn scan_covers_the_remaining_extern_surface() {
    let decls = js_decl_arities();
    let prelude = prelude_arities();
    let externs = extern_js_decls();
    let overlap = decls.keys().filter(|n| prelude.contains_key(*n)).count();
    let extern_with_body = externs.iter().filter(|n| prelude.contains_key(*n)).count();
    eprintln!(
        "decls={} prelude={} overlap={} extern_with_prelude_body={}",
        decls.len(),
        prelude.len(),
        overlap,
        extern_with_body
    );
    assert!(
        overlap >= extern_with_body,
        "{extern_with_body} extern(js) decls have a prelude body but only {overlap} are compared"
    );
    assert!(
        extern_with_body > 50,
        "only {extern_with_body} extern(js) decls still have a prelude body -- if that is real \
         progress, lower this floor; if the scan broke, fix the scan"
    );
}

/// A hand-written `inline(js)` body must not implement the multi-value return
/// ABI itself. Nine imgui stubs did -- `return [false, value];` -- which was
/// correct under the old transport-array ABI and silently wrong under the
/// scratch-slot one: the caller reads leaf 0 (getting the whole array) and then
/// `__rock_ret1` (getting `undefined`). No shape assertion catches it, because
/// the body is an opaque string to the compiler.
///
/// Scans every Rock source that can carry a JS body, not just js_ffi.rock.
#[test]
fn no_inline_js_body_hand_writes_the_array_return_abi() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![root.join("stone"), root.join("stdlib")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("rock") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else { continue };
            scanned += 1;
            for (i, line) in src.lines().enumerate() {
                if line.contains("inline(js)") && line.contains("return [") {
                    offenders.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
    }
    assert!(scanned > 20, "scanned only {scanned} .rock files -- the walk is broken");
    assert!(
        offenders.is_empty(),
        "these inline(js) bodies return an array instead of using the scratch ABI \
         (__rock_ret1 = ..; return leaf0;):\n  {}",
        offenders.join("\n  ")
    );
}
