use crate::common::{
    AllocationActivationGuard, compile_and_run, emit_mir, emit_module, run_luajit, run_main,
    try_emit_module,
};

fn remove_handle(method: &str) {
    let source = format!(
        r#"
struct Item {{ number: int }}
fn remove<T>(items: [T]) -> T? {{ return items.{method}(0) }}
fn main() {{
    let arena: Arena<Item> = arena_new()
    let first = arena.insert(Item {{ number: 7 }})
    let second = arena.insert(Item {{ number: 9 }})
    let items: [Entity<Item>] = [first, second]
    let removed = remove(items).unwrap()
    print(removed.get().unwrap().number, items[0].get().unwrap().number, items.len)
}}
"#
    );
    let lua = try_emit_module(&source).expect("generic handle removal must lower");
    assert_eq!(run_main(&lua), "7\t9\t1");
}

#[test]
fn generic_handle_remove_at_keeps_its_concrete_type() {
    remove_handle("remove_at");
}

#[test]
fn generic_handle_swap_remove_keeps_its_concrete_type() {
    remove_handle("swap_remove");
}

#[test]
fn coroutine_slot_updates_do_not_allocate() {
    let source = r#"
value Task<R = ()> { coroutine: Coroutine<int, R>, state: Entity<int> }
struct Owner { slot: Task? = null }
struct Entry { fetch: (() -> Task?)? = null, task: Task? = null, last: Task? = null }
fn Entry.live_task(entry) -> Task? {
    if let fetch = entry.fetch { return fetch() }
    return entry.task
}
co fn idle() -> Coroutine<int> { while true { yield 0 } }
fn attach<T>(owner: Entity<T>, field: FieldRef<T, Task?>) -> Entry {
    return Entry { fetch: () => if let row = owner.get() { field_get(row, field) } else { null } }
}
@nomangle fn make_entry() -> Entry {
    let owners: Arena<Owner> = arena_new()
    let states: Arena<int> = arena_new()
    let owner = owners.insert(Owner { slot: Task { coroutine: idle(), state: states.insert(1) } })
    return attach(owner, .slot)
}
@nomangle fn update(entry: Entry) -> int {
    if let task = entry.live_task() {
        if let last = entry.last {
            if last.state != task.state { entry.last = task }
        } else { entry.last = task }
        let _ = resume(task.coroutine)
        return task.state._slot
    }
    return -1
}
fn main() { let _ = update(make_entry()) }
"#;
    let lua = emit_module(source);
    let out = run_luajit(&format!(
        r#"{lua}
jit.off(); jit.flush()
local entry = make_entry()
for i = 1, 4 do update(entry) end
collectgarbage('collect'); collectgarbage('stop')
local before = collectgarbage('count')
local total = 0
for i = 1, 2000 do total = total + update(entry) end
local bytes = (collectgarbage('count') - before) * 1024 / 2000
collectgarbage('restart')
assert(total >= 0)
print(bytes)
"#
    ));
    let bytes: f64 = out.parse().expect("bytes per slot update");
    eprintln!("coroutine slot bytes/update: {bytes}");
    assert!(bytes < 1.0, "slot update allocates {bytes} bytes");
}

