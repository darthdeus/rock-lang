//! What the 2.4 trigger must READ to reach every declared slot
//!.
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
use crate::common::emit_rejection;

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

/// A call that omits a DEFAULTED slot. The argument list is shorter than the
/// declared slots, and the column slot is still reached.
const DEFAULTED_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T], token: int = 0) -> int = """
    return #sink._data + token
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs)}")
}
"#;

#[test]
fn a_generic_return_inferred_as_a_column_vec_rejects() {
    let msg = emit_rejection(GENERIC_RETURN);
    assert!(msg.contains("cannot BUILD column-stored `[G]`") && msg.contains("make"));
}

#[test]
fn a_named_out_of_order_column_argument_rejects() {
    let msg = emit_rejection(NAMED_OUT_OF_ORDER);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

/// The reach control: the same call written positionally already rejected.
#[test]
fn the_positional_form_rejects_too() {
    assert!(emit_rejection(POSITIONAL_CONTROL).contains("column-stored `[G]`"));
}

#[test]
fn a_defaulted_slot_is_reached() {
    let msg = emit_rejection(DEFAULTED_SLOT);
    assert!(msg.contains("column-stored `[G]`"));
}

/// An IRRELEVANT generic the call never pins. `U` appears in no slot, so
/// nothing binds it -- and the slot under test, `sink`, is fully determined.
/// Abandoning the whole call when ANY generic fails to convert let an unrelated
/// unbound variable disarm the rule.
const IRRELEVANT_UNBOUND_GENERIC: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T, U>(sink: [T]) -> int = """
    return #sink._data
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs)}")
}
"#;

#[test]
fn an_irrelevant_unbound_generic_does_not_disarm_the_rule() {
    let msg = emit_rejection(IRRELEVANT_UNBOUND_GENERIC);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

/// A NULLABLE slot. `[T]?` names the same boxed representation once the value
/// is present, so a column vec reaching it meets the same nil `_data`.
const NULLABLE_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(sink: [T]? = null) -> int = """
    if sink == nil then return 0 end
    return #sink._data
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe(xs)}")
}
"#;

/// A TUPLE slot carries its element by reference, so the column vec inside it
/// reaches the body exactly as a bare one would.
const TUPLE_SLOT: &str = r#"
value G { x: float, y: float }

@nomangle inline(lua) fn probe<T>(pair: ([T], int)) -> int = """
    return #pair[1]._data
"""

fn main() {
    let xs: [G] = [G { x: 1.0, y: 2.0 }]
    print("{probe((xs, 0))}")
}
"#;

#[test]
fn a_nullable_slot_is_reached() {
    let msg = emit_rejection(NULLABLE_SLOT);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}

#[test]
fn a_tuple_slot_is_reached() {
    let msg = emit_rejection(TUPLE_SLOT);
    assert!(msg.contains("column-stored `[G]`") && msg.contains("probe"));
}
