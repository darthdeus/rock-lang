use crate::common::{bone_rejections, emit_module, run_fn};

#[test]
fn record_variant_pattern_must_name_the_scrutinee_enum() {
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

    assert_eq!(
        bone_rejections(SRC),
        ["pattern for enum `Other` cannot match a value of type `Choice`"]
    );
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
    assert!(!lua.contains("__rock_box_mts[\"Cmd\"]"));
    assert!(!lua.contains("__rock_enum_BlendMode_names = {"));
    assert!(!lua.contains("__rock_enum_BlendMode_Additive ="));
    assert!(!lua.contains("__rock_box_mts[\"BlendMode\"]"));
    assert!(!lua.contains("_by_tag"));
    assert_eq!(run_fn(&lua, "t"), "5 Add 5");
}
