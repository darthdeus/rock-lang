//! SENTINEL -- an `inline(lua)` body that takes a container param receives the
//! COLUMN representation with no adaptation and no rejection. The body's
//! `v._data` is `nil`, so a guarded body silently answers wrong and an unguarded
//! one dies indexing nil.
//!
//! A column-stored `[P]` emits as
//! `{["@vec"] = {repr = "columns", ...}, ["x"] = {}, ["y"] = {}, len = 0}` --
//! per-leaf arrays and no `_data` at all. Every codegen-owned operation on such
//! a vec is ROUTED AROUND the stdlib `inline(lua)` primitives and lowered to
//! direct column stores (which is why `xs.push(..)` works even though
//! `vec_push<T>` is itself an `inline(lua)` writing `_data`; the emitted Lua for
//! a column push contains no call at all). That routing covers the stdlib's own
//! hand-maintained primitive set -- `__vec_data_sort` vs `__rock_col_sort` is
//! the visible seam -- and reaches nothing a user writes.
//!
//! DISCRIMINANT, measured: this is NOT about genericity and NOT about the
//! specialization bail-out. The non-generic control below fails identically, so
//! `compute_call_spec`'s `inline_body.is_some()` arm is not the cause -- an
//! `inline(lua)` body is an opaque string, so nothing can adapt it either way.
//! It is also not about the `nobox` mark: an UNMARKED value struct is
//! column-eligible on the same terms, which is why the unmarked control is here
//! rather than in a nobox-specific file.
//!
//! The bounding control is the ELEMENT type: `[int]` is boxed, keeps `_data`,
//! and answers correctly through the very same body.
//!
//! The correct behavior is a spanned REJECTION at the call, the way a bare `P`
//! at an `inline(lua)` param already rejects (`docs/nobox-case-matrix.md`
//! line 62, F1 erasure). An `inline(lua)` body cannot be given a representation
//! contract, so admitting the call is the bug.
//!
//! PINNED BEHAVIOR (BUG): these assert the wrong answers -- `-1` from the
//! guarded probe and a `_data`-nil death from the unguarded one. When the fix
//! lands they go RED. Replace both with the `Err` assertion the fix produces
//! (the rejection route), and delete this paragraph. If instead the fix teaches
//! the routing to cover user bodies, flip the guarded probe to `1` and the
//! unguarded one to `11`.


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
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

/// `crate::common::run_luajit` asserts a zero exit; this is the tolerant form, for the
/// case whose whole point is that the program dies.
fn run_expect_death(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    let prog = format!("{lua}\nmain()\n");
    let path = std::env::temp_dir().join(format!("skull_test_{}_{tag}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write program");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("spawn luajit");
    let _ = std::fs::remove_file(&path);
    assert!(
        !out.status.success(),
        "PINNED BEHAVIOR (BUG): the unguarded body was expected to DIE indexing \
         a nil `_data`. It exited zero, so the fix landed -- see the header."
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_generic_inline_lua_body_over_a_column_vec_sees_no_data_sentinel() {
    let got = run(GUARDED_GENERIC, "inline_lua_col_generic");
    assert_eq!(
        got, "-1",
        "PINNED BEHAVIOR (BUG): an `inline(lua)` body over a column `[P]` is \
         expected to find `_data` nil and answer `-1`. It answered `{got}`, so \
         the fix landed -- see the header for both correct outcomes."
    );
}

/// THE discriminant. Its failing identically is what proves the defect is the
/// opaque `inline(lua)` boundary and not `compute_call_spec`'s bail-out.
#[test]
fn a_non_generic_inline_lua_body_over_a_column_vec_fails_identically_sentinel() {
    let got = run(GUARDED_CONCRETE, "inline_lua_col_concrete");
    assert_eq!(
        got, "-1",
        "PINNED BEHAVIOR (BUG): the NON-generic body must fail identically -- \
         that is what rules out the specialization bail-out as the cause. It \
         answered `{got}`; if only this one changed, the defect has been split."
    );
}

/// Records the blast radius: this is a column-storage defect, not a `nobox` one.
#[test]
fn an_unmarked_value_element_fails_identically_sentinel() {
    let got = run(GUARDED_UNMARKED, "inline_lua_col_unmarked");
    assert_eq!(
        got, "-1",
        "PINNED BEHAVIOR (BUG): an UNMARKED value element is column-eligible on \
         the same terms and must fail identically. It answered `{got}`."
    );
}

/// Bounds the sentinel on the element type. Without this, a fix that broke every
/// `inline(lua)` container param would leave the sentinels green and read as
/// progress.
#[test]
fn a_boxed_element_through_the_same_body_is_correct() {
    assert_eq!(
        run(GUARDED_INT_ELEM, "inline_lua_col_int"),
        "3",
        "a boxed `[int]` keeps `_data`, so the same body must stay correct"
    );
}

#[test]
fn an_unguarded_inline_lua_body_over_a_column_vec_dies_sentinel() {
    let stderr = run_expect_death(UNGUARDED_GENERIC, "inline_lua_col_unguarded");
    assert!(
        stderr.contains("_data"),
        "PINNED BEHAVIOR (BUG): expected a death indexing the nil `_data`. \
         Got a different failure, which is a separate defect:\n{stderr}"
    );
}
