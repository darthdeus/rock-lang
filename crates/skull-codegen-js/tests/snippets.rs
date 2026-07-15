//! Snippet-emission unit tests for the toy JS intrinsic table (slice 4). No
//! runtime, no program emission -- just the rendered `Math.*` strings and the
//! fail-closed end-to-end check that every JS entry is backed by a stdlib
//! `@intrinsic` declaration.

use skull_codegen_js::{fill_template, JsIntrinsics, JS_INTRINSICS};
use skull_mir::intrinsics::{declared_intrinsics, unbacked_entries};

fn template(name: &str, arity: usize) -> &'static str {
    JS_INTRINSICS
        .iter()
        .find(|(n, a, _)| *n == name && *a == arity)
        .map(|(_, _, t)| *t)
        .unwrap_or_else(|| panic!("no JS entry {name}/{arity}"))
}

// The `Emission::Template` contract renders to real `Math.*` call syntax when
// its `$1..$n` placeholders are filled.
#[test]
fn math_snippets_render() {
    assert_eq!(fill_template(template("sin", 1), &["x"]), "Math.sin(x)");
    assert_eq!(fill_template(template("cos", 1), &["theta"]), "Math.cos(theta)");
    assert_eq!(fill_template(template("sqrt", 1), &["n"]), "Math.sqrt(n)");
    assert_eq!(fill_template(template("abs", 1), &["d"]), "Math.abs(d)");
    assert_eq!(fill_template(template("floor", 1), &["v"]), "Math.floor(v)");
    assert_eq!(fill_template(template("ceil", 1), &["v"]), "Math.ceil(v)");
    assert_eq!(fill_template(template("pow", 2), &["b", "e"]), "Math.pow(b, e)");
    assert_eq!(fill_template(template("min", 2), &["p", "q"]), "Math.min(p, q)");
    assert_eq!(fill_template(template("max", 2), &["p", "q"]), "Math.max(p, q)");
    assert_eq!(fill_template(template("atan2", 2), &["y", "x"]), "Math.atan2(y, x)");
}

// The plan's ~10 math entries, each a distinct (name, arity).
#[test]
fn table_is_the_ten_math_entries() {
    assert_eq!(JS_INTRINSICS.len(), 10);
    let mut keys: Vec<_> = JS_INTRINSICS.iter().map(|(n, a, _)| (*n, *a)).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 10, "duplicate (name, arity) in JS table");
}

// Fail-closed, END TO END: the toy table is math-only and every entry is
// annotated `@intrinsic` in the stdlib (slice 3), so `unbacked_entries` is
// empty. This is the demonstration deferred from slice 2 -- it lives on the JS
// table because there `unbacked_entries` can actually reach empty (the GLSL
// table still has Vec-method/`texture` entries outside the scalar proving set).
// It goes RED the moment a JS key drifts from the stdlib signature.
#[test]
fn every_js_entry_is_backed_by_a_stdlib_intrinsic() {
    let modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect();
    let mut modules = modules;
    modules.push(skull_parser::parse_module("fn main() {}", ustr::Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");

    let declared = declared_intrinsics(&ws);
    let unbacked = unbacked_entries(&JsIntrinsics, &declared);
    assert!(
        unbacked.is_empty(),
        "toy JS table has entries no stdlib @intrinsic declaration backs (drift): {unbacked:?}"
    );
}
