use crate::common::{emit_module, run_luajit};

const SRC: &str = r#"
nobox value Wide { a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int, j: int, k: int, l: int, m: int, n: int, o: int }
enum Choice { None, Empty, Text(string), Wide(Wide) }
struct Holder { item: Choice }
struct MaybeHolder { item: Choice? }

@nomangle fn boxed(kind: int) -> any {
    let item = if kind == 0 { Choice::None } else { Choice::Text("payload") }
    return item
}
@nomangle fn through_vec(kind: int) -> any {
    let values: [Choice] = []
    values.push(if kind == 0 { Choice::None } else { Choice::Text("payload") })
    return values[0]
}
@nomangle fn through_field(kind: int) -> any {
    let holder = Holder { item: if kind == 0 { Choice::None } else { Choice::Text("payload") } }
    return holder.item
}
@nomangle fn through_nullable(kind: int) -> any {
    let holder = MaybeHolder { item: if kind < 0 { null } else { if kind == 0 { Choice::None } else { Choice::Text("payload") } } }
    return holder.item
}
@nomangle fn constant_unit() -> any { Choice::None }
@nomangle fn other_unit() -> any { Choice::Empty }
@nomangle fn unit_equal(kind: int) -> bool {
    let item = if kind == 0 { Choice::None } else { Choice::Text("payload") }
    return item == Choice::None
}
@nomangle fn copied(kind: int) -> any {
    let holder = Holder { item: if kind == 0 { Choice::None } else { Choice::Text("payload") } }
    return deepcopy(holder).item
}
"#;

#[test]
fn dynamic_mixed_units_share_identity_across_boxing_boundaries() {
    let lua = emit_module(SRC);
    let out = run_luajit(&format!(r#"{lua}
local unit = constant_unit()
for _, make in ipairs({{boxed, through_vec, through_field, through_nullable}}) do
    assert(rawequal(make(0), unit))
    local a, b = make(1), make(1)
    assert(not rawequal(a, b))
    a[1] = "changed"
    assert(b[1] == "payload")
end
assert(through_nullable(-1) == nil)
assert(not rawequal(unit, other_unit()))
assert(unit_equal(0) and not unit_equal(1))
local keys = {{[unit] = true, [boxed(0)] = true}}
local count = 0
for _ in pairs(keys) do count = count + 1 end
assert(count == 1)
print("ok")
"#), "mixed_enum_identity");
    assert_eq!(out, "ok");
}

#[test]
fn dynamic_mixed_unit_boxing_allocates_no_per_value_tables() {
    let lua = emit_module(SRC);
    let out = run_luajit(&format!(r#"{lua}
jit.off(); jit.flush()
local n = 20000
local slots = require("table.new")(n, 0)
for i = 1, n do slots[i] = false end
collectgarbage("collect")
local before = collectgarbage("count") * 1024
for i = 1, n do slots[i] = through_field(0) end
collectgarbage("collect")
local bytes = collectgarbage("count") * 1024 - before
assert(bytes < n, tostring(bytes / n) .. " bytes per unit")
assert(rawequal(slots[1], slots[n]))
print("ok")
"#), "mixed_enum_unit_allocation");
    assert_eq!(out, "ok");
}

#[test]
fn deepcopy_canonicalizes_mixed_units_and_keeps_payloads_owned() {
    let lua = emit_module(SRC);
    let out = run_luajit(&format!(r#"{lua}
assert(rawequal(copied(0), constant_unit()))
local a, b = copied(1), copied(1)
a[1] = "changed"
assert(b[1] == "payload")
print("ok")
"#), "mixed_enum_copy");
    assert_eq!(out, "ok");
}

#[test]
fn mixed_units_keep_identity_through_body_overlay_and_full_reload() {
    use crate::common::ws_with_stdlib;
    use bone_semantic::hir::BodyOwner;
    let ws = ws_with_stdlib(SRC);
    let mid = ws.module_ids[&ustr::Ustr::from("<test>")];
    let (full, mut cache) = skull::mir_emit::emit_full(&ws, mid).expect("full");
    let edited = SRC.replace("kind == 0", "kind < 1");
    let ws2 = ws_with_stdlib(&edited);
    let reinferred: Vec<_> = ws2.modules[mid.index()].functions.iter()
        .filter(|(_, f)| !matches!(f.name.as_str(), "constant_unit" | "other_unit"))
        .map(|(fid, _)| (mid, BodyOwner::Function(fid)))
        .collect();
    let overlay = skull::mir_emit::emit_overlay(&ws2, &mut cache, &reinferred)
        .expect("overlay");
    let fresh = skull::mir_emit::emit_full(&ws2, mid).expect("fresh").0;
    let out = run_luajit(&format!(r#"{}
local before = through_field(0)
assert(rawequal(before, constant_unit()))
_G.HOT_RELOAD = true
{}
assert(rawequal(before, through_field(0)))
assert(rawequal(before, through_field(-1)))
{}
assert(rawequal(before, through_vec(0)))
assert(rawequal(before, copied(0)))
print("ok")
"#, full.lua, overlay.lua, fresh.lua), "mixed_enum_reload");
    assert_eq!(out, "ok");
}

#[test]
fn strided_element_reconstruction_canonicalizes_units() {
    let lua = emit_module(SRC);
    let out = run_luajit(&format!(r#"{lua}
local unit = constant_unit()
local vec = {{__ty = "Vec", _data = {{"None", false}}, len = 1,
    ["@vec"] = {{repr = "strided", elem = unit.__ty, stride = 2}}}}
assert(rawequal(__rock_materialize_strided_element(vec, 1), unit))
vec._data[1], vec._data[2] = "Text", "payload"
local a = __rock_materialize_strided_element(vec, 1)
local b = __rock_materialize_strided_element(vec, 1)
a[1] = "changed"
assert(b[1] == "payload")
print("ok")
"#), "mixed_enum_strided_reconstruction");
    assert_eq!(out, "ok");
}
