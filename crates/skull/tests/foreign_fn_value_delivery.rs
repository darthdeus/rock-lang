//! **A FUNCTION-VALUE take of a foreign body needs a standalone definition.**
//!
//! Foreign (`inline(lua)`) delivery is separate from retention: a body can be
//! retained and spliced away at every use, so a definition is emitted only when
//! something needs the residual form. That demand is collected from what
//! already-lowered chunks record they still CALL -- and a value take is not a
//! call. There is nothing to splice into: the program stores the definition
//! itself, under its own symbol.
//!
//! The end-to-end control is `tests/closures/function_ref_cross_module.rock`, whose two
//! `@test`s die on `attempt to call local 'r0' (a nil value)` when the
//! definition is missing.

use crate::common::emit_module;

fn definition_symbols(lua: &str) -> Vec<String> {
    lua.match_indices("\nfunction ")
        .map(|(i, m)| {
            let rest = &lua[i + m.len()..];
            rest[..rest.find('(').expect("fn header has `(`")].to_string()
        })
        .collect()
}

fn defines(lua: &str, symbol: &str) -> bool {
    definition_symbols(lua).iter().any(|s| s == symbol)
}

const SYMBOL: &str = "sqrt_float";
/// A foreign body that IS spliced at its call sites -- integer `+` lowers to a
/// native op rather than a residual call. Asserted in the SAME emission as the
/// arm below, so "define every retained foreign body" cannot be what makes the
/// arm pass.
const SPLICED: &str = "__op_add_int_int";

#[test]
fn a_foreign_body_taken_as_a_value_is_defined() {
    let lua = emit_module(
        r#"
fn main() {
    let f: fn(float) -> float = sqrt
    let a = 3
    let b = 4
    print("{f(2.0)} {a + b}")
}
"#,
    );
    assert!(
        !defines(&lua, SPLICED),
        "control: a spliced foreign body must have no standalone `{SPLICED}`"
    );
    assert!(
        defines(&lua, SYMBOL),
        "a value take has no call to splice into, so `{SYMBOL}` needs a definition"
    );
}

/// The same obligation through a different site shape: the value is stored in a
/// struct field rather than bound by `let`.
#[test]
fn a_foreign_body_stored_in_a_field_is_defined() {
    let lua = emit_module(
        r#"
struct FnBox { f: fn(float) -> float }

fn main() {
    let b = FnBox { f: sqrt }
    print("{b.f(2.0)}")
}
"#,
    );
    assert!(
        defines(&lua, SYMBOL),
        "`{SYMBOL}` needs a definition when stored as a value"
    );
}

/// The second control: `sqrt` is not unconditionally present. A program that
/// neither calls nor value-takes it emits no definition, so the arms above
/// measure the reaching site rather than a fixed stdlib preamble.
#[test]
fn an_unreached_foreign_body_is_not_defined() {
    let lua = emit_module(
        r#"
fn main() {
    let a = 3
    let b = 4
    print("{a + b}")
}
"#,
    );
    assert!(
        !defines(&lua, SYMBOL),
        "`{SYMBOL}` is reached by nothing here"
    );
}
