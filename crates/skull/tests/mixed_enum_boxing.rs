use crate::common::{emit_module, function_body, run_luajit};

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
fn mixed_enum_boxes_preserve_identity_and_payload_layout() {
    let lua = emit_module(SRC);
    for (case, probe) in [
        (
            "unit identity across boxing boundaries",
            r#"
local unit = constant_unit()
for _, make in ipairs({boxed, through_vec, through_field, through_nullable, copied}) do
    assert(rawequal(make(0), unit))
    local a, b = make(1), make(1)
    assert(not rawequal(a, b))
    a[1] = "changed"
    assert(b[1] == "payload")
end
assert(through_nullable(-1) == nil)
assert(not rawequal(unit, other_unit()))
assert(unit_equal(0) and not unit_equal(1))
local keys = {[unit] = true, [boxed(0)] = true}
local count = 0
for _ in pairs(keys) do count = count + 1 end
assert(count == 1)
print("ok")
"#,
        ),
        (
            "unit boxing does not allocate a table per value",
            r#"
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
"#,
        ),
        (
            "strided reconstruction uses the canonical unit",
            r#"
local unit = constant_unit()
local vec = {_data = {"None", false}, len = 1,
    ["@vec"] = {repr = "strided", elem = __rock_get_type(unit), stride = 2}}
assert(rawequal(__rock_materialize_strided_element(vec, 1), unit))
vec._data[1], vec._data[2] = "Text", "payload"
local a = __rock_materialize_strided_element(vec, 1)
local b = __rock_materialize_strided_element(vec, 1)
a[1] = "changed"
assert(b[1] == "payload")
print("ok")
"#,
        ),
        (
            "payload capacity follows the variant",
            r#"
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
local mt = getmetatable(constant_unit())
local control = measure(function() return setmetatable({__variant = "Text", "payload"}, mt) end)
local dynamic = measure(function() return through_field(1) end)
assert(dynamic <= control + 1, "dynamic payload reserves union slots: " .. control .. " vs " .. dynamic)
print("ok")
"#,
        ),
    ] {
        assert_eq!(run_luajit(&format!("{lua}{probe}")), "ok", "{case}");
    }
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
    assert_eq!(
        function_body(&lua, "box_input")
            .matches("__variant =")
            .count(),
        1
    );
    let out = run_luajit(&format!(
        r#"{lua}
for kind, tag in ipairs({{"A", "B", "C", "D", "E", "F"}}) do
    local a, b = box_input(kind), box_input(kind)
    assert(a.__variant == tag and a[1] == "payload")
    assert(not rawequal(a, b))
end
assert(rawequal(box_input(0), box_input(0)))
assert(box_input(-1) == nil)
print("ok")
"#
    ));
    assert_eq!(out, "ok");
}
