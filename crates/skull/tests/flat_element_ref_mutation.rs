use crate::common::try_emit_module;

#[test]
fn recursive_ref_return_of_differently_stored_ref_rejects() {
    let source = r#"
value Cell { v: int }
struct Holder { cell: Cell }
fn pick(c: ref Cell, h: ref Holder, n: int) -> ref Cell {
    if n == 0 { return c }
    return pick(h.cell, h, n - 1)
}
fn bump(c: ref Cell) -> int { c.v += 1
return c.v }
fn main() {
    let xs: [Cell] = []
    xs.push(Cell { v: 10 })
    let h = Holder { cell: Cell { v: 1 } }
    print("{bump(pick(xs[0], h, 2))}")
}
"#;
    let err = try_emit_module(source).expect_err("a flat and a boxed `ref` return must not emit");
    assert!(err.message.contains("`pick`") && err.message.contains("differently stored reference"));
    assert!(!err.labels.is_empty());
}

#[test]
fn rebinding_a_ref_across_storage_shapes_rejects() {
    let cases = [
        ("flat then boxed", "rebind(xs[0], h.cell)"),
        ("boxed then flat", "rebind(h.cell, xs[0])"),
        ("column then nested prefix", "rebind(xs[0], os[0].cell)"),
    ];
    for (label, call) in cases {
        let source = format!(
            r#"
value Cell {{ v: int }}
value Outer {{ cell: Cell, z: int }}
struct Holder {{ cell: Cell }}
fn rebind(a: ref Cell, b: ref Cell) -> int {{
    let r: ref Cell = a
    r = b
    r.v += 1
    return r.v
}}
fn main() {{
    let xs: [Cell] = [Cell {{ v: 1 }}]
    let os: [Outer] = [Outer {{ cell: Cell {{ v: 2 }}, z: 3 }}]
    let h = Holder {{ cell: Cell {{ v: 4 }} }}
    print("{{{call}}}")
}}
"#
        );
        let err = try_emit_module(&source).expect_err(label);
        assert!(err.message.contains("cannot reassign"));
        assert!(!err.labels.is_empty());
    }
}
