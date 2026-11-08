//! modules-v1 nominal type emission. A v1 FUNCTION already had its
//! identifier form; a v1 TYPE still handed its DefKey to every name the backend
//! mints for it, and a DefKey is neither a Lua identifier nor a C one:
//!
//! ```text
//! } v1ty::a::V2;                                   -- ffi.cdef
//! v1ty::a::V2_ct = ffi.typeof("v1ty::a::V2")       -- Lua identifier + C type
//! __rock_enum_v1ty::a::Color_names = { ... }
//! return v1ty::a::V2_ct(v.x, v.y)                  -- a constructor call
//! ```
//!
//! So a v1 package holding a value struct or an enum compiled clean and then
//! died in LuaJIT. These controls execute the chunk.

use crate::common::{InlineOffGuard, V1Fragment, emit_v1_package, run_luajit};

const MAIN: &str = r#"
fn main() {
    print("s={a::sum()} c={a::color_name()} n={a::nb_sum()}")
}
"#;

const MOD_A: &str = r#"
mod a;

value V2 {
    x: float,
    y: float,
}

nobox value NV2 {
    x: float,
    y: float,
}

enum Color {
    Red,
    Blue,
}

pub fn sum() -> float {
    let v = V2 { x: 1.0, y: 2.0 }
    return v.x + v.y
}

pub fn nb_sum() -> float {
    let v = NV2 { x: 3.0, y: 4.0 }
    return v.x + v.y
}

pub fn color_name() -> string {
    let c = Color::Blue
    match c {
        Color::Red => { return "red" }
        Color::Blue => { return "blue" }
    }
}
"#;

fn package() -> skull::mir_emit::MirEmit {
    emit_v1_package(
        "v1ty",
        &[
            V1Fragment {
                file: "main.rock",
                src: MAIN,
            },
            V1Fragment {
                file: "a.rock",
                src: MOD_A,
            },
        ],
    )
}

#[test]
fn v1_value_struct_and_enum_emit_runnable_lua() {
    let emitted = package();
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "modules_v1_types");
    assert_eq!(out, "s=3 c=blue n=7");
}

/// The cdef, the `ffi.typeof` argument and the layout assertions name ONE C
/// type, and a DefKey cannot be that name. The DefKey still appears in display
/// text, which is what the digested names are NOT for.
#[test]
fn v1_nominal_types_mint_identifiers() {
    let emitted = package();
    for spelled in [
        "} v1ty::a::V2;",
        "ffi.typeof(\"v1ty::a::V2\")",
        "ffi.sizeof(\"v1ty::a::NV2\")",
        "__rock_nb_storage(\"v1ty::a::NV2\"",
        "ffi.metatype, \"v1ty::a::V2\"",
        "__rock_enum_v1ty::a::Color",
        "v1ty::a::V2_ct(",
    ] {
        assert!(
            !emitted.lua.contains(spelled),
            "a v1 DefKey must not reach an identifier or C-type position: `{spelled}`"
        );
    }
    assert!(
        emitted.lua.contains("\"v1ty::a::Color::Blue\""),
        "display text is the package-qualified path"
    );
}

/// The type table is a keyed registry the stdlib reads back, and one of its
/// readers builds `_G[<key> .. \"_ct\"]` -- so the key, the `__ty` tag a value
/// carries, and the emitted constructor must be ONE spelling.
#[test]
fn v1_type_table_key_matches_the_emitted_constructor() {
    let emitted = package();
    let digest = skull_mir::mangle::prefix_digest("v1ty::a");
    let key = format!("V2_{digest}");
    assert!(
        emitted
            .lua
            .contains(&format!("_G.__ROCK_TYPE_TABLE[\"{key}\"]")),
        "the type table is keyed on the emitted symbol"
    );
    assert!(
        emitted
            .lua
            .contains(&format!("{key}_ct = ffi.typeof(\"{key}\")")),
        "`_G[<key> .. \"_ct\"]` must resolve to the constructor the cdef declared"
    );
}

const PAYLOAD_MAIN: &str = r#"
fn main() {
    let r = a::Rank(4)
    print("{a::describe()} {a::rank_of(r)}")
}
"#;

const PAYLOAD_MOD: &str = r#"
mod a;

