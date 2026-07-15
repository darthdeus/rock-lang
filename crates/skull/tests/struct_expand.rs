//! Codegen-shape + rejection contract for the `@struct_expand` intrinsic
//! (docs/pass-uniforms-design.md, Layer 2). A call to a `@struct_expand fn f<T>(u:
//! T) -> [(string, E)]` is replaced at emit by the literal pairs list -- one
//! `(field_name, E::V(u.field))` per struct field, declaration order, the variant
//! chosen structurally by payload type. The positive control lives here (successful
//! emit + emitted-Lua substrings); the runtime-value + ordering contract is
//! `tests/pass_uniforms_expand.rock`; the spanned rejections are also pinned in
//! `tests/errors/struct_expand.rock.err`.

mod common;
use common::{emit_module, function_body, try_emit_module};

const BOXED: &str = r#"
value V2 { x: float, y: float }
enum U { F(float), I(int), Vv(V2) }
value AU { alpha: float, count: int, pos: V2 }

@struct_expand inline(lua) fn upairs<T>(u: T) -> [(string, U)] = """
    error("upairs phantom body must never run")
"""

// Non-const fields so the expansion stays inline (a fully-const literal would be
// const-hoisted -- folding correctly threads through the expansion).
fn build(a: float, c: int, px: float) -> int {
    let ps = upairs(AU { alpha: a, count: c, pos: V2 { x: px, y: 4.0 } })
    return ps.len()
}
"#;

#[test]
fn expands_to_literal_pairs_in_declaration_order() {
    let lua = emit_module(BOXED);
    let body = function_body(&lua, "build");

    // Field-name keys, in declaration order.
    let ia = body.find("\"alpha\"").expect("alpha key");
    let ic = body.find("\"count\"").expect("count key");
    let ip = body.find("\"pos\"").expect("pos key");
    assert!(ia < ic && ic < ip, "keys must appear in declaration order:\n{body}");

    // One variant box per field, structurally chosen (float->F, int->I, Vec2->Vv).
    for needle in ["__variant = \"F\"", "__variant = \"I\"", "__variant = \"Vv\""] {
        assert!(body.contains(needle), "missing `{needle}` in:\n{body}");
    }

    // The phantom body is never called or inlined -- the call is fully replaced.
    assert!(!body.contains("phantom body"), "phantom body leaked into build():\n{body}");
    assert!(!body.contains("upairs("), "unexpanded upairs call in build():\n{body}");
}

// A cdata-eligible enum (`One`/`Both` share slot `_0: int`) expands to the
// `<Enum>_ct` cdata form, not a boxed `{__variant}` table.
const CDATA: &str = r#"
enum M { One(int), Both(int, int) }
value MU { n: int }

@struct_expand inline(lua) fn mpairs<T>(u: T) -> [(string, M)] = """
    error("phantom")
"""

fn build(k: int) -> int {
    let ps = mpairs(MU { n: k })
    return ps.len()
}
"#;

#[test]
fn cdata_enum_expands_to_cdata_ctor() {
    let lua = emit_module(CDATA);
    let body = function_body(&lua, "build");
    assert!(body.contains("M_ct("), "cdata enum variant should use `M_ct(...)`:\n{body}");
    assert!(!body.contains("__variant"), "cdata enum must not box:\n{body}");
}

fn header(extra: &str) -> String {
    format!(
        r#"
enum E {{ F(float), I(int) }}
{extra}
"#
    )
}

#[test]
fn rejects_non_value_struct_argument() {
    let src = header(
        r#"
@struct_expand inline(lua) fn epairs<T>(u: T) -> [(string, E)] = """
    error("phantom")
"""
fn build() -> int { let ps = epairs(3); return ps.len() }
"#,
    );
    assert!(try_emit_module(&src).is_err(), "non-value-struct arg must reject");
}

#[test]
fn rejects_field_type_with_no_matching_variant() {
    let src = header(
        r#"
value S { flag: bool }
@struct_expand inline(lua) fn epairs<T>(u: T) -> [(string, E)] = """
    error("phantom")
"""
fn build() -> int { let ps = epairs(S { flag: true }); return ps.len() }
"#,
    );
    assert!(try_emit_module(&src).is_err(), "unmatched field type must reject");
}

#[test]
fn rejects_ambiguous_variant_payload() {
    let src = r#"
enum Dup { A(float), B(float) }
value S { x: float }
@struct_expand inline(lua) fn dpairs<T>(u: T) -> [(string, Dup)] = """
    error("phantom")
"""
fn build() -> int { let ps = dpairs(S { x: 1.0 }); return ps.len() }
"#;
    assert!(try_emit_module(src).is_err(), "ambiguous payload must reject");
}

#[test]
fn rejects_return_type_not_pairs() {
    let src = r#"
value S { x: float }
@struct_expand inline(lua) fn bad<T>(u: T) -> int = """
    error("phantom")
"""
fn build() -> int { return bad(S { x: 1.0 }) }
"#;
    assert!(try_emit_module(src).is_err(), "non-pairs return must reject");
}
