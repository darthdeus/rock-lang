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
//! DISCRIMINANT, measured: this was NOT about genericity and NOT about the
//! specialization bail-out. The non-generic control fails identically, so
//! `compute_call_spec`'s `inline_body.is_some()` arm was never the cause -- an
//! `inline(lua)` body is an opaque string, so nothing can adapt it either way.
//! It is also not about the `nobox` mark: an UNMARKED value struct is
//! column-eligible on the same terms, which is why the unmarked case is here
//! rather than in a nobox-specific file.
//!
//! The rejection is keyed on the DECLARED PARAM type, not the argument's. A
//! param spelled `[T]` states that the body expects the vec representation; a
//! param spelled `any` claims no representation at all, and the
//! column-introspection probes that read `v["@vec"]` deliberately take `any`
//! (`tests/vec_column_stdlib_surface.rock`, `tests/nobox_nullable_column.rock`).
//! Keying on the argument instead rejects the bodies that handle columns
//! CORRECTLY -- measured: it turned three passing files red.
//!
//! The bounding controls stay: `[int]` is boxed, keeps `_data`, and still passes
//! through the very same body; and an `any` param still admits a column vec.

mod common;

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

/// Bounding control: a boxed element keeps `_data`, so the same body is correct.
const GUARDED_INT_ELEM: &str = r#"
@nomangle inline(lua) fn probe_len<T>(v: [T]) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{probe_len(xs)}")
}
"#;

/// The `any` bound: a body that reads the column layout itself is still allowed.
const ANY_PARAM_PROBE: &str = r#"
nobox value P { x: float, y: float }

@nomangle inline(lua) fn header_repr(v: any) -> string = """
    return v["@vec"].repr
"""

fn main() {
    let xs: [P] = [P { x: 1.0, y: 2.0 }]
    print("{header_repr(xs)}")
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

fn run(src: &str, tag: &str) -> String {
    let lua = common::try_emit_module(src).expect("must emit");
    common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

/// The rejection an opaque body over a column vec now produces.
fn rejection(src: &str) -> String {
    common::try_emit_module(src)
        .expect_err("a column vec at an opaque `[T]` param must reject")
        .message
}

#[test]
fn a_generic_inline_lua_body_over_a_column_vec_rejects() {
    let msg = rejection(GUARDED_GENERIC);
    assert!(
        msg.contains("column-stored `[P]`") && msg.contains("probe_len"),
        "the rejection must name the element and the callee, got: {msg}"
    );
}

/// THE discriminant. Its rejecting identically is what keeps the defect attributed
/// to the opaque boundary rather than to `compute_call_spec`'s bail-out.
#[test]
fn a_non_generic_inline_lua_body_over_a_column_vec_rejects_identically() {
    assert!(rejection(GUARDED_CONCRETE).contains("column-stored `[P]`"));
}

/// Records the blast radius: this is a column-storage rule, not a `nobox` one.
#[test]
fn an_unmarked_value_element_rejects_identically() {
    assert!(rejection(GUARDED_UNMARKED).contains("column-stored `[Q]`"));
}

#[test]
fn an_unguarded_inline_lua_body_over_a_column_vec_rejects() {
    assert!(rejection(UNGUARDED_GENERIC).contains("column-stored `[P]`"));
}

/// Bounds the rule on the element type. Without this, a change that rejected
/// every `inline(lua)` container param would leave the others green and read as
/// progress.
#[test]
fn a_boxed_element_through_the_same_body_is_correct() {
    assert_eq!(
        run(GUARDED_INT_ELEM, "inline_lua_col_int"),
        "3",
        "a boxed `[int]` keeps `_data`, so the same body must stay correct"
    );
}

/// Bounds the rule on the PARAM SPELLING: a body that claims no representation
/// still receives a column vec, which is how the introspection probes work.
#[test]
fn an_any_param_still_admits_a_column_vec() {
    assert_eq!(
        run(ANY_PARAM_PROBE, "inline_lua_col_any"),
        "columns",
        "an `any` param claims no representation, so it must still be admitted"
    );
}
