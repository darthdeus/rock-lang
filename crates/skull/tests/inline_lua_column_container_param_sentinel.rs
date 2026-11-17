//! FIXED -- was a sentinel. An `inline(lua)` body that took a container param
//! received the COLUMN representation with no adaptation and no rejection: the
//! body's `v._data` was `nil`, so a guarded body silently answered wrong and an
//! unguarded one died indexing nil. It is now a spanned rejection at the call.
//!
//! A column-stored `[P]` emits as
//! `{["@vec"] = {repr = "columns", ...}, ["x"] = {}, ["y"] = {}, len = 0}` --
//! per-leaf arrays and no `_data` at all. Every codegen-owned operation on such
//! a vec is ROUTED AROUND the stdlib `inline(lua)` primitives and lowered to
//! direct column stores (which is why `xs.push(..)` works even though
//! `vec_push<T>` is itself an `inline(lua)` writing `_data`; the emitted Lua for
//! a column push contains no call at all). That routing covers the stdlib's own
//! hand-maintained primitive set and reaches nothing a user writes, so admitting
//! the call was the bug -- an opaque body cannot be given a representation
//! contract.
//!
//! DISCRIMINANT, measured: this is not about genericity or instance discovery.
//! The non-generic control fails identically: an `inline(lua)` body is an opaque
//! string, so the boundary plan must reject an unstated container contract.
//! It is also not about the `nobox` mark: an UNMARKED value struct is
//! column-eligible on the same terms, which is why the unmarked case is here
//! rather than in a nobox-specific file.
//!
//! The rejection is keyed on the DECLARED PARAM type, not the argument's. A
//! param spelled `[T]` states that the body expects the vec representation; a
//! param spelled `any` claims no representation at all, and the
//! column-introspection probes that read `v["@vec"]` deliberately take `any`
//! (`tests/vectors/column_stdlib_surface.rock`, `tests/nobox/nullable_column.rock`).
//! Keying on the argument instead rejects the bodies that handle columns
//! CORRECTLY -- measured: it turned three passing files red.
//!
//! The bounding controls are in `tests/vectors/opaque_container_param_controls.rock`:
//! `[int]` is boxed, keeps `_data`, and still passes through the very same body;
//! and an `any` param still admits a column vec.
use crate::common::emit_rejection;

/// Guarded: the body probes for `_data` before touching it, so the column case
/// is a silent wrong ANSWER rather than a death.
const GUARDED_GENERIC: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn probe_len<T>(v: [T]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{probe_len(xs)}")
}
"#;

/// The discriminant control: identical body, NOT generic. Fails the same way,
/// which is what rules the specialization bail-out out as the cause.
const GUARDED_CONCRETE: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn probe_len(v: [P]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{probe_len(xs)}")
}
"#;

/// Blast-radius control: an UNMARKED value struct is column-eligible too.
const GUARDED_UNMARKED: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn probe_len<T>(v: [T]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [Q] = [Q { x: 1.0, y: 2.0 }]
    print("{probe_len(xs)}")
}
"#;

/// Unguarded -- what a user actually writes.
const UNGUARDED_GENERIC: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn first_x<T>(v: [T]) -> float = """
    return v._data[1].x
"""

fn main() {
    let xs: [P] = [P { x: 11.0, y: 2.0 }]
    print("{first_x(xs)}")
}
"#;

/// The callee's parameter is a BARE GENERIC and becomes a container only under
/// this call's binding `T = [P]`. `param_ty` returns the declaration's `TyId`
/// and `ty_node` resolves generics through the ENCLOSING body's substitution --
/// the caller's -- so without the call's own bindings this reads as committing
/// to no constructor, the body is admitted, and the guarded form answers `-1`.
/// Measured before the fix.
const GENERIC_PARAM_BOUND_AT_CALL: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn opaque_t<T>(v: T) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{opaque_t(xs)}")
}
"#;

/// The rejection an opaque body over a column vec now produces.
#[test]
fn a_generic_inline_lua_body_over_a_column_vec_rejects() {
    let msg = emit_rejection(GUARDED_GENERIC);
    assert!(msg.contains("column-stored `[P]`") && msg.contains("probe_len"));
}

/// THE discriminant. Its rejecting identically keeps the defect attributed to
/// the opaque boundary rather than to instance discovery.
#[test]
fn a_non_generic_inline_lua_body_over_a_column_vec_rejects_identically() {
    assert!(emit_rejection(GUARDED_CONCRETE).contains("column-stored `[P]`"));
}

/// Records the blast radius: this is a column-storage rule, not a `nobox` one.
#[test]
fn an_unmarked_value_element_rejects_identically() {
    assert!(emit_rejection(GUARDED_UNMARKED).contains("column-stored `[Q]`"));
}

#[test]
fn an_unguarded_inline_lua_body_over_a_column_vec_rejects() {
    assert!(emit_rejection(UNGUARDED_GENERIC).contains("column-stored `[P]`"));
}

/// The call's binding, not the caller's. Without it the body is ADMITTED and
/// the guarded form silently answers `-1`.
#[test]
fn a_generic_param_bound_to_a_column_vec_at_the_call_rejects() {
    let msg = emit_rejection(GENERIC_PARAM_BOUND_AT_CALL);
    assert!(msg.contains("column-stored `[P]`") && msg.contains("opaque_t"));
}
