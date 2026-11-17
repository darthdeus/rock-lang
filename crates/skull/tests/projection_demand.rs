use crate::common::{compile_and_run, emit_module, run_luajit, run_main};

#[test]
fn immediate_nullable_enum_projection_reads_only_selected_field() {
    let _inline = crate::common::InlineOffGuard::on();
    let lua = emit_module(
        r#"
enum Count { Zero, Seven }
value Item { spare: int = 11, count: Count = Count::Seven }
struct Holder { item: Item?, calls: int = 0 }
fn next(holder: Holder) -> Holder { holder.calls += 1; holder }
@nomangle fn nullable_probe(holder: Holder) -> bool { next(holder).item?.count == Count::Seven }
@nomangle fn snapshot_probe(holder: Holder) -> bool {
    let saved = holder.item
    saved?.count == Count::Seven
}
fn mutate(holder: Holder) -> bool { holder.item = null; false }
fn first(a: bool, b: bool) -> bool { a }
fn main() {
    let holder = Holder { item: Item {} }
    print(nullable_probe(holder))
    let saved = holder.item
    print(first(nullable_probe(holder), mutate(holder)))
    print(nullable_probe(holder))
    print(holder.calls)
    print(saved?.count == Count::Seven)
}
"#,
    );
    let probe = crate::common::function_body(&lua, "nullable_probe");
    assert!(!probe.contains(".spare"));
    assert_eq!(run_main(&lua), "true\ntrue\nfalse\n3\ntrue");
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
    assert!(!probe.contains(".unused_sibling"));
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nmain(); local a = pcall(probe, nil); local b = pcall(probe, {{config=nil}}); print(a, b)\n"
        )),
        "7\nfalse\tfalse"
    );
}

#[test]
fn boxed_vec_projection_reads_selected_fields_in_order_and_snapshots_children() {
    let fields = (0..20)
        .map(|i| format!("unused_sibling{i}: int = {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let lua = emit_module(&format!(
        r#"
value Point {{ x: int = 3, y: int = 5 }}
value Config {{ chosen: int = 7, label: string = "table", child: Point = Point {{}}, {fields} }}
struct Log {{ events: [int] }}
fn base(items: [Config], log: Log) -> [Config] {{ log.events.push(1); items }}
fn index(log: Log) -> int {{ log.events.push(2); 0 }}
@inline fn whole(items: [Config], i: int) -> Config {{ items[i] }}
@nomangle fn direct_probe(items: [Config], i: int) -> int {{ items[i].chosen }}
@nomangle fn whole_probe(items: [Config], i: int) -> int {{ whole(items, i).chosen }}
@nomangle fn make_probe_items() -> [Config] {{ [Config {{}}] }}
fn mutate(items: [Config]) -> int {{ items[0].child.y = 99; 1 }}
fn observe(point: Point, ignored: int) -> int {{ point.y }}
fn main() {{
    let items = make_probe_items()
    let log = Log {{ events: [] }}
    print(base(items, log)[index(log)].chosen)
    print(log.events[0]); print(log.events[1]); print(log.events.len())
    let saved = items[0].child
    print(observe(items[0].child, mutate(items)))
    print(saved.y)
    print(items[0].child.y)
}}
"#
    ));
    let probe = lua
        .split("\nfunction direct_probe(")
        .nth(1)
        .unwrap()
        .split("\nend")
        .next()
        .unwrap();
    assert!(!probe.contains(".unused_sibling") && !probe.contains(".label"));
    let whole = crate::common::function_body(&lua, "whole_probe");
    assert!(!whole.contains(".unused_sibling"));
    assert_eq!(run_main(&lua), "7\n1\n2\n2\n5\n5\n99");
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
    assert!(!body.contains(".unused_sibling"));
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nmain(); local a = pcall(selected, nil); local b = pcall(selected, {{config=nil}}); print(a, b)\n"
        )),
        "7\nnew\nfalse\tfalse"
    );
}

#[test]
fn enum_projections_preserve_nullable_children_and_argument_snapshots() {
    let _inline = crate::common::InlineOffGuard::on();
    assert_eq!(
        compile_and_run(
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
"#
        ),
        "7\n1\n13\n7\n17\ntrue\n5\ntrue"
    );
}

#[test]
fn indexed_strided_projection_reads_one_lane_and_checks_once() {
    let lua = emit_module(
        r#"
nobox value Row { a: int, b: int, c: int, d: int, e: int, f: int }
fn selected(rows: [Row], index: int) -> int { rows[index].b }
fn main() { print(selected([Row { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6 }], 0)) }
"#,
    );
    let body = crate::common::function_body(&lua, "selected");
    assert_eq!(body.matches("_data").count(), 1);
    assert_eq!(body.matches("__vec_checked_index(").count(), 1);
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
        run_main(&lua),
        "5\n12\n3\n90\n11\n13\n19\ntrue\nfalse\nhello"
    );
    let checks = run_luajit(&format!(
        r#"{lua}
for _, i in ipairs({{-1, 0}}) do
    local ok, err = pcall(invalid, i)
    print(ok, tostring(err):find("Vec index out of bounds", 1, true) ~= nil)
end
"#
    ));
    assert_eq!(checks, "false\ttrue\nfalse\ttrue");
}
