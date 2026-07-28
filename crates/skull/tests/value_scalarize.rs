//! Codegen-shape contract: a `value` struct whose only uses are field reads
//! must NOT survive as a `<name>_ct(...)` cdata after inlining.
//!
//! skull keeps value structs unboxed (`ValueRepr::Group`) WITHIN a body, but a
//! `@pure`/`@inline` helper that returns a value struct is lowered standalone,
//! packs its return into a `MakeValueStruct` cdata, and the MIR inliner splices
//! that packed cdata into the caller. The field reads then become
//! `Field`-off-`MakeValueStruct`, and the cdata allocates once per iteration --
//! LuaJIT cannot sink it across the side-trace split of a data-dependent branch
//! (the mag `mag.grid.floors_walls` regression). The SROA forwarding pass in
//! `skull-transform` forwards those reads to the ctor operands so DCE drops the
//! cdata. rock scalarizes the same value struct away; this pins skull to parity.

mod common;
use common::{emit_module, function_body};

const VALUE_FIELD_LOOP: &str = r#"
value FixVec2 { x: float, y: float }
value IVec2 { x: int, y: int }

@pure fn fv(x: float, y: float) -> FixVec2 { return FixVec2 { x: x, y: y } }
@pure fn iv(x: int, y: int) -> IVec2 { return IVec2 { x: x, y: y } }
@pure fn tile_center(g: IVec2) -> FixVec2 {
    return fv(((g.x + g.y) as float), ((g.x - g.y) as float))
}

fn hot(w: int, h: int) -> float {
    let acc = 0.0
    for gy in 0..h {
        for gx in 0..w {
            let pos = tile_center(iv(gx, gy))
            let kind = ((gx * 7) + (gy * 13)) % 3
            if kind == 0 {
                acc = (acc + (pos.x * 2.0)) - pos.y
            } else if kind == 1 {
                acc = (acc - pos.y) + pos.x
            } else {
                acc = (acc + pos.x) + pos.y
            }
        }
    }
    return acc
}

fn main() {
    let _ = hot(2, 2)
}
"#;

/// A value struct used BOTH by field reads AND as a whole struct (e.g. `==`)
/// must keep its cdata: the whole-struct use escapes, so SROA must not split the
/// cell. The hazard is a half-rewrite -- forwarding the field reads to per-field
/// cells while the escaping use blocks the store expansion, leaving the field
/// cells unstored (uninitialized reads). Pins the `is_cand` re-check in
/// `field_rewrites`. Runs under luajit so the uninit read is actually caught.
const VALUE_FIELD_AND_WHOLE_USE: &str = r#"
value Color { r: float, g: float, b: float, a: float }
value FixVec3 { x: float, y: float, z: float }

// `c` is field-read into a local (`let r = c.r`) AND used as a whole struct
// (`c == c2`). The whole-struct use escapes, so the cell must NOT be split and the
// `Color_ct` cdata must be KEPT for the `==`. The field read `c.r` itself folds to
// its construction operand `k` (a `value` struct is IMMUTABLE, so `Field(Color_ct,
// r)` == the `r` operand -- the copy-prop forwarding), independent of the kept
// cdata. The half-rewrite hazard this pins: forwarding the read to a never-stored
// per-field CELL (an uninitialized read) -- the runtime check below catches that.
//
// Field values depend on the param `k` so the ctors are NOT all-constant -- this
// isolates opt #1 (SROA keeps the cdata) from opt #7 (const hoisting would lift
// an all-constant ctor to a module-scope global, also leaving the body with no
// `Color_ct(`; covered separately in `const_hoist.rs`).
fn coexist(k: float) -> float {
    let c = Color { r: k, g: 0.5, b: 0.0, a: 1.0 }
    let v = FixVec3 { x: k, y: 20.0, z: 30.0 }
    let r = c.r
    let z = v.z
    let c2 = Color { r: k, g: 0.5, b: 0.0, a: 1.0 }
    let eq = if c == c2 { 1.0 } else { 0.0 }
    return (r + z) + eq
}

