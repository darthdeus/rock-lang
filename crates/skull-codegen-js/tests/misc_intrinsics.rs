//! `tuple_first`/`tuple_second` project one element of the array, so a result
//! whose lanes cannot travel through the return bank must reject on JS rather
//! than read lanes past the first as undefined.

use crate::common::ws_with;

#[test]
fn a_tuple_intrinsic_rejects_an_unprojectable_nullable_tuple_result() {
    let src = "nobox value NP { x: int, y: int }\n\
               fn root(input: (((NP, int)?, int), int)) -> int {\n\
                   let result = tuple_first(input)\n\
                   return result.1\n\
               }";
    let ws = ws_with(src);
    let err = crate::common::emit_closure(&ws, "root")
        .expect_err("nullable tuple lanes cannot use the return bank");
    assert!(
        err.contains("tuple_first"),
        "the unprojectable intrinsic diagnostic must name its callee: {err}"
    );
}

#[test]
fn a_tuple_intrinsic_rejects_dotted_nobox_leaf_paths() {
    let src = "nobox value Inner { x: int, y: int }\n\
               nobox value Outer { inner: Inner, tag: int }\n\
               @intrinsic inline(lua) fn tuple_first(tuple: ((int, Outer), int)) -> (int, Outer) = \"return tuple[1]\"\n\
               fn root() -> int {\n\
                   let nested = [((1, Outer { inner: Inner { x: 2, y: 3 }, tag: 4 }), 0)]\n\
                   let result = tuple_first(nested[0])\n\
                   return tuple_first(result) + tuple_second(result).inner.x\n\
               }";
    let ws = ws_with(src);
    let err = crate::common::emit_closure(&ws, "root")
        .expect_err("dotted leaf lanes cannot use the return bank");
    assert!(
        err.contains("tuple_first"),
        "the unprojectable intrinsic diagnostic must name its callee: {err}"
    );
}
