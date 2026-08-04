//! What the 2.4 trigger must READ to reach every declared slot
//! (`docs/boundary-contracts.md`).
//!
//! The rule asks a question about the callee's DECLARED type. Two ways of
//! answering it were wrong, and both failed silently at runtime rather than
//! rejecting:
//!
//! - Reading the declared RETURN raw. `fn make<T>() -> T` names no constructor
//!   until `T` binds, so a call inferred at `T = [G]` minted `{_data = {}}` for
//!   a column-planned result and the caller's column read died on nil.
//! - Zipping raw argument expressions against parameter INDICES. A named call
//!   supplies arguments in the caller's order, so `probe(token: 0, sink: xs)`
//!   compared `sink`'s declared type against `token`'s argument and reached the
//!   body with a column vec.
//!
//! Both are the same defect: the declared type was not resolved to a concrete
//! one. The fix is one oracle over declared SLOTS -- callee generics from bone's
//! `call_generic_bindings`, then the enclosing substitution for the caller's own
//! -- so named arguments, defaults and spreads need no special case, because no
//! argument list is walked at all.

const GENERIC_RETURN: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn make<T>() -> T = """
    return {_data = {}, len = 0, __ty = "Vec"}
"""

fn main() {
    let xs: [G] = make()
    print("{xs.len}")
}
"#;

/// The same body at a BOXED element: the boxed representation is exactly what it
/// builds, so it must stay valid.
const GENERIC_RETURN_BOXED: &str = r#"
struct B { s: string }

@nomangle inline(lua) fn make<T>() -> T = """
    return {_data = {}, len = 0, __ty = "Vec"}
"""

fn main() {
    let xs: [B] = make()
    print("{xs.len}")
}
"#;

/// The same body at a SCALAR: `T = int` names no container at all, so the rule
/// must find nothing. Without this, "reject every generic return" would pass.
const GENERIC_RETURN_SCALAR: &str = r#"
@nomangle inline(lua) fn make<T>() -> T = """
    return 7
"""

fn main() {
    let n: int = make()
    print("{n}")
}
"#;

const NAMED_OUT_OF_ORDER: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(token: 0, sink: xs)}")
}
"#;

/// The POSITIONAL twin. It rejected before this work, so its staying red is what
/// says the named form is a reach problem rather than a rule problem.
const POSITIONAL_CONTROL: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs, 0)}")
}
"#;

/// Named, out of order, at a BOXED element: still valid.
const NAMED_OUT_OF_ORDER_BOXED: &str = r#"
struct B { s: string }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [B] = [B { s: "a" }]
    print("{probe(token: 0, sink: xs)}")
}
"#;

/// A DEFAULTED slot the call never supplies. The declared type is still the
/// question, and there is no argument expression to have zipped against.
const DEFAULTED_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(token: int, sink: [T] = []) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(0, xs)}")
}
"#;

/// `@repr(opaque)` stays keyed to the DECLARED parameter index, so it must still
/// waive the right slot when the call names its arguments out of order.
const NAMED_WITH_REPR_OPAQUE: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn take<T>(sink: [T], @repr(opaque) item: T, token: int) -> int = """
    sink._data[1] = item
    return token
"""

fn main() {
    let outer: [[G]] = []
    let inner: [G] = [G { x: 1.0, y: 2.0 }]
    print("{take(token: 5, item: inner, sink: outer)}")
}
"#;

fn outcome(src: &str) -> Result<String, String> {
    crate::common::try_emit_module(src).map_err(|e| e.message)
}

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

#[test]
fn a_generic_return_inferred_as_a_column_vec_rejects() {
    let msg = outcome(GENERIC_RETURN).expect_err("must reject");
    assert!(
        msg.contains("cannot BUILD column-stored `[G]`") && msg.contains("make"),
        "got: {msg}"
    );
}

#[test]
fn a_generic_return_at_a_boxed_element_is_admitted() {
    assert_eq!(run(GENERIC_RETURN_BOXED, "decl_ret_boxed"), "0");
}

#[test]
fn a_generic_return_at_a_scalar_is_admitted() {
    assert_eq!(run(GENERIC_RETURN_SCALAR, "decl_ret_scalar"), "7");
}

#[test]
fn a_named_out_of_order_column_argument_rejects() {
    let msg = outcome(NAMED_OUT_OF_ORDER).expect_err("must reject");
    assert!(
        msg.contains("column-stored `[G]`") && msg.contains("probe"),
        "got: {msg}"
    );
}

/// The reach control: the same call written positionally already rejected.
#[test]
fn the_positional_form_rejects_too() {
    assert!(outcome(POSITIONAL_CONTROL).expect_err("must reject").contains("column-stored `[G]`"));
}

#[test]
fn a_named_out_of_order_boxed_argument_is_admitted() {
    assert_eq!(run(NAMED_OUT_OF_ORDER_BOXED, "decl_named_boxed"), "1");
}

#[test]
fn a_defaulted_slot_is_reached() {
    let msg = outcome(DEFAULTED_SLOT).expect_err("must reject");
    assert!(msg.contains("column-stored `[G]`"), "got: {msg}");
}

#[test]
fn repr_opaque_waives_the_declared_slot_under_named_arguments() {
    assert_eq!(
        run(NAMED_WITH_REPR_OPAQUE, "decl_named_repr"),
        "5",
        "the waiver follows the declared index, not the call's argument order"
    );
}
