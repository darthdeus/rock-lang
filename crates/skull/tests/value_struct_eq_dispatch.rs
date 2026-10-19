// RED premise: `a == b` on two operands of the SAME declared `value` struct
// routed through the generic stdlib `__op_eq`/`__op_ne` walker even when the
// struct carries no `nobox` annotation, because every `value` struct (cdata,
// GC-carrying table, or leaf-table tier) already gets an unconditional
// per-type `__eq` (`value_struct_metatypes` / `value_struct_gc_defs` in
// `skull/src/lib.rs`) that native `==` reaches directly. `eq_route`
// (`skull-mir/src/lower/dispatch.rs`) now recognizes same-named `value`
// struct operands and routes to `EqRoute::Primitive`.
use crate::common::{emit_module, function_body, require_luajit, run_luajit};

const HANDLE_EQUALITY_FIXTURE: &str = r#"
struct EqTag { n: int }

fn entity_matches(xs: [Entity<EqTag>], target: Entity<EqTag>) -> int {
    let matches = 0
    for e in xs {
        if e == target { matches += 1 }
        if e != target { matches += 2 }
    }
    return matches
}
"#;

#[test]
fn vector_handle_equality_does_not_materialize_carriers() {
    let lua = emit_module(HANDLE_EQUALITY_FIXTURE);
    let body = function_body(&lua, "entity_matches");
    assert!(
        body.contains("==") && (body.contains("~=") || body.contains("not")),
        "missing comparisons:\n{body}"
    );
    assert!(
        !body.contains("Entity_vt("),
        "vector handle comparisons must read identity leaves:\n{body}"
    );
}

const SCALAR_VALUE: &str = r#"
value V2 { x: float, y: float }

fn eq_v2(a: V2, b: V2) -> bool { return a == b }
fn ne_v2(a: V2, b: V2) -> bool { return a != b }
"#;

#[test]
fn plain_value_struct_equality_skips_the_structural_walk() {
    let lua = emit_module(SCALAR_VALUE);
    for name in ["eq_v2", "ne_v2"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("__op_eq") && !body.contains("__op_ne"),
            "`{name}` still called structural equality:\n{body}"
        );
    }
}

const GC_CARRYING_VALUE: &str = r#"
value Wrap { tag: int, name: string }

fn eq_wrap(a: Wrap, b: Wrap) -> bool { return a == b }
"#;

#[test]
fn gc_carrying_value_struct_equality_also_skips_the_structural_walk() {
    let lua = emit_module(GC_CARRYING_VALUE);
    let body = function_body(&lua, "eq_wrap");
    assert!(
        !body.contains("__op_eq") && !body.contains("__op_ne"),
        "GC-carrying value struct equality still called structural equality:\n{body}"
    );
}

const NULLABLE_VALUE: &str = r#"
value V2 { x: float, y: float }

fn eq_maybe(a: V2?, b: V2?) -> bool { return a == b }
"#;

#[test]
fn nullable_value_struct_equality_also_skips_the_structural_walk() {
    let lua = emit_module(NULLABLE_VALUE);
    let body = function_body(&lua, "eq_maybe");
    assert!(
        !body.contains("__op_eq") && !body.contains("__op_ne"),
        "nullable value struct equality still called structural equality:\n{body}"
    );
}

// Controls: these must NOT be rerouted -- they have no per-type native `==`
// authority (a plain struct has no metatable; an `any` field forces the
// dynamic walk).
const PLAIN_STRUCT_CONTROL: &str = r#"
struct Plain { x: int, y: int }

fn eq_plain(a: Plain, b: Plain) -> bool { return a == b }
"#;

#[test]
fn plain_struct_equality_still_uses_the_structural_walk() {
    let lua = emit_module(PLAIN_STRUCT_CONTROL);
    let body = function_body(&lua, "eq_plain");
    assert!(
        body.contains("__op_eq"),
        "plain struct equality must stay on the structural walk:\n{body}"
    );
}

const VEC_CONTROL: &str = r#"
fn eq_vec(a: [int], b: [int]) -> bool { return a == b }
"#;

#[test]
fn vec_equality_still_uses_the_structural_walk() {
    let lua = emit_module(VEC_CONTROL);
    let body = function_body(&lua, "eq_vec");
    assert!(
        body.contains("__op_eq"),
        "Vec equality must stay on the structural walk:\n{body}"
    );
}

