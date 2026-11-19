use crate::common::{emit_module, run_fn, try_emit_module};

#[test]
fn widened_enum_record_match_rejects_a_field_missing_from_its_variant() {
    const SRC: &str = r#"
enum Choice { A { left: int, shared: int }, B { right: string, shared: string } }
enum Other { A { ghost: int } }
fn read() -> int {
    return match (Choice::A { left: 7, shared: 13 }) {
        Other::A { ghost } => ghost,
        Choice::A { left } => left,
        Choice::B { right } => right.len(),
    }
}
fn main() { print(read()) }
"#;

    assert!(try_emit_module(SRC).is_err());
}

const SRC: &str = r#"
enum BlendMode { Alpha, Additive, Multiply }
value Cmd { z: int, blend: BlendMode }

@nomangle inline(lua) fn __probe(s: int) = "_G.__sink = s"

fn area(c: Cmd) -> int { return c.z }

@test(output = "5 Add 5")
fn t() {
    let c = Cmd { z: 5, blend: BlendMode::Additive }
    let s = match c.blend {
        BlendMode::Alpha => "A"
        BlendMode::Additive => "Add"
        BlendMode::Multiply => "M"
    }
    print("{c.z} {s} {area(c)}")
}

fn main() { t() }
"#;

#[test]
fn value_struct_with_enum_field_has_cdef_int_not_table() {
    let lua = emit_module(SRC);
    assert!(lua.contains("Cmd_ct") && lua.contains("} Cmd;"));
    assert!(!lua.contains("__ty = \"Cmd\""));
    assert!(!lua.contains("__rock_enum_BlendMode_names = {"));
    assert!(!lua.contains("__rock_enum_BlendMode_Additive ="));
    assert!(!lua.contains("__ty = \"BlendMode\""));
    assert!(!lua.contains("_by_tag"));
    assert_eq!(run_fn(&lua, "t"), "5 Add 5");
}

#[test]
fn payload_free_enum_inspects_as_its_ordinal() {
    const SRC: &str = r#"
enum BlendMode { Normal, Additive, Multiply }
@test
fn t() {
    let b = BlendMode::Additive
    print(inspect(b))
}
fn main() { t() }
"#;
    assert_eq!(run_fn(&emit_module(SRC), "t"), "1");
}
