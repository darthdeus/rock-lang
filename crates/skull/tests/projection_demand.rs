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
    let fields = (0..50)
        .map(|i| format!("f{i}: int = {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        r#"
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
    let direct_before = o.wide.child
    let opaque_before = opaque(local)
    let nullable_before = nullable(local)
    print(scalar(local))
    print(nested(local))
    print(ordered(local))
    o.wide.child.point.y = 99
    if o.wide.child.opaque != null {{ o.wide.child.opaque.y = 101 }}
    o.wide.nullable = Flat {{ x: 103, y: 107 }}
    print(before.point.y)
    print(direct_before.point.y)
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
"#
    );
    let lua = emit_module(&source);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "boxed_projection_demand"),
        "1\n5\n2\n5\n5\n11\n11\n17\n17\n1\n-1\n1\n1\ntrue\n-1\n2"
    );
}

#[test]
fn global_projections_preserve_snapshots_and_read_order() {
    let source = r#"
value Point { x: int = 3, y: int = 5 }
nobox value Flat { x: int = 7, y: int = 11 }
value Config { child: Point = Point {}, optional: Point? = Point {}, flat: Flat? = Flat {}, spare: int = 13 }
global CONFIG: Config = Config {}
global MAYBE: Config? = Config {}
global FLAT: Flat? = Flat {}
value Generic<T> { child: T, spare: int }
global GENERIC: Generic<Point> = Generic { child: Point {}, spare: 17 }
fn mutate() -> int { CONFIG.child.y = 99; CONFIG.optional = null; CONFIG.flat = null; return 1 }
fn observe(p: Point, ignored: int) -> int { p.y }
fn narrowed() -> int { if MAYBE != null { return MAYBE.child.y }; return -1 }
fn flat() -> int { if FLAT != null { return FLAT.y }; return -1 }
fn main() {
    let generic_child = GENERIC.child
    print(GENERIC.spare)
    print(GENERIC.child.y)
    GENERIC.child.y = 101
    print(generic_child.y)
    let whole = CONFIG
    let child = CONFIG.child
    let optional = CONFIG.optional
    let selected_flat = CONFIG.flat
    print(observe(CONFIG.child, mutate()))
    print(CONFIG.child.y)
    print(whole.child.y)
    print(child.y)
    print(optional?.y ?? -1)
    print(selected_flat?.y ?? -1)
    print(CONFIG.optional == null)
    print(CONFIG.flat == null)
    print(narrowed())
    MAYBE = null
    print(narrowed())
    print(flat())
    FLAT = null
    print(flat())
}
"#;
    let lua = emit_module(source);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "global_projection_demand"),
        "17\n5\n5\n5\n99\n5\n5\n5\n11\ntrue\ntrue\n5\n-1\n11\n-1"
    );
}

#[test]
fn reference_owned_value_projection_reads_only_the_selected_field() {
    let _inline = crate::common::InlineOffGuard::on();
    let fields = (0..50)
        .map(|i| format!("unused_sibling{i}: int = {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let lua = emit_module(&format!(
        r#"
enum Choice {{ Empty, Number(int) }}
value Config {{ chosen: int, choice: Choice, items: [string], {fields} }}
struct Owner {{ config: Config }}
@nomangle fn probe(owner: Owner) -> int {{ owner.config.chosen }}
fn main() {{ print(probe(Owner {{ config: Config {{ chosen: 7, choice: Choice::Empty, items: [] }} }})) }}
"#
    ));
    let probe = lua
        .split("\nfunction probe(")
        .nth(1)
        .unwrap()
        .split("\nend")
        .next()
        .unwrap();
    assert!(
        !probe.contains(".unused_sibling"),
        "an immediate projection must not expand its siblings: {probe}"
    );
    assert_eq!(
        run_luajit(
            &format!(
                "{lua}\nmain(); local a = pcall(probe, nil); local b = pcall(probe, {{config=nil}}); print(a, b)\n"
            ),
            "reference_owned_projection"
        ),
        "7\nfalse\tfalse"
    );
}

#[test]
fn enum_projection_does_not_expand_sibling_fields() {
    let _inline = crate::common::InlineOffGuard::on();
    let lua = emit_module(
        r#"
enum Choice { Empty, Number(int), Text(string) }
value Config { choice: Choice, unused_sibling: int, items: [string] }
struct Owner { config: Config }
@nomangle fn selected(owner: Owner) -> Choice { owner.config.choice }
fn main() {
    let owner = Owner { config: Config { choice: Choice::Number(7), unused_sibling: 11, items: [] } }
    let saved = selected(owner)
    owner.config.choice = Choice::Text("new")
    print(match saved { Choice::Number(n) => n, _ => -1 })
    print(match selected(owner) { Choice::Text(s) => s, _ => "bad" })
}
"#,
    );
    let body = lua
        .split("\nfunction selected(")
        .nth(1)
        .unwrap()
        .split("\nend")
        .next()
        .unwrap();
    assert!(
        !body.contains(".unused_sibling"),
        "selected enum must not read siblings: {body}"
    );
    assert_eq!(
        run_luajit(
            &format!(
                "{lua}\nmain(); local a = pcall(selected, nil); local b = pcall(selected, {{config=nil}}); print(a, b)\n"
            ),
            "selected_enum_projection"
        ),
        "7\nnew\nfalse\tfalse"
    );
}

