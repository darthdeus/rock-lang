use crate::common::{emit_module, function_body, run_main};

const VALUE_FIELD_LOOP: &str = r#"
value FixVec2 { x: float, y: float }
value IVec2 { x: int, y: int }

@pure fn fv(x: float, y: float) -> FixVec2 { return FixVec2 { x: x, y: y } }
@pure fn iv(x: int, y: int) -> IVec2 { return IVec2 { x: x, y: y } }
@pure fn tile_center(g: IVec2) -> FixVec2 {
    return fv(((g.x + g.y) as float), ((g.x - g.y) as float))
}

fn hot(w: int, h: int) -> float {
    let acc = 0.0
    for gy in 0..h {
        for gx in 0..w {
            let pos = tile_center(iv(gx, gy))
            let kind = ((gx * 7) + (gy * 13)) % 3
            if kind == 0 {
                acc = (acc + (pos.x * 2.0)) - pos.y
            } else if kind == 1 {
                acc = (acc - pos.y) + pos.x
            } else {
                acc = (acc + pos.x) + pos.y
            }
        }
    }
    return acc
}

fn main() {
    let _ = hot(2, 2)
}
"#;

const VALUE_FIELD_AND_WHOLE_USE: &str = r#"
value Color { r: float, g: float, b: float, a: float }
value FixVec3 { x: float, y: float, z: float }
@nomangle inline(lua) fn __same(a: Color, b: Color) -> bool = "return a.r == b.r and a.g == b.g and a.b == b.b and a.a == b.a"

// `c` is field-read into a local (`let r = c.r`) AND used as a whole struct
// (`__same(c, c2)`; `==` compares leaves). The whole-struct use escapes, so the
// cell must NOT be split and the `Color_ct` cdata must be KEPT for the call. The field read `c.r` itself folds to
// its construction operand `k` (a `value` struct is IMMUTABLE, so `Field(Color_ct,
// r)` == the `r` operand -- the copy-prop forwarding), independent of the kept
// cdata. The half-rewrite hazard this pins: forwarding the read to a never-stored
// per-field CELL (an uninitialized read) -- the runtime check below catches that.
//
// Field values depend on the param `k` so the ctors are NOT all-constant -- this
// isolates opt #1 (SROA keeps the cdata) from opt #7 (const hoisting would lift
// an all-constant ctor to a module-scope global, also leaving the body with no
// `Color_ct(`; covered separately in `const_hoist.rs`).
fn coexist(k: float) -> float {
    let c = Color { r: k, g: 0.5, b: 0.0, a: 1.0 }
    let v = FixVec3 { x: k, y: 20.0, z: 30.0 }
    let r = c.r
    let z = v.z
    let c2 = Color { r: k, g: 0.5, b: 0.0, a: 1.0 }
    let eq = if __same(c, c2) { 1.0 } else { 0.0 }
    return (r + z) + eq
}

@nomangle inline(lua) fn __out(x: float) = "io.write(tostring(x))"
fn main() {
    __out(coexist(2.0))   // 2.0 (c.r) + 30.0 (v.z) + 1.0 (same c c2) = 33
}
"#;

#[test]
fn value_struct_with_whole_use_keeps_cdata() {
    let lua = emit_module(VALUE_FIELD_AND_WHOLE_USE);
    let body = function_body(&lua, "coexist");
    // The whole-struct argument escapes, so the cdata must be KEPT (not SROA'd).
    assert!(body.contains("Color_ct("));
    // The separate field read `c.r` folds to its construction operand (no `.r`
    // projection needed), since the value struct is immutable.
    assert!(!body.contains(".r"));
    // Runtime guard against a BAD fold (e.g. an uninitialized-cell read): the value
    // must be exactly 33.

    assert_eq!(run_main(&lua), "33");
}

#[test]
fn value_struct_field_reads_dont_materialize_cdata() {
    let lua = emit_module(VALUE_FIELD_LOOP);
    let hot = function_body(&lua, "hot");
    // The inlined value structs are consumed only by field reads, so neither
    // ctor should survive in the hot loop -- SROA splits the cells into scalars.
    assert!(!hot.contains("FixVec2_ct("));
    assert!(!hot.contains("IVec2_ct("));
}

const NATIVE_BUFFER_VALUE_STORE: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

@pure fn mk(x: float) -> Vert { return Vert { pos: V2 { x: x, y: x }, uv: V2 { x: x, y: x }, w: x } }

fn fill(buf: NativeBuffer<Vert>, n: int) {
    for i in 0..n {
        buf[i] = mk(i as float)
    }
}

fn main() {
    let buf = nb_vert(4)
    fill(buf, 4)
}
"#;

