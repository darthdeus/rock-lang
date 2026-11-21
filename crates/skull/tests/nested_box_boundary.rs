use crate::common::{activation_scope, compile_and_run, emit_mir, emit_module, run_luajit};

#[test]
fn a_read_only_snapshot_does_not_rebox_its_nullable_child() {
    let _inline = crate::common::inline_off_guard();
    let fields = (0..50)
        .map(|i| format!("f{i}: int = 0"))
        .collect::<Vec<_>>()
        .join(", ");
    let lua = emit_module(&format!(
        r#"
value Wide {{ {fields} }}
value Row {{ child: Wide?, refs: [int] }}
struct Owner {{ row: Row }}
fn make_wide(seed: int) -> Wide {{ Wide {{ f0: seed }} }}
@nomangle fn read_only(seed: int) -> int {{
    let owner = Owner {{ row: Row {{ child: make_wide(seed), refs: [] }} }}
    let saved = owner.row
    let first = if saved.child != null {{ saved.child.f0 }} else {{ -1 }}
    let second = saved.child?.f0 ?? -1
    return first + second
}}
"#
    ));
    let body = crate::common::function_body(&lua, "read_only");
    assert!(!body.contains("Wide_ct("));
    assert_eq!(run_luajit(&format!("{lua}\nprint(read_only(7))")), "14");
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
    assert!(bytes < 1.0);
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
@nomangle fn select_receipt(first: Receipt, second: Receipt, flag: bool) -> Receipt {
    let selected = if flag { first } else { second }
    return selected
}
@nomangle fn match_receipt(first: Receipt, second: Receipt, flag: bool) -> Receipt {
    let selected = match flag { true => first, false => second }
    return selected
}
value HandleRow { handle: Entity<int>?, sequence: int }
@nomangle fn relay_handle(row: HandleRow) -> HandleRow { return row }
@nomangle fn relay_token(token: Entity<int>) -> Entity<int> { return token }
fn main() { let _ = retain(relay(make(42.0))) }
"#;

fn allocation_probe_lua() -> String {
    let _activation = activation_scope(true);
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
    lua
}

#[test]
fn records_and_handles_crossing_calls_keep_their_boxed_children() {
    let lua = allocation_probe_lua();
    for (case, probe, expected) in [
        (
            "returning a record keeps its boxed child",
            r#"
local result = relay(receipt)
print(tables, cdata, rawequal(receipt.landing, result.landing))
"#,
            "0\t0\ttrue",
        ),
        (
            "returning an immutable conditional local keeps its boxed child",
            r#"
local second = make(7)
tables, cdata = 0, 0
local first_result = select_receipt(receipt, second, true)
local second_result = select_receipt(receipt, second, false)
print(tables, cdata, rawequal(receipt.landing, first_result.landing), rawequal(second.landing, second_result.landing))
"#,
            "0\t0\ttrue\ttrue",
        ),
        (
            "returning an immutable match local keeps its boxed child",
            r#"
local second = make(7)
tables, cdata = 0, 0
local first_result = match_receipt(receipt, second, true)
local second_result = match_receipt(receipt, second, false)
print(tables, cdata, rawequal(receipt.landing, first_result.landing), rawequal(second.landing, second_result.landing))
"#,
            "0\t0\ttrue\ttrue",
        ),
        (
            "passing a record into an enum keeps its boxed child",
            r#"
local result = retain(receipt)
print(tables, cdata, rawequal(receipt.landing, result[1]))
"#,
            "1\t0\ttrue",
        ),
        (
            "passing a handle field through keeps the handle",
            r#"
local handle = Entity_ct(1, 2, 3, 4)
tables, cdata = 0, 0
local row = { __ty = "HandleRow", handle = handle, sequence = 7 }
local result = relay_handle(row)
print(tables, cdata, rawequal(handle, result.handle), result.sequence)
"#,
            "0\t0\ttrue\t7",
        ),
        (
            "passing a handle through keeps the callers box",
            r#"
local handle = Entity_ct(1, 2, 3, 4)
tables, cdata = 0, 0
local result = relay_token(handle)
print(tables, cdata, rawequal(handle, result))
"#,
            "0\t0\ttrue",
        ),
    ] {
        assert_eq!(run_luajit(&format!("{lua}{probe}")), expected, "{case}");
    }
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
