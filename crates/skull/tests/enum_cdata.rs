use crate::common::{emit_module, function_body, run_luajit};

const UNIT_EQ_SRC: &str = r#"
enum Slice { Sliced, Empty, Leather(int) }
struct Holder { slice: Slice, maybe: Slice? }

@nomangle fn field_is(h: Holder) -> bool { h.slice == Slice::Sliced }
@nomangle fn field_is_not(h: Holder) -> bool { Slice::Empty != h.slice }
@nomangle fn param_is(s: Slice) -> bool { s == Slice::Empty }
@nomangle fn element_is(v: [Slice], i: int) -> bool { v[i] == Slice::Sliced }
@nomangle fn maybe_is(h: Holder) -> bool { h.maybe == Slice::Sliced }
@nomangle fn holder(k: int) -> Holder {
    let s = if k == 0 { Slice::Sliced } else if k == 1 { Slice::Empty } else { Slice::Leather(k) }
    Holder { slice: s, maybe: if k < 0 { null } else { s } }
}
@nomangle fn slices() -> [Slice] { [Slice::Sliced, Slice::Leather(0), Slice::Empty] }
"#;

#[test]
fn equality_with_a_unit_variant_compares_the_tag() {
    let lua = emit_module(UNIT_EQ_SRC);
    for f in ["field_is", "field_is_not", "param_is", "element_is"] {
        let body = function_body(&lua, f);
        assert!(
            !body.contains("__op_eq") && !body.contains("__op_ne"),
            "{f}: {body}"
        );
    }
    let out = run_luajit(&format!(
        r#"{lua}
local h0, h1, h2, hn = holder(0), holder(1), holder(2), holder(-1)
assert(field_is(h0) and not field_is(h1) and not field_is(h2))
assert(field_is_not(h0) and not field_is_not(h1) and field_is_not(h2))
assert(param_is(Slice_ct(1)) and not param_is(Slice_ct(0)) and not param_is(Slice_ct(2, 1)))
local v = slices()
assert(element_is(v, 0) and not element_is(v, 1) and not element_is(v, 2))
assert(maybe_is(h0) and not maybe_is(h1) and not maybe_is(hn))
print("ok")
"#
    ));
    assert_eq!(out, "ok");
}
