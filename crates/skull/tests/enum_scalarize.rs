//! Codegen + runtime contract: a PAYLOAD-FREE enum is an `int` value globally (the
//! variant's declaration-order ordinal) -- construction lowers to an int const,
//! `match`/`==` int-compare, a `value` struct field stores a plain int, and a `{x}`
//! interp hole renders `<Enum>::<Variant>` from a static `__rock_enum_<E>_names`
//! table. No singleton tables, no `__tag`, no `by_tag` for such enums.
//!
//! A `value` struct holding the enum field therefore scalarizes into a C-layout
//! cdata with an int field (NOT a Lua table) and works across the scalar ABI
//! (by-value pass). See `docs/skull-enum-value-types.md`.
use crate::common::{emit_mir, emit_module, require_luajit};

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
    assert!(
        lua.contains("Cmd_ct") && lua.contains("} Cmd;"),
        "Cmd must be a C-layout cdata (a `Cmd_ct` ctor + cdef), not a table:\n{}",
        lua.split("\nfunction ").next().unwrap_or(&lua)
    );
    // The construction must NOT build a `{__ty = "Cmd", ...}` table.
    assert!(
        !lua.contains("__ty = \"Cmd\""),
        "Cmd must not be emitted as a Lua table:\n{lua}"
    );
    // A payload-free enum is an `int` globally: NO singleton tables, NO `__tag`,
    // NO `by_tag` (the dual-rep is gone). Only the ordinal->display-name table for
    // interp stringify is emitted; its `blend` field stores a plain int.
    assert!(
        lua.contains("__rock_enum_BlendMode_names = {"),
        "payload-free enum must emit a names table for stringify:\n{lua}"
    );
    assert!(
        !lua.contains("__rock_enum_BlendMode_Additive ="),
        "payload-free enum must NOT emit singleton tables:\n{lua}"
    );
    // (`__tag` as a bare substring survives in the stdlib `inspect` key-filter;
    // the dual-rep was the `__tag = <ord>` singleton FIELD + the `by_tag` array.)
    assert!(!lua.contains("__tag = "), "the dead `__tag =` singleton field must be gone:\n{lua}");
    assert!(!lua.contains("_by_tag"), "the dead `by_tag` array must be gone:\n{lua}");
}

/// Runtime: construct + `match c.blend` + by-value pass `area(c)` (was crashing
/// with `Cmd_ct` nil before scalarization). Can't live in `tests/` -- rock
/// rejects a value struct with an enum field -- so it runs skull-only here.
#[test]
fn value_struct_with_enum_field_runs_correctly() {
    require_luajit();
    let emit = emit_mir(SRC);
    let mut prog = emit.lua.clone();
    // Capture print, run the @test fn (`@test` names are unmangled), echo output.
    prog.push_str(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         t()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    );
    let dir = std::env::temp_dir();
    let path = dir.join(format!("skull_enum_rt_{}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "luajit failed:\nstderr: {}\n--- lua tail ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        prog.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    );
    assert!(
        stdout.contains("5 Add 5"),
        "expected '5 Add 5', got stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Compile `src`, append a print-capturing harness that calls `fn_name`, run under
/// luajit, and return the captured stdout (panicking if luajit is unavailable).
fn run_skull_test(src: &str, fn_name: &str) -> String {
    require_luajit();
    let mut prog = emit_mir(src).lua.clone();
    prog.push_str(&format!(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={{}} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         {fn_name}()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    ));
    let path = std::env::temp_dir().join(format!("skull_enum_sentinel_{}.lua", std::process::id()));
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "luajit failed:\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// SENTINEL (`docs/test-sentinels.md`): a payload-free enum that ESCAPES into `any`
/// -- here passed directly to the generic `inspect(any)` -- loses its static type
/// and renders as the bare ordinal int, NOT `<Enum>::<Variant>`. A `{x}` interp
/// hole renders correctly (the static type is known at the hole; see
/// `enum_payload_free_stringify.rock`); this `inspect`/`any`-escape path is the
/// deferred box-on-escape-into-`any` work (increment 4 in
/// `docs/skull-enum-value-types.md`).
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
    assert_eq!(
        stdout.trim(),
        "1",
        "SENTINEL: `inspect(payload_free_enum)` currently leaks the ordinal int. \
         When box-on-escape-into-`any` (increment 4) lands this becomes \
         \"BlendMode::Additive\" -- flip the expected value then. got={stdout:?}"
    );
}
