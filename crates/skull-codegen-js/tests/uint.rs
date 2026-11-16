//! tests/types/uint_semantics.rock runs uint arithmetic, ranges, formatting
//! and checked shifts on both backends; these cover what only a JS host sees.

use crate::common::ws_with;

#[test]
fn uint_entry_values_from_the_host_are_normalized() {
    let js = crate::common::emit_user(&ws_with("fn root(a: uint) -> uint { a }"), "root");
    let out = crate::common::run_node(&format!("{js}\nconsole.log(root(-1));"));
    assert_eq!(out.trim(), "4294967295", "{js}");
}

#[test]
fn native_buffer_uint_uses_unsigned_storage() {
    let src = "
fn root() -> uint {
    let b = native_buffer_uint(1)
    b[0] = 4294967295
    b[0]
}";
    let js = crate::common::emit_user(&ws_with(src), "root");
    assert!(
        js.contains("new Uint32Array"),
        "uint buffer lost its physical type:\n{js}"
    );
}
