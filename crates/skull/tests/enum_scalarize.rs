use crate::common::{emit_mir, emit_module, run_fn};

#[test]
fn widened_enum_record_match_rejects_a_field_missing_from_its_variant() {
    use crate::common::try_emit_module;

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

    let emitted = try_emit_module(SRC);
    assert!(emitted.is_err());
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
    // c.z direct, and area(c) crosses the scalar-ABI boundary by value.
    print("{c.z} {s} {area(c)}")
}

fn main() { t() }
"#;

#[test]
fn value_struct_with_enum_field_has_cdef_int_not_table() {
    let lua = emit_module(SRC);
    // Cmd must get a real cdef ctor (scalarized), with `blend` an int field.
    assert!(lua.contains("Cmd_ct") && lua.contains("} Cmd;"));
    // The construction must NOT build a `{__ty = "Cmd", ...}` table.
    assert!(!lua.contains("__ty = \"Cmd\""));
    // A payload-free enum is an `int` globally: NO singleton tables, NO `__tag`,
    // NO `by_tag` (the dual-rep is gone). Its names table exists only for interp
    // stringify, which this program never does; `blend` stores a plain int.
    assert!(!lua.contains("__rock_enum_BlendMode_names = {"));
    assert!(!lua.contains("__rock_enum_BlendMode_Additive ="));
    assert!(!lua.contains("__ty = \"BlendMode\""));
    assert!(!lua.contains("_by_tag")); // Construct, `match c.blend`, and a by-value pass across the scalar ABI.
    assert_eq!(run_fn(&lua, "t"), "5 Add 5");
}

fn run_skull_test(src: &str, fn_name: &str) -> String {
    run_fn(&emit_mir(src).lua, fn_name)
}

#[test]
fn payload_free_enum_inspect_any_escape_increment4_sentinel() {
    const SRC: &str = r#"
enum BlendMode { Normal, Additive, Multiply }
@test
fn t() {
    let b = BlendMode::Additive
    print(inspect(b))
}
fn main() { t() }
"#;
    let stdout = run_skull_test(SRC, "t");
    assert_eq!(stdout.trim(), "1");
}