pub enum Move {
    Pass,
    Step(int),
    Jump { from: int, to: int },
}

pub newtype Rank(int)

pub fn rank_of(r: Rank) -> int {
    return r.0
}

pub fn describe() -> string {
    let stepped = Move::Step(2)
    let jumped = Move::Jump { from: 1, to: 3 }
    let passed = Move::Pass
    return "{tag(stepped)} {tag(jumped)} {tag(passed)}"
}

fn tag(m: Move) -> string {
    match m {
        Move::Step(d) => "step{d}",
        Move::Jump { from, to } => "jump{from}{to}",
        Move::Pass => "pass",
    }
}
"#;

/// A mixed enum's three variant forms, constructed and matched inside one v1
/// module: the unit variant is an interned `__rock_enum_<E>_<V>` singleton
/// (whose global name is minted from the enum's symbol), and the payload
/// variants are tagged tables. It also pins the two call-position lowerings a
/// v1 path needs: a path naming a tuple VARIANT must carry its arguments rather
/// than lower as the unit form, and a path naming a NEWTYPE is called by name
/// like its bare spelling, not as a value.
#[test]
fn v1_mixed_enum_variants_construct_and_match() {
    let emitted = emit_v1_package(
        "v1enum",
        &[
            V1Fragment {
                file: "main.rock",
                src: PAYLOAD_MAIN,
            },
            V1Fragment {
                file: "a.rock",
                src: PAYLOAD_MOD,
            },
        ],
    );
    let out = run_luajit(
        &format!("{}\nmain()\n", emitted.lua),
        "modules_v1_enum_payload",
    );
    assert_eq!(out, "step2 jump13 pass 4");
}

const SPEC_MAIN: &str = r#"
fn main() {
    print("total={a::total()}")
}
"#;

/// A stdlib generic instantiated at a v1 nominal: `for m in moves` specializes
/// `iter`/`next`/`len` at `Vec<v1spec::a::Step>`.
const SPEC_MOD_A: &str = r#"
mod a;

pub struct Step {
    cost: int,
}

pub fn total() -> int {
    let steps: [Step] = []
    steps.push(Step { cost: 3 })
    steps.push(Step { cost: 4 })
    let sum = 0
    for s in steps {
        sum = sum + s.cost
    }
    return sum + steps.len()
}
"#;

/// A specialized body's emitted symbol is built from the instantiation's
/// CANONICAL identity, and a v1 nominal's canonical name is its DefKey. The
/// two roles were the same string, so the symbol came out as
/// `iter_arr_T_spec_a_n18_v1spec::a::Step_0_z` -- `rock compile`
/// exited 0 and wrote a chunk LuaJIT refused with `')' expected near '::'`.
/// Converting `libs/chess-core` hit this on the first `for m in moves`.
#[test]
fn a_generic_specialized_at_a_v1_nominal_emits_runnable_lua() {
    // Keep the specialized definition available for the identifier checks.
    let _inline = InlineOffGuard::on();
    let emitted = emit_v1_package(
        "v1spec",
        &[
            V1Fragment {
                file: "main.rock",
                src: SPEC_MAIN,
            },
            V1Fragment {
                file: "a.rock",
                src: SPEC_MOD_A,
            },
        ],
    );
    let digest = skull_mir::mangle::prefix_digest("v1spec::a");
    let nominal = format!("Step_{digest}");
    let encoded = format!("spec_a_n{}_{}_0_z", nominal.len(), nominal);
    assert!(
        !emitted.lua.contains("spec_a_n15_v1spec::a::Step_0_z"),
        "a specialized body's SYMBOL must not carry a DefKey"
    );
    assert!(
        emitted
            .lua
            .contains(&format!("function vec_len_arr_T_{encoded}(")),
        "the specialized definition takes the nominal's identifier form"
    );
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "modules_v1_spec");
    assert_eq!(out, "total=9");
}

