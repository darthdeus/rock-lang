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
    let ast = skull_parser::parse_module(&src, Ustr::from("<js_ffi>")).expect("parse js_ffi.rock");
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

#[test]
fn scan_covers_a_meaningful_overlap() {
    let decls = js_decl_arities();
    let prelude = prelude_arities();
    let overlap = decls.keys().filter(|n| prelude.contains_key(*n)).count();
    eprintln!("decls={} prelude={} overlap={}", decls.len(), prelude.len(), overlap);
    assert!(overlap > 100, "arity check only compares {overlap} names -- too weak to trust");
}
