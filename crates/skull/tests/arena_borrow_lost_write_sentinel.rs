//! PINNED BUG: on Lua, `Arena<T>.borrow(e)` over a column-stored arena of a
//! `value` struct hands back a copy of the element, so writes through the
//! returned `ref` (whole element or a nested value field) are silently lost.
//! The C backend writes through and prints "6 2" for this program. Recorded in
//! docs/test-sentinels.md.

use crate::common::{emit_module, run_luajit};

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
fn arena_borrow_of_column_value_element_loses_writes_on_lua() {
    let lua = emit_module(SRC);
    let out = run_luajit(&format!("{lua}\nmain()\n"), "arena_borrow_lost_write");
    // Inverted: this passes BECAUSE the bug exists. Correct output is "6 2"
    // (what C prints). When it prints "6 2", flip this to
    // `assert_eq!(out, "6 2")`, drop PINNED BUG from the module doc, and
    // delete the entry in docs/test-sentinels.md.
    assert_eq!(
        out, "5 1",
        "PINNED BEHAVIOR (BUG): Arena.borrow on Lua returns a copy of a column-stored \
         value element, so both writes are lost. If this now prints \"6 2\", the fix \
         landed -- flip the assertion."
    );
}