#[test]
fn record_codec_metadata_keeps_primitive_payloads_and_collection_elements() {
    let lua = crate::common::emit_module(
        r#"
enum Entry { Empty, Text(string), Pair(int, float), Names([string]) }
struct Record { entries: [Entry], title: string?, weights: Map<string, float> }
inline(lua) fn verify(value: any) = """
    local fields = _G.__ROCK_TYPE_TABLE[value.__ty].fields
    assert(fields[2].desc.k == 'opt' and fields[2].desc.inner.k == 'string')
    assert(fields[3].desc.key.k == 'string' and fields[3].desc.val.k == 'float')
    local variants = _G.__ROCK_TYPE_TABLE[fields[1].desc.elem.name].variant_desc
    assert(variants.Empty.pos and #variants.Empty.pos == 0)
    assert(variants.Text.pos[1].k == 'string')
    assert(variants.Pair.pos[1].k == 'int' and variants.Pair.pos[2].k == 'float')
    assert(variants.Names.pos[1].k == 'vec' and variants.Names.pos[1].elem.k == 'string')
"""
fn main() {
    verify(Record { entries: [Entry::Text("hello")], title: null, weights: map_new() })
    print("ok")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "record_codec_metadata"),
        "ok"
    );
}

#[test]
fn typed_json_records_roundtrip_and_reject_malformed_fields() {
    let lua = crate::common::emit_module(
        r#"
enum Event { Empty, Text(string), Pair(int, float), Names([string]) }
value Tint { red: float, visible: bool }
value Change { event: Event, count: int }
struct Record {
    rows: [Change],
    title: string?,
    weights: Map<string, float>,
    tint: Tint,
}
fn main() {
    let codec = JsonRecordCodec {}
    let weights: Map<string, float> = map_new()
    weights["alpha"] = 1.25
    let source = Record { rows: [Change { event: Event::Empty, count: 1 },
        Change { event: Event::Pair(5, 2.5), count: 2 },
        Change { event: Event::Names(["a", "b"]), count: 3 }],
        title: null, weights, tint: Tint { red: 0.75, visible: false } }
    let wire = json_record_encode(codec, source)
    let copy = json_record_decode(codec, json_decode(json_encode(wire)).unwrap(), source).unwrap()
    assert(copy == source)
    assert(!same_ref(copy, source))
    copy.rows[0] = Change { event: Event::Text("changed"), count: 8 }
    assert(source.rows[0].count == 1)
    let fields = json_as_object(wire).unwrap()
    fields["rows"] = json_str("wrong")
    assert(json_record_decode(codec, wire, source) == null)
    fields["rows"] = json_array([json_object(map_new())])
    assert(json_record_decode(codec, wire, source) == null)
    fields["rows"] = json_array([])
    fields["weights"] = json_str("wrong")
    assert(json_record_decode(codec, wire, source) == null)
    fields["weights"] = json_object(map_new())
    fields["title"] = json_int(4)
    assert(json_record_decode(codec, wire, source) == null)
    print("ok")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "typed_json_records"),
        "ok"
    );
}

