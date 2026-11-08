//! Snippet-emission unit tests for the toy JS intrinsic table. No
//! runtime, no program emission -- just the rendered `Math.*` strings and the
//! fail-closed end-to-end check that every JS entry is backed by a stdlib
//! `@intrinsic` declaration (stdlib or the real test-local host fixtures).

use skull_codegen_js::{JS_INTRINSICS, JsIntrinsics, fill_template};
use skull_mir::intrinsics::{declared_intrinsics, unbacked_entries};

fn template(name: &str, arity: usize) -> &'static str {
    JS_INTRINSICS
        .iter()
        .find(|(n, a, _)| *n == name && *a == arity)
        .and_then(|(_, _, t)| match t {
            skull_mir::intrinsics::Emission::Template(t) => Some(*t),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no JS entry {name}/{arity}"))
}

// The `Emission::Template` contract renders to real `Math.*` call syntax when
// its `$1..$n` placeholders are filled.
#[test]
fn math_snippets_render() {
    assert_eq!(fill_template(template("sin", 1), &["x"]), "Math.sin(x)");
    assert_eq!(
        fill_template(template("cos", 1), &["theta"]),
        "Math.cos(theta)"
    );
    assert_eq!(fill_template(template("sqrt", 1), &["n"]), "Math.sqrt(n)");
    assert_eq!(fill_template(template("abs", 1), &["d"]), "Math.abs(d)");
    assert_eq!(fill_template(template("floor", 1), &["v"]), "Math.floor(v)");
    assert_eq!(fill_template(template("ceil", 1), &["v"]), "Math.ceil(v)");
    assert_eq!(
        fill_template(template("pow", 2), &["b", "e"]),
        "Math.pow(b, e)"
    );
    assert_eq!(
        fill_template(template("min", 2), &["p", "q"]),
        "Math.min(p, q)"
    );
    assert_eq!(
        fill_template(template("max", 2), &["p", "q"]),
        "Math.max(p, q)"
    );
    assert_eq!(
        fill_template(template("atan2", 2), &["y", "x"]),
        "Math.atan2(y, x)"
    );
}

// The conformance differential set: mappings
// that are algorithmically distinct from the Rock fallback bodies where a second
// implementation exists (clamp/lerp/sign), coincident where none does
// (round/fract).
#[test]
fn differential_set_snippets_render() {
    assert_eq!(
        fill_template(template("clamp", 3), &["x", "lo", "hi"]),
        "Math.min(Math.max(x, lo), hi)"
    );
    assert_eq!(
        fill_template(template("lerp", 3), &["a", "b", "t"]),
        "(a * (1 - t) + b * t)"
    );
    assert_eq!(
        fill_template(template("round", 1), &["x"]),
        "Math.floor(x + 0.5)"
    );
    assert_eq!(fill_template(template("sign", 1), &["n"]), "Math.sign(n)");
    assert_eq!(
        fill_template(template("fract", 1), &["x"]),
        "(x - Math.floor(x))"
    );
    // smoothstep has no builtin; an IIFE binds the clamped t once.
    assert_eq!(
        fill_template(template("smoothstep", 3), &["e0", "e1", "v"]),
        "(function(e0,e1,x){var t=Math.min(Math.max((x-e0)/(e1-e0),0),1);return t*t*(3-2*t);})(e0, e1, v)"
    );
}

// The NativeBuffer four-fn surface:
// the 5 phantom-width per-type TypedArray allocs (int/float now lower to the
// `FfiNewArray` MIR node, not a named template -- see emit.rs), the shared
// nb_get/nb_set index templates, and the bounds-checked `get` accessor (IIFE so
// receiver/index evaluate once). Cross-backend semantics live in
// tests/nb_differential.rs; these pin the rendered shapes.
#[test]
fn native_buffer_snippets_render() {
    for (name, ctor) in [
        ("__nb_alloc_u8", "Uint8Array"),
        ("__nb_alloc_i8", "Int8Array"),
        ("__nb_alloc_u16", "Uint16Array"),
        ("__nb_alloc_i16", "Int16Array"),
        ("__nb_alloc_u32", "Uint32Array"),
    ] {
        assert_eq!(
            fill_template(template(name, 1), &["n"]),
            format!("new {ctor}(n)")
        );
    }
    assert_eq!(
        fill_template(template("nb_get", 2), &["b", "i"]),
        "b.ptr[i]"
    );
    assert_eq!(
        fill_template(template("nb_set", 3), &["b", "i", "v"]),
        "(b.ptr[i] = v)"
    );
    assert_eq!(
        fill_template(template("get", 2), &["buf", "k"]),
        "(function(b, i) { return (i >= 0 && i < b.len) ? b.ptr[i] : null; })(buf, k)"
    );
}

// Every entry is a distinct (name, arity); covers the base math set + the eight
// conformance differential intrinsics.
#[test]
fn table_entries_are_distinct_keys() {
    let mut keys: Vec<_> = JS_INTRINSICS.iter().map(|(n, a, _)| (*n, *a)).collect();
    let total = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), total, "duplicate (name, arity) in JS table");
    // the eight fallback+mapping intrinsics must all be present.
    for name in [
        "min",
        "max",
        "clamp",
        "lerp",
        "smoothstep",
        "round",
        "sign",
        "fract",
    ] {
        assert!(
            JS_INTRINSICS.iter().any(|(n, _, _)| *n == name),
            "differential intrinsic `{name}` missing from JS table"
        );
    }
}

// Every JS table key must have a matching declaration in the stdlib or the
// real test-local put/try_call fixtures. A source rename or arity drift fails here.
#[test]
fn every_js_entry_is_backed_by_a_declared_intrinsic() {
    let modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    let mut modules = modules;
    modules.push(
        skull_parser::parse_module_clean(
            include_str!("fixtures/js_put.rock"),
            ustr::Ustr::from("<test>"),
        )
        .expect("parse"),
    );
    modules.push(
        skull_parser::parse_module_clean(
            include_str!("fixtures/js_try_call.rock"),
            ustr::Ustr::from("<try-call>"),
        )
        .expect("parse try_call fixture"),
    );
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");

    let declared = declared_intrinsics(&ws);
    let unbacked = unbacked_entries(&JsIntrinsics(&ws), &declared);
    assert!(
        unbacked.is_empty(),
        "JS table has entries no @intrinsic declaration backs (drift): {unbacked:?}"
    );
}
