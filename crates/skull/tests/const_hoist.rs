//! Codegen-shape contract: an all-constant `MakeValueStruct` is hoisted to ONE
//! function-owned module-scope constant, not rebuilt at each use.
//!
//! The application HUD/text path's `make_text_vertex` writes a nested
//! `outline_color: FixVec2 { 0.0, 0.0 }` into every glyph vertex slot. opt #3 does
//! not recurse into nested value-struct fields, so this becomes a literal
//! `slot.outline_color = FixVec2_ct(0.0, 0.0)` per store. On a trace it sinks; in the
//! interpreter (application's aborted HUD loops) it allocates a fresh cdata every store --
//! the entire skull-vs-rock interpreter-path gap on the text loop (skull 8.25 vs
//! rock 6.70 KB per draw_text call; `benchmarks/opt_showcase/07_text_interp_alloc.rock`).
//! rock hoists an all-constant value-struct literal to a module-scope constant
//! (`__rock_const_FixVec2_...`); skull emits `__skull_kc_<fn>_<g> = <Name>_ct(..)`
//! once and references it. Value structs are immutable, so one shared cdata is
//! sound, built once at chunk load (0 alloc on BOTH the traced and interpreted
//! paths -- unlike a per-call function-entry local, which defeats SINK).
//!
//! Emitted in the function's OWN chunk (not shared scaffolding), so an incremental
//! body-edit overlay carries the def with the function it serves.
use crate::common::{emit_module, emit_v1_package, function_body, run_luajit, V1Fragment};

fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

// Two nested const FixVec2 fields, both a DISTINCTIVE `FixVec2 { 3.0, 7.0 }`, written
// into a NativeBuffer<Outer> slot. opt #3 lowers the store to
// `slot.a = FixVec2_ct(3, 7); slot.b = FixVec2_ct(3, 7)`. Hoisting + dedup makes that
// one module-scope `__skull_kc_..._N = FixVec2_ct(3.0, 7.0)` referenced twice.
const TWO_CONST_NESTED: &str = r#"
value FixVec2 { x: float, y: float }
value Outer { a: FixVec2, b: FixVec2, c: float }

// A `[Outer]` Vec, NOT a NativeBuffer: `Outer` has 5 leaves, over
// `MAX_COLUMN_LEAVES`, so the Vec keeps boxed elements and the ctor genuinely
// has to be built. (A NativeBuffer element is leaf-addressed now and writes the
// constant leaves straight through its views, so it builds no ctor to hoist --
// which would make this test vacuous rather than exercise hoisting.)
fn fill(out: [Outer], n: int) {
    for i in 0..n {
        out.push(Outer { a: FixVec2 { x: 3.0, y: 7.0 }, b: FixVec2 { x: 3.0, y: 7.0 }, c: i as float })
    }
}

fn main() {
    let out: [Outer] = []
    fill(out, 4)
    print("{out.len}")
}
"#;

#[test]
fn const_value_struct_ctor_is_hoisted_to_module_scope() {
    let lua = emit_module(TWO_CONST_NESTED);
    // The distinctive ctor is BUILT exactly once, in a module-scope hoist def.
    let builds = count(&lua, "FixVec2_ct(3.0, 7.0)");
    assert_eq!(
        builds, 1,
        "the all-constant FixVec2 {{3,7}} must be built once (one hoist def), found {builds}:\n{lua}"
    );
    // The def is a `__skull_kc_*` global assignment, not inside the loop.
    assert!(
        lua.contains("__skull_kc_") && lua.contains("= FixVec2_ct(3.0, 7.0)"),
        "expected a module-scope `__skull_kc_* = FixVec2_ct(3.0, 7.0)` hoist def:\n{lua}"
    );
    // The hot function rebuilds nothing -- it references the global instead.
    let body = function_body(&lua, "fill");
    assert_eq!(
        count(body, "FixVec2_ct("),
        0,
        "fill must reference the hoisted global, not rebuild the ctor:\n{body}"
    );
    assert!(
        body.contains("__skull_kc_"),
        "fill must reference the hoisted const global:\n{body}"
    );
}

// A package value's DefKey is qualified (`pkg::a::Id`); the hoist def must name
// its ctor by the emitted type symbol, as an inline ctor does, or LuaJIT reads
// the `::` as a label and rejects the chunk.
// The constant `a::Pt` is a nested field of a boxed `Seg` element, the
// read-only consumer `TWO_CONST_NESTED` hoists (`==` compares leaves and builds
// no box).
const PACKAGE_MAIN: &str = r#"
value Seg { a: a::Pt, b: a::Pt, c: float }
fn main() {
    let out: [Seg] = []
    for i in 0..2 {
        out.push(Seg { a: a::Pt { x: 3.0, y: 7.0 }, b: a::make(i as float), c: 1.0 })
    }
    print("{a::label(out[1].a)} {a::label(out[1].b)}")
}
"#;

const PACKAGE_MOD_A: &str = r#"
mod a;

pub value Pt {
    x: float,
    y: float,
}

pub fn make(x: float) -> Pt {
    return Pt { x, y: x }
}

pub fn label(p: Pt) -> string {
    return "{p.x}/{p.y}"
}
"#;

#[test]
fn hoisted_package_value_ctor_uses_the_emitted_type_symbol() {
    let emitted = emit_v1_package(
        "kcpkg",
        &[
            V1Fragment { file: "main.rock", src: PACKAGE_MAIN },
            V1Fragment { file: "a.rock", src: PACKAGE_MOD_A },
        ],
    );
    assert!(
        emitted.lua.contains("__skull_kc_"),
        "the all-constant package value literal must be hoisted:\n{}",
        emitted.lua
    );
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "const_hoist_package_value");
    assert_eq!(out, "3/7 1/1");
}