#[test]
fn typed_json_uses_declared_scalar_enum_adapters_and_embedded_values() {
    let lua = crate::common::emit_module(
        r#"
enum Kind { First, Second }
nobox value Tint { red: float, visible: bool }
nobox value Point { x: int, y: int }
nobox value Marker { point: Point, tint: Tint }
value Key { namespace: string, name: string }
nobox value Label { name: string, marker: Marker?, key: Key }
struct Packet { kind: Kind, tint: Tint, spare: Tint?, marker: Marker, backup: Marker?, rows: [Label] }
fn marker_encode(value: Marker) -> JsonValue {
    return json_array([json_int(value.point.x), json_int(value.point.y),
        json_float(value.tint.red), json_bool(value.tint.visible)])
}
fn marker_decode(value: JsonValue) -> Marker? {
    let fields = json_as_array(value) ?? return null
    if fields.len() != 4 { return null }
    return Marker { point: Point { x: json_as_int(fields[0]) ?? return null,
        y: json_as_int(fields[1]) ?? return null },
        tint: Tint { red: json_as_float(fields[2]) ?? return null,
            visible: json_as_bool(fields[3]) ?? return null } }
}
fn kind_encode(value: Kind) -> JsonValue { return json_str(if value == Kind::First { "a" } else { "b" }) }
fn kind_decode(value: JsonValue) -> Kind? {
    let key = json_as_str(value) ?? return null
    if key == "a" { return Kind::First }
    if key == "b" { return Kind::Second }
    return null
}
fn main() {
    let key = Key { namespace: "sample", name: "record" }
    let marker = Marker { point: Point { x: 7, y: -3 }, tint: Tint { red: 0.25, visible: false } }
    let codec = JsonRecordCodec { adapters: [json_record_adapter(Kind::First, kind_encode, kind_decode),
        json_record_adapter(marker, marker_encode, marker_decode)] }
    let source = Packet { kind: Kind::Second, tint: Tint { red: 0.5, visible: false }, spare: null, marker, backup: marker,
        rows: [Label { name: "first", marker, key }, Label { name: "empty", marker: null, key }] }
    let wire = json_record_encode(codec, source)
    assert(json_as_str(json_field(wire, "kind").unwrap()).unwrap() == "b")
    let copy = json_record_decode(codec, wire, source).unwrap()
    assert(copy == source)
    assert(json_encode(json_field(wire, "marker").unwrap()) == "[7,-3,0.25,false]")
    assert(copy.backup.unwrap().point.y == -3 && !copy.backup.unwrap().tint.visible)
    assert(copy.rows.len() == 2 && copy.rows[0].marker.unwrap().point.x == 7)
    assert(copy.rows[1].marker == null && copy.rows[1].key == key)
    copy.rows.push(Label { name: "added", marker, key })
    copy.rows[0] = Label { name: "changed", marker: null, key }
    assert(source.rows.len() == 2 && source.rows[0].name == "first")
    assert(copy.rows[2].marker.unwrap().tint.red == 0.25)
    json_as_object(wire).unwrap()["backup"] = json_null()
    assert(json_record_decode(codec, wire, source).unwrap().backup == null)
    json_as_object(wire).unwrap()["marker"] = json_str("invalid")
    assert(json_record_decode(codec, wire, source) == null)
    json_as_object(wire).unwrap()["marker"] = marker_encode(marker)
    json_as_object(wire).unwrap()["kind"] = json_str("unknown")
    assert(json_record_decode(codec, wire, source) == null)
    print("ok")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "typed_json_adapters"),
        "ok"
    );
}

#[test]
fn typed_json_flat_enum_fields_and_widened_payloads() {
    let lua = crate::common::emit_module(
        r#"
nobox value Tint { red: float, visible: bool }
enum Event { Empty, Text(string), Colored(string, Tint), Pair(string, int) }
enum Envelope { Wrapped(Event), Other(string) }
struct Record { event: Event, envelope: Envelope, optional: Event? }
fn main() {
    let codec = JsonRecordCodec {}
    for event in [Event::Empty, Event::Text("hello"), Event::Colored("red", Tint { red: 0.5, visible: false }), Event::Pair("count", 7)] {
        let source = Record { event, envelope: Envelope::Wrapped(event), optional: if event == Event::Empty { null } else { event } }
        let wire = json_record_encode(codec, source)
        let copy = json_record_decode(codec, json_decode(json_encode(wire)).unwrap(), source).unwrap()
        assert(copy == source)
        assert(json_encode(json_record_encode(codec, copy)) == json_encode(wire))
        if let Event::Colored(label, tint) = copy.event {
            assert(label == "red" && tint.red == 0.5 && !tint.visible)
        }
    }
    print("ok")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "typed_json_flat"),
        "ok"
    );
}

#[test]
fn typed_json_rejects_cycles_and_unsupported_storage() {
    let lua = crate::common::emit_module(
        r#"
struct Node { next: Node? = null }
value Point { x: float }
struct NativeRows { rows: [Point] }
struct NumericKeys { entries: Map<int, string> }
inline(lua) fn fails(run: fn() -> JsonValue, message: string) = """
    local ok, err = pcall(run)
    assert(not ok and tostring(err):find(message, 1, true), tostring(err))
"""
fn main() {
    let codec = JsonRecordCodec {}
    let node = Node {}
    node.next = node
    fails(() => json_record_encode(codec, node), "cannot encode cycles")
    fails(() => json_record_encode(codec, NativeRows { rows: [Point { x: 1.0 }] }), "native value arrays")
    fails(() => json_record_encode(codec, NumericKeys { entries: map_new() }), "require string keys")
    print("ok")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "typed_json_rejections"),
        "ok"
    );
}
