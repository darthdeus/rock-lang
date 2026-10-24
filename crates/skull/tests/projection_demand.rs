use crate::common::{emit_module, run_luajit};

#[test]
fn immediate_projections_preserve_nullable_snapshots_and_evaluation_order() {
    let source = r#"
nobox value Point { x: int, y: int }
nobox value Branch { spare: int, child: Point? }
value PlainPoint { x: int, y: int }
value Box { child: PlainPoint?, spare: int }
struct State { branch: Branch, ordinary: Box, calls: int }

fn selected(v: Branch) -> Point? { v.child }
fn nested(v: Branch) -> int { if let p = v.child { return p.y }; return -1 }
fn ordinary(v: Box) -> PlainPoint? { v.child }
fn produced(s: State) -> Branch { s.calls += 1; return s.branch }
fn mutate(s: State) -> int { s.branch.child = Point { x: 30, y: 40 }; return 1 }
fn sum(a: int, b: int) -> int { a + b }
fn ordered(v: Branch) -> int { sum(v.spare, { v = Branch { spare: 99, child: null }; 1 }) }
fn main() {
    let s = State {
        branch: Branch { spare: 7, child: Point { x: 3, y: 5 } },
        ordinary: Box { child: PlainPoint { x: 11, y: 13 }, spare: 17 }, calls: 0 }
    let saved = selected(s.branch)
    let opaque = ordinary(s.ordinary)
    print(nested(s.branch))
    print(ordered(s.branch))
    print(saved?.y ?? -1)
    mutate(s)
    print(nested(s.branch))
    if s.ordinary.child != null { s.ordinary.child.y = 100 }
    print(opaque?.y ?? -1)
    print(produced(s).child?.y ?? -1)
    print(s.calls)
    s.branch.child = null
    print(selected(s.branch) == null)
    print(nested(s.branch))
}

"#;
    let lua = emit_module(source);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "projection_demand"),
        "5\n8\n5\n40\n13\n40\n1\ntrue\n-1"
    );
}

#[test]
fn boxed_wide_projection_preserves_selected_snapshots_and_opaque_children() {
    let fields = (0..50).map(|i| format!("f{i}: int = {i}")).collect::<Vec<_>>().join(", ");
    let source = format!(r#"
value Plain {{ x: int, y: int }}
value Child {{ point: Plain, opaque: Plain? }}
nobox value Flat {{ x: int, y: int }}
value Wide {{ {fields}, child: Child, nullable: Flat? }}
struct Owner {{ wide: Wide, calls: int }}
fn scalar(v: Wide) -> int {{ v.f1 }}
fn nested(v: Wide) -> int {{ v.child.point.y }}
fn selected(v: Wide) -> Child {{ v.child }}
fn nullable(v: Wide) -> Flat? {{ v.nullable }}
fn narrowed(v: Wide) -> int {{ if v.nullable != null {{ return v.nullable.y }}; return -1 }}
fn narrowed_root(v: Wide?) -> int {{ if v != null {{ return v.f1 }}; return -1 }}
fn opaque(v: Wide) -> Plain? {{ v.child.opaque }}
fn produced(o: Owner) -> Wide {{ o.calls += 1; return o.wide }}
fn sum(a: int, b: int) -> int {{ a + b }}
fn ordered(v: Wide) -> int {{ sum(v.f1, {{ v = Wide {{ ..v, f1: 90 }}; 1 }}) }}
fn main() {{
    let o = Owner {{ wide: Wide {{ child: Child {{ point: Plain {{ x: 3, y: 5 }},
        opaque: Plain {{ x: 7, y: 11 }} }}, nullable: Flat {{ x: 13, y: 17 }} }}, calls: 0 }}
    let local = o.wide
    let before = selected(local)
    let opaque_before = opaque(local)
    let nullable_before = nullable(local)
    print(scalar(local))
    print(nested(local))
    print(ordered(local))
    o.wide.child.point.y = 99
    if o.wide.child.opaque != null {{ o.wide.child.opaque.y = 101 }}
    o.wide.nullable = Flat {{ x: 103, y: 107 }}
    print(before.point.y)
    print(before.opaque?.y ?? -1)
    print(opaque_before?.y ?? -1)
    print(nullable_before?.y ?? -1)
    print(narrowed(local))
    print(narrowed_root(local))
    print(narrowed_root(null))
    print(produced(o).f1)
    print(o.calls)
    o.wide.nullable = null
    print(nullable(o.wide) == null)
    print(narrowed(o.wide))
    print(local.f2)
}}
"#);
    let lua = emit_module(&source);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "boxed_projection_demand"),
        "1\n5\n2\n5\n11\n11\n17\n17\n1\n-1\n1\n1\ntrue\n-1\n2"
    );
}
