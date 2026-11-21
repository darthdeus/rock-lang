use crate::common::{activation_scope, emit_mir, emit_module, run_luajit};

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
