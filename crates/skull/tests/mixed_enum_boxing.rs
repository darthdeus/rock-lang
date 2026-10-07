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

#[test]
fn dynamic_payload_boxes_reserve_only_the_variants_payload() {
    let lua = emit_module(SRC);
    let out = run_luajit(&format!(r#"{lua}
jit.off(); jit.flush()
local n = 20000
local slots = require("table.new")(n, 0)
for i = 1, n do slots[i] = false end
local function measure(make)
    collectgarbage("collect")
    local before = collectgarbage("count") * 1024
    for i = 1, n do slots[i] = make() end
    collectgarbage("collect")
    local bytes = collectgarbage("count") * 1024 - before
    for i = 1, n do slots[i] = false end
    collectgarbage("collect")
    return bytes / n
end
local ty = constant_unit().__ty
local control = measure(function() return {{__variant = "Text", __ty = ty, "payload"}} end)
local dynamic = measure(function() return through_field(1) end)
print("payload bytes", control, dynamic)
assert(dynamic <= control + 1, "dynamic payload reserves union slots")
print("ok")
"#), "mixed_enum_payload_size");
    assert!(out.ends_with("ok"), "{out}");
}

#[test]
fn variant_sizing_preserves_widened_offsets_and_named_payloads() {
    let src = r#"
nobox value Wide { a: int, b: int, c: int }
enum Packet {
    None, Narrow(string), Wide(Wide), Pair(string, int),
    Named { text: string }, NamedWide { text: string, code: int }
}
struct Holder { packet: Packet }
fn make_packet(kind: int) -> Packet {
    let p = if kind == 0 { Packet::None } else if kind == 1 { Packet::Narrow("one") }
        else if kind == 2 { Packet::Wide(Wide { a: 2, b: 3, c: 5 }) }
        else if kind == 3 { Packet::Pair("pair", 7) }
        else if kind == 4 { Packet::Named { text: "named" } }
        else { Packet::NamedWide { text: "wide", code: 11 } }
    return p
}
@nomangle fn packet(kind: int) -> any {
    let holder = Holder { packet: make_packet(kind) }
    return holder.packet
}
@nomangle fn score(kind: int) -> int {
    let p = make_packet(kind)
    return match p {
        Packet::None => 0,
        Packet::Narrow(s) => s.len(),
        Packet::Wide(w) => w.a + w.b + w.c,
        Packet::Pair(s, n) => s.len() + n,
        Packet::Named { text } => text.len(),
        Packet::NamedWide { text, code } => text.len() + code,
    }
}
"#;
    let lua = emit_module(src);
    let out = run_luajit(&format!(r#"{lua}
local expected = {{0, 3, 10, 11, 5, 15}}
for kind = 0, 5 do assert(score(kind) == expected[kind + 1]) end
local narrow = packet(1)
assert(narrow[1] == "one" and narrow[2] == nil and narrow[3] == nil)
local pair = packet(3)
assert(pair[1] == "pair" and pair[4] == 7)
local named = packet(4)
assert(named.text == "named" and named.code == nil and named[1] == nil)
local named_wide = packet(5)
assert(named_wide.text == "wide" and named_wide.code == 11)
print("ok")
"#), "mixed_enum_variant_layout");
    assert_eq!(out, "ok");
}

#[test]
fn dynamic_boxing_dispatches_by_payload_layout() {
    let src = r#"
enum Packet { None, A(string), B(string), C(string), D(string), E(string), F(string) }
struct BoxHolder { packet: Packet? }
@nomangle fn box_input(kind: int) -> any {
    let h = BoxHolder { packet: if kind < 0 { null } else {
        if kind == 0 { Packet::None } else if kind == 1 { Packet::A("payload") }
        else if kind == 2 { Packet::B("payload") } else if kind == 3 { Packet::C("payload") }
        else if kind == 4 { Packet::D("payload") } else if kind == 5 { Packet::E("payload") }
        else { Packet::F("payload") }
    } }
    return h.packet
}
"#;
    let lua = emit_module(src);
    let body = lua.split("function box_input(").nth(1).expect("box_input body")
        .split("\nend").next().expect("box_input end");
    assert_eq!(body.matches("__variant =").count(), 1,
        "identical payload layouts need one allocation site: {body}");
    let out = run_luajit(&format!(r#"{lua}
for kind, tag in ipairs({{"A", "B", "C", "D", "E", "F"}}) do
    local a, b = box_input(kind), box_input(kind)
    assert(a.__variant == tag and a[1] == "payload")
    assert(not rawequal(a, b))
end
assert(rawequal(box_input(0), box_input(0)))
assert(box_input(-1) == nil)
print("ok")
"#), "mixed_enum_layout_dispatch");
    assert_eq!(out, "ok");
}

#[test]
fn snapshot_and_image_reconstruction_canonicalize_mixed_units() {
    let source = include_str!("../../../stone/engine/snapshot.rock");
    let installer = source.split("fn __stone_install_deep_copy() = \"\"\"")
        .nth(1).expect("snapshot installer").split("\"\"\"")
        .next().expect("snapshot installer body");
    let lua = emit_module(SRC);
    let out = run_luajit(&format!(r#"{lua}
do
{installer}
end
local unit = constant_unit()
local reconstructed = {{__ty = unit.__ty, __variant = unit.__variant}}
assert(rawequal(__stone_deep_copy(reconstructed, {{}}), unit))
local capture = {{image_mode = "capture", image_live = {{}}, storage = {{}}}}
local image = __stone_deep_copy({{unit, reconstructed}}, {{}}, capture)
assert(not rawequal(image[1], unit))
for _, mode in ipairs({{"materialize", "host_materialize"}}) do
    local live = __stone_deep_copy(image, {{}}, {{image_mode = mode}})
    assert(rawequal(live[1], unit) and rawequal(live[2], unit))
end
local patch = __stone_patch_prepare(image, capture)
local live = __stone_patch_restore(patch, {{}}, {{image_mode = "materialize"}})
assert(rawequal(live[1], unit) and rawequal(live[2], unit))
local payload = through_field(1)
local a = __stone_deep_copy(payload, {{}})
local b = __stone_deep_copy(payload, {{}})
a[1] = "changed"
assert(b[1] == "payload")
print("ok")
"#), "mixed_enum_images");
    assert_eq!(out, "ok");
}