#[test]
fn native_buffer_value_store_writes_leaves_through_views() {
    let lua = emit_module(NATIVE_BUFFER_VALUE_STORE);
    let body = function_body(&lua, "fill");
    // Leaf form: the element is bounds-checked ONCE by index, then each leaf is
    // written through its own view -- no slot reference is taken at all.
    assert!(body.contains("__nb_checked_index("));
    assert!(!body.contains("__nb_checked_get("));
    // NOT the packed form (build the whole cdata and copy it in).
    assert!(!body.contains("__nb_checked_set("));
    // No ctor survives -- neither the element's nor a nested field's.
    for ct in ["Vert_ct(", "V2_ct("] {
        assert!(!body.contains(ct));
    }
}

const GENERIC_VALUE_SCALARIZE: &str = r#"
value Pair<T> { a: T, b: T }
fn pair_sum(p: Pair<int>) -> int { return p.a + p.b }
fn mk(x: int, y: int) -> Pair<int> { return Pair { a: x, b: y } }
fn drive(x: int, y: int) -> int {
    let p = mk(x, y)
    return pair_sum(p)
}
fn main() { let _ = drive(3, 4) }
"#;

#[test]
fn generic_value_struct_rides_scalar_abi() {
    let lua = emit_module(GENERIC_VALUE_SCALARIZE);
    let sum = function_body(&lua, "pair_sum");
    // Scalarized params: two leaf args, not a single boxed table.
    assert!(!sum.contains("Pair_vt") && !sum.contains("Pair_ct"));
    let drive = function_body(&lua, "drive");
    assert!(!drive.contains("Pair_vt") && !drive.contains("Pair_ct"));
}

const NESTED_SCALAR_ABI: &str = r#"
value V2 { x: float, y: float }
value Rect { tl: V2, br: V2 }

@scalar_abi inline(lua) fn nested_leaves(r: Rect) -> float = """
    return r__tl__x + r__br__y
"""

fn main() { let _ = nested_leaves(Rect { tl: V2 { x: 1.0, y: 2.0 }, br: V2 { x: 4.0, y: 8.0 } }) }
"#;

#[test]
fn nested_scalar_abi_leaf_params_are_injective() {
    let lua = emit_module(NESTED_SCALAR_ABI);
    let f = function_body(&lua, "nested_leaves");
    let params = f
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| panic!("no param list in:\n{f}"));
    let names: Vec<&str> = params
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut uniq = names.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), names.len());
}

const UNDERSCORE_FIELD_NO_COLLISION: &str = r#"
nobox value V2 { x: float, y: float }
nobox value Ok { tl__z: float, tl: V2 }

@scalar_abi inline(lua) fn fine(r: Ok) -> float = """
    return r__tl__z + r__tl__x * 10.0 + r__tl__y * 100.0
"""

fn apply(f: (Ok) -> float, r: Ok) -> float { return f(r) }

@nomangle inline(lua) fn __out(x: float) = "io.write(tostring(x))"
fn main() {
    let r = Ok { tl__z: 7.0, tl: V2 { x: 1.0, y: 2.0 } }
    __out(apply(fine, r))
}
"#;

const UNMARKED_VALUE_VIA_NULLABLE_FN_VALUE: &str = r#"
value V2 { x: float, y: float }
value Box2 { tl: V2, w: float }

@scalar_abi inline(lua) fn probe(r: Box2) -> float = """
    return r__tl__x + r__tl__y * 10.0 + r__w * 100.0
"""

fn apply(f: ((Box2) -> float)?, r: Box2) -> float {
    if f != null { return f(r) }
    return -1.0
}

fn main() {
    let r = Box2 { tl: V2 { x: 1.0, y: 2.0 }, w: 3.0 }
    print("{apply(probe, r)}")
}
"#;

const UNMARKED_VALUE_VIA_FN_VALUE: &str = r#"
value V2 { x: float, y: float }
value Box2 { tl: V2, w: float }

@scalar_abi inline(lua) fn probe(r: Box2) -> float = """
    return r__tl__x + r__tl__y * 10.0 + r__w * 100.0
"""

fn apply(f: (Box2) -> float, r: Box2) -> float { return f(r) }

fn main() {
    let r = Box2 { tl: V2 { x: 1.0, y: 2.0 }, w: 3.0 }
    print("{apply(probe, r)}")
}
"#;

#[test]
fn unmarked_value_through_fn_value_rejects() {
    let err = crate::common::try_emit_module(UNMARKED_VALUE_VIA_FN_VALUE)
        .expect_err("an unreconcilable fn-value ABI must reject");
    assert!(err.message.contains("cannot be taken as a fn value") && err.message.contains("probe"));
}

#[test]
fn unmarked_value_through_a_nullable_fn_value_still_rejects() {
    let err = crate::common::try_emit_module(UNMARKED_VALUE_VIA_NULLABLE_FN_VALUE)
        .expect_err("a nullable fn-value reference must not launder an unreconcilable ABI");
    assert!(err.message.contains("cannot be taken as a fn value") && err.message.contains("probe"));
}

