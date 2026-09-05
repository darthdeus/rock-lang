// RED premise: `a == b` on two operands of the SAME declared `value` struct
// routed through the generic stdlib `__op_eq`/`__op_ne` walker even when the
// struct carries no `nobox` annotation, because every `value` struct (cdata,
// GC-carrying table, or leaf-table tier) already gets an unconditional
// per-type `__eq` (`value_struct_metatypes` / `value_struct_gc_defs` in
// `skull/src/lib.rs`) that native `==` reaches directly. `eq_route`
// (`skull-mir/src/lower/dispatch.rs`) now recognizes same-named `value`
// struct operands and routes to `EqRoute::Primitive`.
use crate::common::{emit_module, function_body, run_luajit};

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
    if std::process::Command::new("luajit").arg("-v").output().is_err() {
        eprintln!("luajit not found; skipping execution check");
        return;
    }
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
    if std::process::Command::new("luajit").arg("-v").output().is_err() {
        eprintln!("luajit not found; skipping execution check");
        return;
    }
    let mut lua = emit_module(SEMANTIC_CORRECTNESS);
    lua.push_str(
        "\nprint(eq_scalar(1.0, 2.0, 1.0, 2.0), eq_scalar(1.0, 2.0, 1.0, 3.0), ne_scalar(1.0, 2.0, 1.0, 3.0), ne_scalar(1.0, 2.0, 1.0, 2.0), eq_gc_same_ref(5), ne_gc_different_tag(5, 6))\n",
    );
    let out = run_luajit(&lua, "value_struct_eq_semantics");
    assert_eq!(out.trim(), "true\tfalse\ttrue\tfalse\ttrue\ttrue", "semantic mismatch:\n{out}");
}