@nomangle inline(lua) fn __out(x: float) = "io.write(tostring(x))"
fn main() {
    __out(coexist(2.0))   // 2.0 (c.r) + 30.0 (v.z) + 1.0 (c == c2) = 33
}
"#;

#[test]
fn value_struct_with_whole_use_keeps_cdata() {
    let lua = emit_module(VALUE_FIELD_AND_WHOLE_USE);
    let body = function_body(&lua, "coexist");
    // The whole-struct `==` use escapes, so the cdata must be KEPT (not SROA'd).
    assert!(
        body.contains("Color_ct("),
        "a value struct used as a whole (==) must keep its cdata:\n{body}"
    );
    // The separate field read `c.r` folds to its construction operand (no `.r`
    // projection needed), since the value struct is immutable.
    assert!(
        !body.contains(".r"),
        "the immutable field read should fold to the operand, not project `.r`:\n{body}"
    );
    // Runtime guard against a BAD fold (e.g. an uninitialized-cell read): the value
    // must be exactly 33.
    if std::process::Command::new("luajit").arg("-v").output().is_ok() {
        let path = std::env::temp_dir().join(format!("skull_coexist_{}.lua", std::process::id()));
        std::fs::write(&path, format!("{lua}\nmain()\n")).expect("write");
        let out = std::process::Command::new("luajit").arg(&path).output().expect("run");
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success(), "luajit failed:\n{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "33", "coexist must compute correctly");
    }
}

#[test]
fn value_struct_field_reads_dont_materialize_cdata() {
    let lua = emit_module(VALUE_FIELD_LOOP);
    let hot = function_body(&lua, "hot");
    // The inlined value structs are consumed only by field reads, so neither
    // ctor should survive in the hot loop -- SROA splits the cells into scalars.
    assert!(
        !hot.contains("FixVec2_ct("),
        "FixVec2 cdata ctor survived inlining in hot() (should scalarize away):\n{hot}"
    );
    assert!(
        !hot.contains("IVec2_ct("),
        "IVec2 cdata ctor survived inlining in hot() (should scalarize away):\n{hot}"
    );
}

/// Storing a value-struct literal into a `NativeBuffer<value>` element writes
/// the element's LEAVES through typed views -- no whole cdata copy
/// (`__nb_checked_set(i, Vertex_ct(..))`), and no slot reference either.
///
/// This supersedes the slot-write form (`slot = buf[i]; slot.f = ..`) that a
/// previous arc landed. That form sank to 0 B/element ON TRACE, but paid a
/// 24 B slot reference plus a nested cdata per nested field everywhere sinking
/// does not happen; leaf addressing is 0 on both. Measured on this repo's own
/// benchmarks: `nativebuffer_value_store` 72.9 -> 0.9 B/vertex under
/// `jit.off()`, unchanged at 0 with the JIT on.
/// See docs/native-buffer-leaf-addressing-plan.md.
const NATIVE_BUFFER_VALUE_STORE: &str = r#"
value V2 { x: float, y: float }
value Vert { pos: V2, uv: V2, w: float }

@nomangle inline(lua) fn __nb_alloc_Vert(n: int) -> Ptr<Vert> = 'return ffi.new("Vert[?]", n)'
fn nb_vert(n: int) -> NativeBuffer<Vert> { return NativeBuffer { ptr: __nb_alloc_Vert(n), len: n } }

@pure fn mk(x: float) -> Vert { return Vert { pos: V2 { x: x, y: x }, uv: V2 { x: x, y: x }, w: x } }

fn fill(buf: NativeBuffer<Vert>, n: int) {
    for i in 0..n {
        buf[i] = mk(i as float)
    }
}

fn main() {
    let buf = nb_vert(4)
    fill(buf, 4)
}
"#;