#[test]
fn non_colliding_underscore_field_still_scalarizes() {
    let lua = emit_module(UNDERSCORE_FIELD_NO_COLLISION);
    let f = function_body(&lua, "fine");
    for leaf in ["r__tl__z", "r__tl__x", "r__tl__y"] {
        assert!(f.contains(leaf));
    }
    assert!(!f.contains("r__tl__z, r__tl__z"));
    assert_eq!(crate::common::run_main(&lua), "217");
}

const LEAF_FORM_SAVED_IN_CLOSURE: &str = r#"
global ACTIVE: bool = true
global VIEW: Mat4 = mat4_identity()
fn reset() {
    ACTIVE = false
    VIEW = mat4_identity()
}
fn with_fn(f: fn() -> ()) { f() }
fn with_saved(draw: fn() -> ()) {
    with_fn(() => {
        let was = ACTIVE
        let saved = VIEW
        reset()
        draw()
        ACTIVE = was
        VIEW = saved
    })
}
fn main() {
    with_saved(() => { VIEW = mat4_identity() })
    print("{ACTIVE} {VIEW.c3.w}")
}
"#;

#[test]
fn leaf_form_value_copy_in_a_closure_builds_one_box() {
    let lua = emit_module(LEAF_FORM_SAVED_IN_CLOSURE);
    let body = function_body(&lua, "with_saved_fn_ret_unit__lambda0");
    assert_eq!(
        body.matches("Mat4_lct(").count() + body.matches("Mat4_ct(").count(),
        1
    );
    assert_eq!(run_main(&lua), "true 1");
}

const ENUM_CHILD_OF_A_COPIED_VALUE: &str = r#"
enum QKind { Gold, Key(string), Bandage }
enum QProp { Bound }
value QBook { pages: [int] = [] }
value QItem {
    id: int = 0,
    kind: QKind,
    amount: int = 1,
    property: QProp? = null,
    book: QBook? = null,
}
value QIdent { item: QItem, next: int }
value QInv { stacks: [QItem] = [] }
fn ident(item: QItem, next: int) -> QIdent { return QIdent { item: QItem { ..item, id: next }, next: next + 1 } }
fn set_count(inv: QInv, kind: QKind, amount: int, next: int) -> int {
    let old = inv.stacks[0]
    let item = if amount > 0 { QItem { ..old, kind, amount } } else { QItem { kind, amount: 0 } }
    let identified = if amount > 0 { ident(item, next) } else { QIdent { item, next } }
    inv.stacks[0] = identified.item
    return identified.next
}
fn main() {
    let inv = QInv { stacks: [QItem { kind: QKind::Key("a"), amount: 1 }] }
    let n = set_count(inv, QKind::Key("b"), 3, 7)
    let m = set_count(inv, QKind::Gold, 0, n)
    let k = match inv.stacks[0].kind { QKind::Gold => "gold", QKind::Key(s) => s, QKind::Bandage => "b" }
    print("{n} {m} {inv.stacks[0].amount} {inv.stacks[0].id} {k}")
}
"#;

#[test]
fn a_value_copy_shares_its_enum_children() {
    let lua = emit_module(ENUM_CHILD_OF_A_COPIED_VALUE);
    let body = function_body(&lua, "set_count");
    // `let item` keeps its join as leaves, so the one `kind` box is the
    // stored item's; the copies of `item` add none.
    assert_eq!(body.matches("__ty = \"QKind\"").count(), 1);
    assert_eq!(run_main(&lua), "8 8 0 0 gold");
}

const RECURSIVE_VALUE_COPY: &str = r#"
value Chain { n: int, next: Chain?, xs: [int] }
struct ChainHolder { c: Chain }
fn main() {
    let c = Chain { n: 0, next: Chain { n: 1, next: null, xs: [] }, xs: [] }
    let a = ChainHolder { c: c }
    let b = ChainHolder { c: c }
    if a.c.next != null { a.c.next.n = 99 }
    if b.c.next != null { print(b.c.next.n) }
}
"#;

const RECURSIVE_VALUE_READ: &str = r#"
value Chain { n: int, next: Chain?, xs: [int] }
struct ChainHolder { c: Chain }
fn main() {
    let h = ChainHolder { c: Chain { n: 0, next: Chain { n: 1, next: null, xs: [] }, xs: [] } }
    let c = h.c
    print(c.next?.n ?? -1)
}
"#;

#[test]
fn a_recursive_value_rejects_at_definition_whatever_its_use() {
    for (_use_kind, src) in [
        ("copy", RECURSIVE_VALUE_COPY),
        ("read", RECURSIVE_VALUE_READ),
    ] {
        let errs = crate::common::bone_rejections(src);
        assert_eq!(
            errs,
            ["recursive value type: Chain -> Chain; use a reference or handle"]
        );
    }
}
