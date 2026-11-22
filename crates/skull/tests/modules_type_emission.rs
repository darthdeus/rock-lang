use crate::common::{emit_v1_package, inline_off_guard, run_main};

const MAIN: &str = r#"
fn main() {
    print("s={a::sum()} c={a::color_name()} n={a::nb_sum()} t={a::color_text()}")
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

pub fn color_text() -> string {
    let c = Color::Blue
    return "{c}"
}
"#;

fn package() -> skull::mir_emit::MirEmit {
    emit_v1_package("v1ty", &[("main.rock", MAIN), ("a.rock", MOD_A)])
}

#[test]
fn v1_type_table_key_matches_the_emitted_constructor() {
    let emitted = package();
    let digest = skull_mir::mangle::prefix_digest("v1ty::a");
    let key = format!("V2_{digest}");
    assert!(
        emitted
            .lua
            .contains(&format!("_G.__ROCK_TYPE_TABLE[\"{key}\"]"))
    );
    assert!(
        emitted
            .lua
            .contains(&format!("{key}_ct = ffi.typeof(\"{key}\")"))
    );
}

const SPEC_MAIN: &str = r#"
fn main() {
    print("total={a::total()}")
}
"#;

const SPEC_MOD_A: &str = r#"
mod a;

pub struct Step {
    cost: int,
}

pub fn total() -> int {
    let steps: [Step] = []
    steps.push(Step { cost: 3 })
    steps.push(Step { cost: 4 })
    if steps.is_empty() { return 0 }
    let sum = 0
    for s in steps {
        sum = sum + s.cost
    }
    return sum + steps.len()
}
"#;

#[test]
fn a_generic_specialized_at_a_v1_nominal_emits_runnable_lua() {
    let _inline = inline_off_guard();
    let emitted = emit_v1_package(
        "v1spec",
        &[("main.rock", SPEC_MAIN), ("a.rock", SPEC_MOD_A)],
    );
    let digest = skull_mir::mangle::prefix_digest("v1spec::a");
    let nominal = format!("Step_{digest}");
    let encoded = format!("spec_a_n{}_{}_0_z", nominal.len(), nominal);
    assert!(!emitted.lua.contains("spec_a_n15_v1spec::a::Step_0_z"));
    assert!(
        emitted
            .lua
            .contains(&format!("function is_empty_arr_T_{encoded}("))
    );
    assert_eq!(run_main(&emitted.lua), "total=9");
}

#[test]
fn user_json_field_keeps_its_nominal_descriptor() {
    let emitted = emit_v1_package(
        "presence",
        &[
            ("main.rock", "fn main() { a::verify() }"),
            (
                "a.rock",
                r#"
mod a;
value JsonField { value: int }
@lua_named struct Holder { field: JsonField }
inline(lua) fn check(value: Holder) = """
        local desc = __rock_field_declaration(value).fields[1].desc
        assert(desc.k == 'nom')
        assert(_G.__ROCK_TYPE_TABLE[desc.name].fields[1].name == 'value')
        print('ok')
"""
pub fn verify() {
    check(Holder { field: JsonField { value: 7 } })
}
"#,
            ),
        ],
    );
    assert_eq!(run_main(&emitted.lua), "ok");
}
