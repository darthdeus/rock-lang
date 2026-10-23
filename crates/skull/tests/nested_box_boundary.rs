use crate::common::{AllocationActivationGuard, emit_mir, emit_module, run_luajit};

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

fn allocation_probe(tag: &str, probe: &str) -> String {
    let _activation = AllocationActivationGuard::on();
    let emit = emit_mir(SOURCE);
    let mut lua = emit.lua;
    lua.push_str("\nlocal constructors = {\n");
    for body in emit.allocation_report.bodies.values() {
        for site in &body.direct_sites {
            if site.aggregate_type.is_some() {
                let kind = if site.aggregate_type.as_deref() == Some("Hit") { "cdata" } else { "table" };
                lua.push_str(&format!("[\"{:016x}\"] = \"{kind}\",\n", site.id));
            }
        }
    }
    lua.push_str(r#"}
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
"#);
    lua.push_str(probe);
    run_luajit(&lua, tag)
}

#[test]
fn returning_a_record_keeps_its_boxed_child() {
    assert_eq!(allocation_probe("returning_a_record_keeps_its_boxed_child", r#"
local result = relay(receipt)
print(tables, cdata, rawequal(receipt.landing, result.landing))
"#), "0\t0\ttrue");
}

#[test]
fn passing_a_record_into_an_enum_keeps_its_boxed_child() {
    assert_eq!(allocation_probe("passing_a_record_into_an_enum_keeps_its_boxed_child", r#"
local result = retain(receipt)
print(tables, cdata, rawequal(receipt.landing, result[1]))
"#), "1\t0\ttrue");
}

#[test]
fn passing_a_handle_field_through_keeps_the_handle() {
    assert_eq!(allocation_probe("passing_a_handle_field_through_keeps_the_handle", r#"
local handle = { __ty = "Entity", _scene_id = 1, _arena_id = 2, _slot = 3, _generation = 4 }
tables, cdata = 0, 0
local row = { __ty = "HandleRow", handle = handle, sequence = 7 }
local result = relay_handle(row)
print(tables, cdata, rawequal(handle, result.handle), result.sequence)
"#), "0\t0\ttrue\t7");
}

#[test]
fn passing_a_handle_through_keeps_the_callers_box() {
    assert_eq!(allocation_probe("passing_a_handle_through_keeps_the_callers_box", r#"
local handle = { __ty = "Entity", _scene_id = 1, _arena_id = 2, _slot = 3, _generation = 4 }
tables, cdata = 0, 0
local result = relay_token(handle)
print(tables, cdata, rawequal(handle, result))
"#), "0\t0\ttrue");
}

#[test]
fn an_immutable_local_snapshots_a_mutable_owners_child() {
    let fields = (0..50).map(|i| format!("f{i}: int")).collect::<Vec<_>>().join(", ");
    let values = (0..50).map(|i| format!("f{i}: 1")).collect::<Vec<_>>().join(", ");
    let source = format!(r#"
value Child {{ {fields} }}
value Row {{ child: Child?, items: [int] }}
struct Owner {{ row: Row }}
fn main() {{
    let owner = Owner {{ row: Row {{ child: Child {{ {values} }}, items: [] }} }}
    let saved = owner.row
    if owner.row.child != null {{ owner.row.child.f0 = 99 }}
    print(saved.child?.f0 ?? -1)
}}
"#);
    let lua = emit_module(&source);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nested_box_snapshot"), "1");
}

#[test]
fn shared_boundary_results_copy_before_mutable_storage_and_ref_borrows() {
    let source = format!(r#"{SOURCE}
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
"#);
    let lua = emit_module(&source);
    assert_eq!(run_luajit(&format!("{lua}\nprobe_semantics()\n"), "nested_box_semantics"), "ok");
}
