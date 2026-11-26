use crate::common::{emit_closure, ws_with};

// `tuple_first`/`tuple_second` project one element of the array, so a result
// whose lanes cannot travel through the return bank must reject on JS rather
// than read lanes past the first as undefined.
#[test]
fn a_tuple_intrinsic_rejects_lanes_that_cannot_use_the_return_bank() {
    for src in [
        // nullable tuple result
        "nobox value NP { x: int, y: int }\n\
         @intrinsic inline(lua) fn tuple_first(tuple: (((NP, int)?, int), int)) -> ((NP, int)?, int) = \"return tuple[1]\"\n\
         fn root(input: (((NP, int)?, int), int)) -> int {\n\
             let result = tuple_first(input)\n\
             return result.1\n\
         }",
        // dotted nobox leaf paths
        "nobox value Inner { x: int, y: int }\n\
         nobox value Outer { inner: Inner, tag: int }\n\
         @intrinsic inline(lua) fn tuple_first(tuple: ((int, Outer), int)) -> (int, Outer) = \"return tuple[1]\"\n\
         fn root() -> int {\n\
             let nested = [((1, Outer { inner: Inner { x: 2, y: 3 }, tag: 4 }), 0)]\n\
             let result = tuple_first(nested[0])\n\
             return tuple_first(result) + tuple_second(result).inner.x\n\
         }",
    ] {
        let err =
            emit_closure(&ws_with(src), "root").expect_err("lanes cannot use the return bank");
        assert!(
            err.contains("tuple_first"),
            "diagnostic must name its callee: {err}"
        );
    }
}

#[test]
fn decimal_formatting_preserves_half_even_ties_and_notation_boundaries() {
    let output = crate::common::run_js(
        &[skull_codegen_js::JS_STRING_RUNTIME],
        r#"
console.log([
  __c_f(1.125, 2), __c_f(1.1250000000000002, 2), __c_f(3.5, 0),
  __c_f(-0, 2), __c_f(1e21, 0),
  __c_g(-0, 14), __c_g(99999999999999, 14), __c_g(100000000000000, 14),
  __c_g(3, 3, true)
].join("|"));
"#,
    );
    assert_eq!(
        output.trim(),
        "1.12|1.13|4|-0.00|1000000000000000000000|-0|99999999999999|1e+14|3.00"
    );
}
