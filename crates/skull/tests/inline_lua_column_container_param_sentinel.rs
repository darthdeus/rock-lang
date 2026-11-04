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
//! (`tests/vec_column_stdlib_surface.rock`, `tests/nobox_nullable_column.rock`).
//! Keying on the argument instead rejects the bodies that handle columns
//! CORRECTLY -- measured: it turned three passing files red.
//!
//! The bounding controls stay: `[int]` is boxed, keeps `_data`, and still passes
//! through the very same body; and an `any` param still admits a column vec.

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

/// Same signature, boxed element: the binding is still `T = [int]`, so this
/// bounds the fix on the ELEMENT rather than on "a generic param rejects".
const GENERIC_PARAM_BOXED_ELEM: &str = r#"
@nomangle inline(lua) fn opaque_t<T>(v: T) -> int = """
    if v._data == nil then return -1 end
    return #v._data
"""

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{opaque_t(xs)}")
}
"#;

/// A bare generic bound to a NON-container: the rule must find no constructor
/// and stay silent, or every generic opaque body would reject.
/// The type guard leads because indexing a number THROWS in Lua before the body
/// could answer -- without it this fixture dies for a reason unrelated to the
/// rule, and would read as the rule firing.
const GENERIC_PARAM_SCALAR: &str = r#"
@nomangle inline(lua) fn opaque_t<T>(v: T) -> int = """
    if type(v) ~= "table" then return -1 end
    if v._data == nil then return -2 end
    return #v._data
"""

fn main() {
    print("{opaque_t(7)}")
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

/// The rejection an opaque body over a column vec now produces.
fn rejection(src: &str) -> String {
    crate::common::try_emit_module(src)
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

/// THE discriminant. Its rejecting identically keeps the defect attributed to
/// the opaque boundary rather than to instance discovery.
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

/// The call's binding, not the caller's. Without it the body is ADMITTED and
/// the guarded form silently answers `-1`.
#[test]
fn a_generic_param_bound_to_a_column_vec_at_the_call_rejects() {
    let msg = rejection(GENERIC_PARAM_BOUND_AT_CALL);
    assert!(
        msg.contains("column-stored `[P]`") && msg.contains("opaque_t"),
        "a param spelled `T` that binds to `[P]` here must reject, got: {msg}"
    );
}

/// Bounds it on the element: same generic param, boxed element, still correct.
#[test]
fn a_generic_param_bound_to_a_boxed_vec_is_correct() {
    assert_eq!(run(GENERIC_PARAM_BOXED_ELEM, "inline_lua_gen_boxed"), "3");
}

/// Bounds it on the constructor: a generic bound to a scalar names no
/// container, so the rule must not fire.
#[test]
fn a_generic_param_bound_to_a_scalar_is_admitted() {
    assert_eq!(run(GENERIC_PARAM_SCALAR, "inline_lua_gen_scalar"), "-1");
}