const SINGLE_EVAL_CONTROL: &str = r#"
value V2 { x: float, y: float }

@nomangle inline(lua) fn note(msg: string) = "print(msg)"

fn make(tag: string, x: float, y: float) -> V2 {
    note(tag)
    return V2 { x: x, y: y }
}

@nomangle fn eq_effectful() -> bool {
    return make("lhs", 1.0, 2.0) == make("rhs", 1.0, 2.0)
}
"#;

#[test]
fn value_struct_equality_evaluates_each_operand_exactly_once() {
    require_luajit();
    let mut lua = emit_module(SINGLE_EVAL_CONTROL);
    lua.push_str("\nprint(eq_effectful())\n");
    let out = run_luajit(&lua, "value_struct_eq_single_eval");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["lhs", "rhs", "true"], "unexpected evaluation order/result:\n{out}");
}

const SEMANTIC_CORRECTNESS: &str = r#"
value V2 { x: float, y: float }
value Wrap { tag: int, name: string }

@nomangle fn eq_scalar(ax: float, ay: float, bx: float, by: float) -> bool {
    return V2 { x: ax, y: ay } == V2 { x: bx, y: by }
}

@nomangle fn ne_scalar(ax: float, ay: float, bx: float, by: float) -> bool {
    return V2 { x: ax, y: ay } != V2 { x: bx, y: by }
}

@nomangle fn eq_gc_same_ref(tag: int) -> bool {
    let n = "shared"
    return Wrap { tag: tag, name: n } == Wrap { tag: tag, name: n }
}

@nomangle fn ne_gc_different_tag(tag_a: int, tag_b: int) -> bool {
    let n = "shared"
    return Wrap { tag: tag_a, name: n } != Wrap { tag: tag_b, name: n }
}
"#;

#[test]
fn value_struct_equality_preserves_semantics() {
    require_luajit();
    let mut lua = emit_module(SEMANTIC_CORRECTNESS);
    lua.push_str(
        "\nprint(eq_scalar(1.0, 2.0, 1.0, 2.0), eq_scalar(1.0, 2.0, 1.0, 3.0), ne_scalar(1.0, 2.0, 1.0, 3.0), ne_scalar(1.0, 2.0, 1.0, 2.0), eq_gc_same_ref(5), ne_gc_different_tag(5, 6))\n",
    );
    let out = run_luajit(&lua, "value_struct_eq_semantics");
    assert_eq!(out.trim(), "true\tfalse\ttrue\tfalse\ttrue\ttrue", "semantic mismatch:\n{out}");
}

// Scalar `value` operands arrive as leaf cells. Comparing them must stay
// leaf-wise: packing both sides into `*_vt` tables to reach `__eq` allocated
// two tables per comparison (application's per-frame `Task.state == ...` checks on
// `Entity<T>`, which is generic and so never answers `nobox_value_name`).
const SCALAR_LEAF_EQUALITY: &str = r#"
value V2 { x: float, y: float }
value Handle<T> { slot: int, generation: int }

fn eq_v2(a: V2, b: V2) -> bool { return a == b }
fn ne_v2(a: V2, b: V2) -> bool { return a != b }
fn eq_handle(a: Handle<int>, b: Handle<int>) -> bool { return a == b }
fn ne_handle(a: Handle<int>, b: Handle<int>) -> bool { return a != b }

value Tagged { tag: int, handle: Handle<int> }
struct Slot { last: Tagged? = null }

fn stored_matches(slot: Slot, x: Handle<int>) -> bool {
    if let last = slot.last { return last.handle == x }
    return false
}
"#;

#[test]
fn scalar_value_equality_compares_leaves_without_materializing() {
    let lua = emit_module(SCALAR_LEAF_EQUALITY);
    for name in ["eq_v2", "ne_v2", "eq_handle", "ne_handle", "stored_matches"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("_vt(") && !body.contains("__op_eq") && !body.contains("__op_ne"),
            "`{name}` materialized its operands to compare them:\n{body}"
        );
    }
}

const SCALAR_LEAF_SEMANTICS: &str = r#"
value Handle<T> { slot: int, generation: int }

