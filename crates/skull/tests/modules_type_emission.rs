//! modules-v1 nominal type emission (plan M4). Slice 1 gave a v1 FUNCTION its
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

use crate::common::{emit_v1_package, run_luajit, V1Fragment};

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
            V1Fragment { file: "main.rock", src: MAIN },
            V1Fragment { file: "a.rock", src: MOD_A },
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
/// type, and a DefKey cannot be that name. The DefKey still appears in identity
/// strings and in display text, which is what the digested names are NOT for.
#[test]
fn v1_nominal_types_mint_identifiers_and_keep_the_key_for_identity() {
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
        emitted.lua.contains("identity=\"value:v1ty::a::V2\""),
        "the type's own identity stays its DefKey"
    );
    assert!(
        emitted.lua.contains("\"v1ty::a::Color::Blue\""),
        "display text stays the readable path"
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
        emitted.lua.contains(&format!("_G.__ROCK_TYPE_TABLE[\"{key}\"]")),
        "the type table is keyed on the emitted symbol"
    );
    assert!(
        emitted.lua.contains(&format!("{key}_ct = ffi.typeof(\"{key}\")")),
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
            V1Fragment { file: "main.rock", src: PAYLOAD_MAIN },
            V1Fragment { file: "a.rock", src: PAYLOAD_MOD },
        ],
    );
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "modules_v1_enum_payload");
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
/// `iter_arr_T_ret_VecIter_T_spec_a_n18_v1spec::a::Step_0_z` -- `rock compile`
/// exited 0 and wrote a chunk LuaJIT refused with `')' expected near '::'`.
/// Converting `libs/chess-core` hit this on the first `for m in moves`.
#[test]
fn a_generic_specialized_at_a_v1_nominal_emits_runnable_lua() {
    let emitted = emit_v1_package(
        "v1spec",
        &[
            V1Fragment { file: "main.rock", src: SPEC_MAIN },
            V1Fragment { file: "a.rock", src: SPEC_MOD_A },
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
        emitted.lua.contains(&format!("function vec_len_arr_T_ret_int_{encoded}(")),
        "the specialized definition takes the nominal's identifier form"
    );
    assert!(
        emitted.lua.contains("identity = \"named:<stdlib:unified.rock>::vec_len<a_n15_v1spec::a::Step_0_z>\""),
        "the instance's IDENTITY keeps the DefKey -- that is the half that must not move"
    );
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "modules_v1_spec");
    assert_eq!(out, "total=9");
}
