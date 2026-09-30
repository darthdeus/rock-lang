//! On Lua, `borrow` of a `value` element that `Arena<T>` stores as flat columns
//! is rejected at the call: the element has no table a `ref` can alias, and the
//! copy it used to return silently lost every write through it. A known Lua gap,
//! not policy: C runs this program and prints "6 2". Fix options: "Lua has no
//! `ref` to a flat-stored container element" in docs/unfuck-backlog.md.

use crate::common::try_emit_module;

const SRC: &str = r#"
value Inner { v: int }
value Cell { inner: Inner, w: int }
fn bump(c: ref Cell) { c.w += 1 }
fn bump2(c: ref Inner) { c.v += 1 }
fn main() {
    let a: Arena<Cell> = arena_new()
    let e = a.insert(Cell { inner: Inner { v: 1 }, w: 5 })
    let c = a.borrow(e)
    if c != null {
        bump(c)
        bump2(c.inner)
    }
    let g = a.get(e) ?? Cell { inner: Inner { v: 0 }, w: 0 }
    print("{g.w} {g.inner.v}")
}
"#;

#[test]
fn arena_borrow_of_column_value_element_is_rejected_at_the_call_on_lua() {
    let err = try_emit_module(SRC).expect_err("a borrow of a column-stored element must not emit");
    assert!(
        err.message.contains("the Lua backend does not yet support `borrow`"),
        "not the known-gap rejection:\n{}",
        err.message
    );
    for error in std::iter::once(&err).chain(&err.siblings) {
        assert_eq!(error.location.file.as_str(), "<test>", "not spanned at the call: {error:?}");
    }
    assert_eq!(
        Some(err.location.offset),
        SRC.find("a.borrow(e)"),
        "not spanned at `a.borrow(e)`: {err:?}"
    );
}