fn same(x: Handle<int>, y: Handle<int>) -> bool { return x == y }
fn differ(x: Handle<int>, y: Handle<int>) -> bool { return x != y }

@nomangle fn handle_eq(a: int, b: int, c: int, d: int) -> bool {
    let x: Handle<int> = Handle { slot: a, generation: b }
    return same(x, Handle { slot: c, generation: d })
}

@nomangle fn handle_ne(a: int, b: int, c: int, d: int) -> bool {
    let x: Handle<int> = Handle { slot: a, generation: b }
    return differ(x, Handle { slot: c, generation: d })
}
"#;

#[test]
fn scalar_value_leaf_equality_keeps_field_semantics() {
    require_luajit();
    let mut lua = emit_module(SCALAR_LEAF_SEMANTICS);
    lua.push_str(
        "\nprint(handle_eq(1, 2, 1, 2), handle_eq(1, 2, 1, 3), handle_eq(1, 2, 4, 2))\
         \nprint(handle_ne(1, 2, 1, 2), handle_ne(1, 2, 1, 3))\n",
    );
    let out = run_luajit(&lua, "value_struct_eq_leaf_semantics");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["true\tfalse\tfalse", "false\ttrue"], "unexpected results:\n{out}");
}

// A C-layout `value` stores `float` leaves as single precision, so leaves read
// back from storage are rounded while a scalarized operand's leaves are not.
// Equality must still see a stored `Tint { r: 0.4 }` equal to a fresh one.
const STORED_FLOAT_VALUE: &str = r#"
value Tint { r: float, g: float, n: int }
struct Holder { tint: Tint? = null }

fn same(h: Holder, t: Tint) -> bool { return h.tint.unwrap() == t }
fn differ(h: Holder, t: Tint) -> bool { return h.tint.unwrap() != t }

@nomangle fn stored_float_eq(r: float, g: float) -> bool {
    let h = Holder {}
    h.tint = Tint { r: r, g: g, n: 3 }
    return same(h, Tint { r: r, g: g, n: 3 })
}

@nomangle fn stored_float_ne(r: float, g: float) -> bool {
    let h = Holder {}
    h.tint = Tint { r: r, g: g, n: 3 }
    return differ(h, Tint { r: r, g: g, n: 3 })
}
"#;

#[test]
fn stored_float_value_equality_matches_a_fresh_operand() {
    require_luajit();
    let mut lua = emit_module(STORED_FLOAT_VALUE);
    lua.push_str("\nprint(stored_float_eq(0.4, 0.1), stored_float_ne(0.4, 0.1), stored_float_eq(0.5, 0.25))\n");
    let out = run_luajit(&lua, "value_struct_eq_stored_float");
    assert_eq!(out.trim(), "true\tfalse\ttrue", "stored float value equality:\n{out}");
}

#[test]
fn nullable_boxed_value_fields_compare_contents_in_enclosing_values() {
    require_luajit();
    let mut lua = emit_module(r#"
value Child { name: string, amount: int }
value Parent { name: string, child: Child? = null }
struct Holder { row: Parent }
fn same(a: Holder, b: Holder) -> bool { return a.row == b.row }

struct Reference { name: string }
value ReferenceParent { child: Reference? = null }
struct ReferenceHolder { row: ReferenceParent }
fn same_reference(a: ReferenceHolder, b: ReferenceHolder) -> bool { return a.row == b.row }
"#);
    lua.push_str(r#"
local function row(child) return { row = Parent_vt("parent", child) } end
local a = row(Child_vt("child", 3))
local b = row(Child_vt("child", 3))
local c = row(Child_vt("child", 4))
local absent = row(nil)
local reference = { name = "same" }
print(a.row == b.row, a.row == c.row, absent.row == row(nil).row, a.row == absent.row,
  ReferenceParent_vt(reference) == ReferenceParent_vt(reference),
  ReferenceParent_vt(reference) == ReferenceParent_vt({name="same"}))
"#);
    let out = run_luajit(&lua, "nullable_boxed_value_field_equality");
    assert_eq!(out.trim(), "true\tfalse\ttrue\tfalse\ttrue\tfalse",
        "nullable value fields compare contents; nullable reference fields retain identity:\n{out}");
}
