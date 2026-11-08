//! Codegen-shape + rejection contract for the `@struct_expand` intrinsic
//!. A call to a `@struct_expand fn f<T>(u:
//! T) -> [(string, E)]` is replaced at emit by the literal pairs list -- one
//! `(field_name, E::V(u.field))` per struct field, declaration order, the variant
//! chosen structurally by payload type. The positive control lives here (successful
//! emit + emitted-Lua substrings); the runtime-value + ordering contract is
//! `tests/annotations/pass_uniforms_expand.rock`; the spanned rejections are also pinned in
//! `tests/errors/struct_expand.rock.err`.
use crate::common::{emit_module, function_body, try_emit_module};

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
    assert!(
        ia < ic && ic < ip,
        "keys must appear in declaration order:\n{body}"
    );

    // One variant box per field, structurally chosen (float->F, int->I, Vec2->Vv).
    for needle in [
        "__variant = (\"F\")",
        "__variant = (\"I\")",
        "__variant = (\"Vv\")",
    ] {
        assert!(body.contains(needle), "missing `{needle}` in:\n{body}");
    }
    // The flat enum stores a value-struct payload spread over its slots, as a
    // source `U::Vv(V2 { .. })` does; a boxed `V2_ct` there is read back by the
    // match as the first slot alone.
    assert!(
        !body.contains("V2_ct("),
        "the Vv payload was boxed, not spread:\n{body}"
    );

    // The phantom body is never called or inlined -- the call is fully replaced.
    assert!(
        !body.contains("phantom body"),
        "phantom body leaked into build():\n{body}"
    );
    assert!(
        !body.contains("upairs("),
        "unexpanded upairs call in build():\n{body}"
    );
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
    assert!(
        body.contains("M_ct("),
        "cdata enum variant should use `M_ct(...)`:\n{body}"
    );
    assert!(
        !body.contains("__variant"),
        "cdata enum must not box:\n{body}"
    );
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
    assert!(
        try_emit_module(&src).is_err(),
        "non-value-struct arg must reject"
    );
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
    assert!(
        try_emit_module(&src).is_err(),
        "unmatched field type must reject"
    );
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
    assert!(
        try_emit_module(src).is_err(),
        "ambiguous payload must reject"
    );
}

// The variant payload `A(T)` is the enum's generic parameter, not the struct
// `T` that happens to share its name. Both render as "T"; matching them stored
// a `T` value where `E<float>` expects a float.
#[test]
fn rejects_nominal_field_against_same_named_generic_payload() {
    let src = r#"
value T { x: float }
enum E<T> { A(T), I(int) }
value S { t: T, n: int }
@struct_expand inline(lua) fn epairs<P>(u: P) -> [(string, E<float>)] = """
    error("phantom")
"""
fn build() -> int { let ps = epairs(S { t: T { x: 2.5 }, n: 3 }); return ps.len() }
"#;
    let err = try_emit_module(src).expect_err("a nominal `T` is not the generic payload `T`");
    assert!(
        err.message.contains("field `t: T` has no variant of `E`"),
        "{}",
        err.message
    );
}

// The enum and the struct live in different modules, so the payload and field
// types sit in different arenas; the same `V2` must still match.
#[test]
fn matches_payload_types_across_modules() {
    let lib = r#"
value V2 { x: float, y: float }
enum U { F(float), Vv(V2) }
"#;
    let src = r#"
value AU { alpha: float, pos: V2 }
@struct_expand inline(lua) fn upairs<T>(u: T) -> [(string, U)] = """
    error("phantom")
"""
fn build(a: float) -> int {
    let ps = upairs(AU { alpha: a, pos: V2 { x: a, y: 4.0 } })
    return ps.len()
}
"#;
    let lua = crate::common::try_emit_with_stdlib_module(lib, src).expect("cross-module expand");
    let body = function_body(&lua, "build");
    for needle in ["__variant = (\"F\")", "__variant = (\"Vv\")"] {
        assert!(body.contains(needle), "missing `{needle}` in:\n{body}");
    }
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
    assert!(
        try_emit_module(src).is_err(),
        "non-pairs return must reject"
    );
}

const NOBOX_PAYLOAD: &str = r#"
nobox value NV2 { x: float, y: float }
enum U { F(float), Vv(NV2) }
value AU { t: float, pos: NV2 }

@struct_expand inline(lua) fn upairs<T>(u: T) -> [(string, U)] = """
    error("upairs phantom body must never run")
"""

fn build(t: float, c: NV2) -> int {
    let ps = upairs(AU { t: t, pos: c })
    return ps.len()
}
"#;

/// A `nobox` payload at the expansion spreads into the flat enum's slots, as a
/// source ctor stores it, so no box is minted at all. Emitting at all is the
/// other half: an implicit box here used to reject a legitimate program
/// (`queue_effect_uniforms`'s `pass_uniforms`).
#[test]
fn a_nobox_payload_spreads_into_the_flat_slots() {
    let lua = emit_module(NOBOX_PAYLOAD);
    let body = function_body(&lua, "build");
    assert!(
        body.contains("__variant = (\"Vv\")"),
        "missing the Vv variant in:\n{body}"
    );
    assert!(
        !body.contains("NV2_ct("),
        "the nobox payload was boxed, not spread:\n{body}"
    );
}