#[test]
fn opaque_forwarded_results_snapshot_before_their_owner_changes() {
    let fields = (0..50)
        .map(|i| format!("f{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = (0..50)
        .map(|i| format!("f{i}: 1"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        r#"
value Child {{ {fields} }}
value Wide {{ {fields}, child: Child? }}
struct Owner {{ row: Wide }}
inline(lua) fn read_row(owner: Owner) -> Wide = "return owner.row"
inline(lua) fn read_optional(owner: Owner) -> Wide? = "return owner.row"
fn forward(owner: Owner) -> Wide {{ return read_row(owner) }}
fn forward_optional(owner: Owner) -> Wide? {{ return read_optional(owner) }}
fn change(owner: Owner) -> int {{
    owner.row.f0 = 3
    if owner.row.child != null {{ owner.row.child.f0 = 3 }}
    return 0
}}
fn observe(row: Wide, ignored: int) {{
    assert_eq(row.f0, 1)
    assert_eq(row.child?.f0, 1)
}}
fn main() {{
    let owner = Owner {{ row: Wide {{ {values}, child: Child {{ {values} }} }} }}
    let direct = read_row(owner)
    let forwarded = forward(owner)
    let optional = read_optional(owner)
    let forwarded_optional = forward_optional(owner)
    let callback: () -> Wide? = () => read_optional(owner)
    let indirect = callback()
    let selected = if true {{ read_row(owner) }} else {{ read_row(owner) }}
    let selected_optional = if true {{ read_optional(owner) }} else {{ read_optional(owner) }}
    let selected_match = match 0 {{ 0 => read_row(owner), _ => read_row(owner) }}
    let selected_iflet = if let row = optional {{ read_row(owner) }} else {{ read_row(owner) }}
    let coalesced = read_optional(owner) ?? owner.row
    if let narrowed = callback() {{
        owner.row.f0 = 2
        if owner.row.child != null {{ owner.row.child.f0 = 2 }}
        assert_eq(narrowed.f0, 1)
        assert_eq(narrowed.child?.f0, 1)
        owner.row.f0 = 1
        if owner.row.child != null {{ owner.row.child.f0 = 1 }}
    }} else {{ assert(false) }}
    owner.row.f0 = 99
    if owner.row.child != null {{ owner.row.child.f0 = 99 }}
    assert_eq(direct.f0, 1)
    assert_eq(direct.child?.f0, 1)
    assert_eq(forwarded.f0, 1)
    assert_eq(forwarded.child?.f0, 1)
    assert_eq(optional?.f0, 1)
    assert_eq(optional?.child?.f0, 1)
    assert_eq(forwarded_optional?.f0, 1)
    assert_eq(forwarded_optional?.child?.f0, 1)
    assert_eq(indirect?.f0, 1)
    assert_eq(indirect?.child?.f0, 1)
    assert_eq(selected.f0, 1)
    assert_eq(selected.child?.f0, 1)
    assert_eq(selected_optional?.f0, 1)
    assert_eq(selected_optional?.child?.f0, 1)
    assert_eq(selected_match.child?.f0, 1, "match snapshot")
    assert_eq(selected_iflet.child?.f0, 1, "if-let snapshot")
    assert_eq(coalesced.f0, 1)
    assert_eq(coalesced.child?.f0, 1)
    owner.row.f0 = 1
    if owner.row.child != null {{ owner.row.child.f0 = 1 }}
    observe(row: read_row(owner), ignored: change(owner))
    owner.row.f0 = 1
    if owner.row.child != null {{ owner.row.child.f0 = 1 }}
    fn observe_nested(row: Wide, ignored: int) -> int {{
        return row.f0 + (row.child?.f0 ?? -1)
    }}
    assert_eq(observe_nested(row: read_row(owner), ignored: change(owner)), 2)
    print("ok")
}}
"#
    );
    let lua = emit_module(&source);
    assert_eq!(run_main(&lua), "ok");
}

const SOURCE: &str = r#"
nobox value Hit { f0: float, f1: float, f2: float, f3: float, f4: float, f5: float, f6: float }
value Landing { a: Hit, b: Hit, c: Hit, d: Hit, e: Hit, f: Hit, g: Hit, environment: [float]? = null }
value Receipt { landing: Landing? = null, sequence: int }
enum Event { Cast(Receipt), Other }
@nomangle fn make(seed: float) -> Receipt {
    let hit = Hit { f0: seed, f1: 1.0, f2: 2.0, f3: 3.0, f4: 4.0, f5: 5.0, f6: 6.0 }
    return Receipt { sequence: 1, landing: Landing { a: hit, b: hit, c: hit, d: hit, e: hit, f: hit, g: hit } }
}
@nomangle fn retain(receipt: Receipt) -> Event { return Event::Cast(receipt) }
@nomangle fn relay(receipt: Receipt) -> Receipt { return receipt }
value HandleRow { handle: Entity<int>?, sequence: int }
@nomangle fn relay_handle(row: HandleRow) -> HandleRow { return row }
@nomangle fn relay_token(token: Entity<int>) -> Entity<int> { return token }
fn main() { let _ = retain(relay(make(42.0))) }
"#;

fn allocation_probe(probe: &str) -> String {
    let _activation = AllocationActivationGuard::on();
    let emit = emit_mir(SOURCE);
    let mut lua = emit.lua;
    lua.push_str("\nlocal constructors = {\n");
    for body in emit.allocation_report.bodies.values() {
        for site in &body.direct_sites {
            if site.aggregate_type.is_some() {
                let kind = if site.aggregate_type.as_deref() == Some("Hit") {
                    "cdata"
                } else {
                    "table"
                };
                lua.push_str(&format!("[\"{:016x}\"] = \"{kind}\",\n", site.id));
            }
        }
    }
    lua.push_str(
        r#"}
jit.off(); jit.flush()
local tables, cdata = 0, 0
local hit = __rock_alloc_hit
__rock_alloc_hit = function(id, value)
    if constructors[id] == "table" then tables = tables + 1 end
    if constructors[id] == "cdata" then cdata = cdata + 1 end
    return hit(id, value)
end
local ffi_new = ffi.new
ffi.new = function(...)
    cdata = cdata + 1
    return ffi_new(...)
end
local receipt = make(42)
tables, cdata = 0, 0
"#,
    );
    lua.push_str(probe);
    run_luajit(&lua)
}

#[test]
fn returning_a_record_keeps_its_boxed_child() {
    assert_eq!(
        allocation_probe(
            r#"
local result = relay(receipt)
print(tables, cdata, rawequal(receipt.landing, result.landing))
"#
        ),
        "0\t0\ttrue"
    );
}

#[test]
fn passing_a_record_into_an_enum_keeps_its_boxed_child() {
    assert_eq!(
        allocation_probe(
            r#"
local result = retain(receipt)
print(tables, cdata, rawequal(receipt.landing, result[1]))
"#
        ),
        "1\t0\ttrue"
    );
}

#[test]
fn passing_a_handle_field_through_keeps_the_handle() {
    assert_eq!(
        allocation_probe(
            r#"
local handle = { __ty = "Entity", _scene_id = 1, _arena_id = 2, _slot = 3, _generation = 4 }
tables, cdata = 0, 0
local row = { __ty = "HandleRow", handle = handle, sequence = 7 }
local result = relay_handle(row)
print(tables, cdata, rawequal(handle, result.handle), result.sequence)
"#
        ),
        "0\t0\ttrue\t7"
    );
}

#[test]
fn passing_a_handle_through_keeps_the_callers_box() {
    assert_eq!(
        allocation_probe(
            r#"
local handle = { __ty = "Entity", _scene_id = 1, _arena_id = 2, _slot = 3, _generation = 4 }
tables, cdata = 0, 0
local result = relay_token(handle)
print(tables, cdata, rawequal(handle, result))
"#
        ),
        "0\t0\ttrue"
    );
}

#[test]
fn an_immutable_local_snapshots_a_mutable_owners_child() {
    let fields = (0..50)
        .map(|i| format!("f{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = (0..50)
        .map(|i| format!("f{i}: 1"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        r#"
value Child {{ {fields} }}
value Row {{ child: Child?, items: [int] }}
struct Owner {{ row: Row }}
fn main() {{
    let owner = Owner {{ row: Row {{ child: Child {{ {values} }}, items: [] }} }}
    let saved = owner.row
    if owner.row.child != null {{ owner.row.child.f0 = 99 }}
    print(saved.child?.f0 ?? -1)
}}
"#
    );
    assert_eq!(compile_and_run(&source), "1");
}

#[test]
fn shared_boundary_results_copy_before_mutable_storage_and_ref_borrows() {
    let source = format!(
        r#"{SOURCE}
struct Owner {{ receipt: Receipt }}
global SAVED: Receipt? = null
fn change(receipt: ref Receipt) {{
    if receipt.landing != null {{ receipt.landing.a.f0 = 99.0 }}
}}
fn borrowed_inside(receipt: Receipt) -> Receipt {{
    change(receipt)
    return receipt
}}
fn save_and_return(receipt: Receipt) -> Receipt {{
    return {{ SAVED = receipt; receipt }}
}}
fn signal(receipt: Receipt) -> float {{ return receipt.landing.unwrap().a.f0 }}
fn Receipt.pass(receipt) -> Receipt {{ return receipt }}
inline(lua) fn foreign_receipt(owner: Owner) -> Receipt = """return owner.receipt"""
@nomangle fn probe_semantics() {{
    let original = make(42.0)
    let owner = Owner {{ receipt: relay(original) }}
    change(owner.receipt)
    assert_eq(signal(original), 42.0)
    assert(signal(owner.receipt) == 99.0, "owner changed")
    let local = relay(original)
    change(local)
    assert_eq(signal(original), 42.0)
    assert(signal(local) == 99.0, "local changed")
    assert(signal(borrowed_inside(original)) == 99.0, "by-value parameter changed locally")
    assert_eq(signal(original), 42.0)
    let returned = save_and_return(original)
    if SAVED != null {{ change(SAVED) }}
    assert_eq(signal(original), 42.0)
    assert_eq(signal(returned), 42.0)
    let items = [relay(original)]
    change(items[0])
    assert_eq(signal(original), 42.0)
    assert(signal(items[0]) == 99.0, "element changed")
    let event = retain(original)
    let again = Owner {{ receipt: original }}
    change(again.receipt)
    match event {{ Event::Cast(receipt) => assert_eq(signal(receipt), 42.0), _ => assert(false) }}
    let foreign_owner = Owner {{ receipt: make(42.0) }}
    let foreign_copy = foreign_receipt(foreign_owner)
    change(foreign_owner.receipt)
    assert(signal(foreign_copy) == 42.0, "a foreign result is a snapshot")
    let maybe: Receipt? = original
    let method_owner = Owner {{ receipt: maybe?.pass() ?? original }}
    change(method_owner.receipt)
    assert(signal(original) == 42.0, "a safe-nav call result copies at a mutable store")
    print("ok")
}}
"#
    );
    let lua = emit_module(&source);
    assert_eq!(run_luajit(&format!("{lua}\nprobe_semantics()\n")), "ok");
}

fn assert_allocation_free_compare(source: &str, predicate: &str) {
    let lua = emit_module(source);
    let output = run_luajit(&format!(
        r#"{lua}
jit.off(); jit.flush()
local owner = make_owner()
for i = 1, 10 do assert({predicate}) end
collectgarbage('collect'); collectgarbage('stop')
local before = collectgarbage('count')
for i = 1, 1000 do assert({predicate}) end
local bytes = (collectgarbage('count') - before) * 1024
collectgarbage('restart')
assert(bytes < 1024, 'nullable comparison allocated ' .. bytes .. ' bytes')
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}

#[test]
fn unused_nullable_child_is_not_copied_for_membership_comparison() {
    let source = r#"
value Token { a: int, b: int, c: int, d: int }
nobox value Membership { token: Token?, rows: [int], selected: [int] }
struct Owner { membership: Membership? }
fn shared(first: Membership, second: Membership) -> bool {
    return same_ref(first.rows, second.rows) && same_ref(first.selected, second.selected)
}
@nomangle fn make_owner() -> Owner {
    return Owner { membership: Membership { token: Token { a: 1, b: 2, c: 3, d: 4 }, rows: [3], selected: [4] } }
}
@nomangle fn compare(owner: Owner) -> bool {
    let membership = owner.membership ?? return false
    return shared(membership, owner.membership.unwrap())
}
fn main() { let _ = compare(make_owner()) }
"#;
    assert_allocation_free_compare(source, "compare(owner)");
}

#[test]
fn nullable_child_read_crosses_a_value_call_without_reboxing() {
    let source = r#"
value Token { a: int, b: int, c: int, d: int }
nobox value Membership { token: Token?, rows: [int], selected: [int] }
struct Owner { membership: Membership? }
fn read(value: Membership) -> int {
    let membership = value
    return membership.token?.a ?? -1
}

@nomangle fn make_owner() -> Owner {
    return Owner { membership: Membership { token: Token { a: 1, b: 2, c: 3, d: 4 }, rows: [3], selected: [4] } }
}
@nomangle fn compare(owner: Owner) -> int {
    let membership = owner.membership ?? return -1
    return read(membership)
}
fn main() { let _ = compare(make_owner()) }
"#;
    assert_allocation_free_compare(source, "compare(owner) == 1");
}

#[test]
fn nullable_parameter_lanes_snapshot_before_later_argument_effects() {
    let source = r#"
value Token { a: int, b: int, c: int, d: int }
nobox value Row { token: Token?, items: [int] }
struct Owner { row: Row }
fn change(owner: Owner) -> int {
    if owner.row.token != null { owner.row.token.a = 99 }
    return 0
}
fn observe(row: Row, ignored: int) -> int { return row.token?.a ?? -1 }
fn local_change(row: Row) -> int {
    let copy = Owner { row }
    if copy.row.token != null { copy.row.token.a = 7 }
    return copy.row.token?.a ?? -1
}
fn main() {
    let owner = Owner { row: Row { token: Token { a: 1, b: 2, c: 3, d: 4 }, items: [] } }
    assert_eq(observe(owner.row, change(owner)), 1)
    owner.row.token = Token { a: 1, b: 2, c: 3, d: 4 }
    assert_eq(observe(row: owner.row, ignored: change(owner)), 1)
    assert_eq(owner.row.token?.a, 99)
    assert_eq(local_change(owner.row), 7)
    assert_eq(owner.row.token?.a, 99)
    owner.row.token = null
    assert_eq(observe(owner.row, 0), -1)
    assert_eq(local_change(owner.row), -1)
    print("ok")
}
"#;
    assert_eq!(compile_and_run(source), "ok");
}

#[test]
fn nullable_child_crosses_a_call_with_an_omitted_default_without_reboxing() {
    let source = r#"
value Token { a: int, b: int, c: int, d: int }
nobox value Membership { token: Token?, rows: [int], selected: [int] }
struct Owner { membership: Membership? }
fn first(value: Membership) -> int { return value.token?.a ?? -1 }
fn read(value: Membership, offset: int = 0) -> int { return first(value) + offset }

@nomangle fn make_owner() -> Owner {
    return Owner { membership: Membership { token: Token { a: 1, b: 2, c: 3, d: 4 }, rows: [3], selected: [4] } }
}
@nomangle fn compare(owner: Owner) -> int {
    let membership = owner.membership ?? return -1
    return read(membership) + read(membership, 1) - 2
}
fn main() {
    let owner = make_owner()
    assert_eq(compare(owner), 1)
    let empty = Membership { token: null, rows: [], selected: [] }
    assert_eq(read(empty), -1)
    assert_eq(read(offset: 2, value: empty), 1)
    print("ok")
}
"#;
    assert_allocation_free_compare(source, "compare(owner) == 1");
    assert_eq!(compile_and_run(source), "ok");
}
