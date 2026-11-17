//! Codegen + runtime contract: a PAYLOAD-FREE enum is an `int` value globally (the
//! variant's declaration-order ordinal) -- construction lowers to an int const,
//! `match`/`==` int-compare, a `value` struct field stores a plain int, and a `{x}`
//! interp hole renders `<Enum>::<Variant>` from a static `__rock_enum_<E>_names`
//! table. No singleton tables, no `__tag`, no `by_tag` for such enums.
//!
//! A `value` struct holding the enum field therefore scalarizes into a C-layout
//! cdata with an int field (NOT a Lua table) and works across the scalar ABI
//! (by-value pass).
use crate::common::{emit_mir, emit_module, key, require_luajit, run_fn};

#[test]
fn widened_enum_record_match_rejects_a_field_missing_from_its_variant() {
    use crate::common::{try_emit_module, ws_with_stdlib};
    use bone_semantic::hir::VariantPayload;
    use ustr::Ustr;

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

    let ws = ws_with_stdlib(SRC);
    assert!(
        skull_mir::lower::EnumPlan::for_name(&ws, key(&ws, "Choice"))
            .unwrap()
            .flat()
            .is_some()
    );
    let enum_id = ws.enums_by_name[&key(&ws, "Choice")];
    let choice = &ws.modules[enum_id.module.index()].enums[enum_id.def];
    let variant = choice
        .variants
        .iter()
        .find(|variant| variant.name == "A")
        .unwrap();
    let VariantPayload::Record(fields) = &variant.payload else {
        panic!("Choice::A declaration must be a record variant");
    };
    assert!(fields.iter().all(|field| field.name != Ustr::from("ghost")));

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

/// Compile `src`, run its `fn_name` under luajit, and return the stdout.
fn run_skull_test(src: &str, fn_name: &str) -> String {
    require_luajit();
    run_fn(&emit_mir(src).lua, fn_name)
}

/// SENTINEL: a payload-free enum that ESCAPES into `any`
/// -- here passed directly to the generic `inspect(any)` -- loses its static type
/// and renders as the bare ordinal int, NOT `<Enum>::<Variant>`. A `{x}` interp
/// hole renders correctly (the static type is known at the hole; see
/// `strings/enum_format.rock`); this `inspect`/`any`-escape path is the
/// deferred box-on-escape-into-`any` work.
///
/// This pins the CURRENT (wrong) behavior. When increment 4 lands, the output
/// becomes "BlendMode::Additive" and this assertion goes red -- FLIP the expected
/// value to "BlendMode::Additive" and delete the sentinel framing in that commit.
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
