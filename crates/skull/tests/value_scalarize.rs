use crate::common::{emit_module, function_body, run_main};

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
    assert_eq!(body.matches("__rock_box_mts[\"QKind\"]").count(), 1);
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
    for src in [RECURSIVE_VALUE_COPY, RECURSIVE_VALUE_READ] {
        assert_eq!(
            crate::common::bone_rejections(src),
            ["recursive value type: Chain -> Chain; use a reference or handle"]
        );
    }
}