#[test]
fn enum_projections_preserve_nullable_children_and_argument_snapshots() {
    let _inline = crate::common::InlineOffGuard::on();
    let lua = emit_module(
        r#"
enum Choice { Empty, Number(int), Text(string) }
value Child { choice: Choice, marker: int }
value Config { child: Child, optional: Choice?, spare: int }
struct Owner { config: Config, calls: int }
global CONFIG: Config = Config { child: Child { choice: Choice::Number(3), marker: 5 }, optional: null, spare: 11 }
fn get(owner: Owner) -> Owner { owner.calls += 1; return owner }
fn mutate(owner: Owner) -> int {
    owner.config.child.choice = Choice::Text("changed")
    owner.config.child.marker = 99
    owner.config.optional = null
    return 0
}
fn observe(choice: Choice, ignored: int) -> int { match choice { Choice::Number(n) => n, _ => -1 } }
fn optional(owner: Owner) -> Choice? { owner.config.optional }
fn main() {
    let owner = Owner { config: Config { child: Child { choice: Choice::Number(7), marker: 13 },
        optional: Choice::Number(17), spare: 19 }, calls: 0 }
    let saved = get(owner).config.child
    let present = optional(owner)
    print(observe(owner.config.child.choice, mutate(owner)))
    print(owner.calls)
    print(saved.marker)
    print(match saved.choice { Choice::Number(n) => n, _ => -1 })
    if let choice = present { print(match choice { Choice::Number(n) => n, _ => -1 }) }
    print(optional(owner) == null)
    let global_child = CONFIG.child
    CONFIG.child.marker = 101
    print(global_child.marker)
    print(CONFIG.optional == null)
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "enum_projection_snapshots"),
        "7\n1\n13\n7\n17\ntrue\n5\ntrue"
    );
}

#[test]
fn indexed_strided_projection_lowers_only_selected_lanes() {
    use skull_mir::mir::{ContainerKind, Inst};
    let ws = crate::common::ws_with_stdlib(
        r#"
nobox value Row { a: int, b: int, c: int, d: int, e: int, f: int }
fn selected(rows: [Row], index: int) -> int { rows[index].b }
fn main() { print(selected([Row { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6 }], 0)) }
"#,
    );
    let roots = skull::mir_emit::emission_roots(&ws, None);
    let graph = skull::instance_graph::build(&ws, roots, false);
    let bodies = skull::concrete_body::build(&ws, &graph);
    let plan = bodies
        .plans
        .iter()
        .find(|(inst, _)| {
            ws.module(inst.func.module).functions[inst.func.func]
                .name
                .as_str()
                == "selected"
        })
        .expect("selected has no plan")
        .1;
    let scx = skull_mir::lower::ScalarCtx::new(&ws).sharing(graph.interner().clone());
    let mir = skull_mir::lower::lower_instance_with_plans(&ws, &scx, plan, &bodies.plans)
        .expect("lower selected");
    let reads = mir
        .insts
        .iter()
        .filter(|(_, inst)| {
            matches!(
                inst,
                Inst::Index {
                    kind: ContainerKind::VecStrideSlot,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        reads, 1,
        "scalar field projection expanded sibling lanes before cleanup"
    );
    assert_eq!(
        mir.insts
            .iter()
            .filter(|(_, inst)| matches!(inst, Inst::VecCheckedIndex { .. }))
            .count(),
        1,
        "projection must retain exactly one element bounds check"
    );
}

#[test]
fn indexed_strided_projection_keeps_read_order_snapshots_and_bounds() {
    let source = r#"
nobox value Pair { x: int, y: int }
value Boxed { n: int }
enum Choice { Empty, Pair(Pair), Text(string) }
nobox value Row {
    spare: int = 99, point: Pair, optional: Pair?, boxed: Boxed?,
    choice: Choice, flag: bool = false, text: string = "hello"
}
struct State { rows: [Row], calls: int = 0 }
fn receiver(s: State) -> [Row] { s.calls = s.calls * 10 + 1; return s.rows }
fn index(s: State) -> int { s.calls = s.calls * 10 + 2; return 0 }
fn mutate(s: State) -> int {
    s.rows[0].point.x = 90
    s.rows[0].optional = null
    s.rows[0].boxed = Boxed { n: 100 }
    s.rows[0].choice = Choice::Empty
    return 1
}
fn observe(p: Pair, ignored: int) -> int { p.x }
@nomangle fn invalid(index: int) -> int {
    let rows: [Row] = []
    return rows[index].point.x
}
fn main() {
    let s = State { rows: [Row { point: Pair { x: 3, y: 5 },
        optional: Pair { x: 7, y: 11 }, boxed: Boxed { n: 13 },
        choice: Choice::Pair(Pair { x: 17, y: 19 }) }] }
    print(receiver(s)[index(s)].point.y)
    print(s.calls)
    let optional = s.rows[0].optional
    let boxed = s.rows[0].boxed
    let choice = s.rows[0].choice
    print(observe(s.rows[0].point, mutate(s)))
    print(s.rows[0].point.x)
    print(optional?.y ?? -1)
    print(boxed?.n ?? -1)
    print(match choice { Choice::Pair(p) => p.y, _ => -1 })
    print(s.rows[0].optional == null)
    print(s.rows[0].flag)
    print(s.rows[0].text)
}
"#;
    let lua = emit_module(source);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "indexed_projection_semantics"),
        "5\n12\n3\n90\n11\n13\n19\ntrue\nfalse\nhello"
    );
    let checks = run_luajit(
        &format!(
            r#"{lua}
for _, i in ipairs({{-1, 0}}) do
    local ok, err = pcall(invalid, i)
    print(ok, tostring(err):find("Vec index out of bounds", 1, true) ~= nil)
end
"#
        ),
        "indexed_projection_bounds",
    );
    assert_eq!(checks, "false\ttrue\nfalse\ttrue");
}