#[test]
fn native_buffer_value_store_writes_leaves_through_views() {
    let lua = emit_module(NATIVE_BUFFER_VALUE_STORE);
    let body = function_body(&lua, "fill");
    // Leaf form: the element is bounds-checked ONCE by index, then each leaf is
    // written through its own view -- no slot reference is taken at all.
    assert!(
        body.contains("__nb_checked_index("),
        "NativeBuffer<value> store should bounds-check the index:\n{body}"
    );
    assert!(
        !body.contains("__nb_checked_get("),
        "leaf addressing must not take a slot reference:\n{body}"
    );
    // NOT the packed form (build the whole cdata and copy it in).
    assert!(
        !body.contains("__nb_checked_set("),
        "NativeBuffer<value> store should not build+copy a whole cdata:\n{body}"
    );
    // No ctor survives -- neither the element's nor a nested field's.
    for ct in ["Vert_ct(", "V2_ct("] {
        assert!(!body.contains(ct), "`{ct}` must not survive the store:\n{body}");
    }
    // The block-scoped `__nbslot` local this used to require existed to keep the
    // slot pointer cdata off a loop-carried PHI so LuaJIT could sink it. Leaf
    // addressing creates no slot cdata to sink in the first place.
    assert!(
        !body.contains("local __nbslot"),
        "leaf addressing should not need a slot local:\n{body}"
    );
}

const NATIVE_BUFFER_SINGLE_FIELD: &str = r#"
value V1 { x: float }

@nomangle inline(lua) fn __nb_alloc_V1(n: int) -> Ptr<V1> = 'return ffi.new("V1[?]", n)'
fn nb_v1(n: int) -> NativeBuffer<V1> { return NativeBuffer { ptr: __nb_alloc_V1(n), len: n } }

fn fill(buf: NativeBuffer<V1>, n: int) {
    for i in 0..n {
        buf[i] = V1 { x: i as float }
    }
}

fn main() {
    let buf = nb_v1(4)
    fill(buf, 4)
}
"#;

#[test]
fn native_buffer_single_field_store_takes_no_slot() {
    // A single-leaf element used to be the leakiest case: the slot pointer cdata
    // stayed live across the iteration in a function register (loop-carried PHI,
    // 24 B/iter), which the block-scope cluster existed to avoid. Leaf addressing
    // takes no slot, so there is nothing to scope -- bench
    // phi-scope/slot_write_single_field.rock is 0 B/iter with the JIT on AND off.
    let lua = emit_module(NATIVE_BUFFER_SINGLE_FIELD);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("local __nbslot"),
        "single-field NativeBuffer store should take no slot reference:\n{body}"
    );
    assert!(
        body.contains("__nb_checked_index("),
        "the element access must still be bounds-checked:\n{body}"
    );
}

/// A GENERIC value struct (`Pair<T>`, GC-carrying tier) must ride the scalar
/// ABI just like an FFI-clean value struct: per-field leaf params, a
/// destructured multi-return, and NO `_vt` table materialization in a
/// non-escaping caller. Pins item 4 of docs/value-two-tier-plan.md -- the
/// mechanism the Entity<T> flip depends on for its per-iteration-table win.
const GENERIC_VALUE_SCALARIZE: &str = r#"
value Pair<T> { a: T, b: T }
fn pair_sum(p: Pair<int>) -> int { return p.a + p.b }
fn mk(x: int, y: int) -> Pair<int> { return Pair { a: x, b: y } }
fn drive(x: int, y: int) -> int {
    let p = mk(x, y)
    return pair_sum(p)
}
fn main() { let _ = drive(3, 4) }
"#;

#[test]
fn generic_value_struct_rides_scalar_abi() {
    let lua = emit_module(GENERIC_VALUE_SCALARIZE);
    let sum = function_body(&lua, "pair_sum");
    // Scalarized params: two leaf args, not a single boxed table.
    assert!(
        !sum.contains("Pair_vt") && !sum.contains("Pair_ct"),
        "pair_sum must scalarize its generic value param (no materialization):\n{sum}"
    );
    let drive = function_body(&lua, "drive");
    assert!(
        !drive.contains("Pair_vt") && !drive.contains("Pair_ct"),
        "a non-escaping generic value chain must not materialize a table:\n{drive}"
    );
}
