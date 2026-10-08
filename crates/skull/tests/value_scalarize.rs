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
use crate::common::{emit_module, function_body, require_luajit, run_luajit};

const NULLABLE_ASSET_READ: &str = r#"
value Handle { id: int, tags: [int] }
value Asset { image: Handle?, shadow: Handle?, width: int }
global ART: Asset? = null
global KEPT: Asset? = null
fn sample(a: Asset) -> int { return (a.image?.id ?? 0) + (a.shadow?.id ?? 0) + a.width }
fn draw_asset() -> int {
    let art = ART ?? return -1
    return sample(art) + sample(art)
}
fn keep_asset() { let art = ART ?? return; KEPT = art }
fn mutate(a: ref Asset) { if a.image != null { a.image.id = 99 } }
fn borrow_asset() { let art = ART ?? return; mutate(art) }
fn main() {
    print(draw_asset())
    ART = Asset { image: Handle { id: 2, tags: [] }, shadow: Handle { id: 3, tags: [] }, width: 7 }
    print(draw_asset())
    keep_asset()
    borrow_asset()
    print("{ART?.image?.id ?? 0} {KEPT?.image?.id ?? 0}")
    if ART != null && ART.image != null { ART.image.id = 11 }
    print(KEPT?.image?.id ?? 0)
}
"#;

#[test]
fn a_coalesced_nullable_asset_keeps_owned_copies() {
    let lua = emit_module(NULLABLE_ASSET_READ);
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "nullable_asset_copy"), "-1\n24\n2 2\n2");
}

#[test]
fn coalesced_nullable_children_snapshot_before_source_writes() {
    let probes = [
        ("source-write", include_str!("fixtures/coalesce_snapshot/source-write.rock")),
        ("closure-read", include_str!("fixtures/coalesce_snapshot/closure-read.rock")),
        ("cdata-source-write", include_str!("fixtures/coalesce_snapshot/cdata-source-write.rock")),
        ("cdata-closure-read", include_str!("fixtures/coalesce_snapshot/cdata-closure-read.rock")),
        ("generic-source-write", include_str!("fixtures/coalesce_snapshot/generic-source-write.rock")),
        ("global-source-write", include_str!("fixtures/coalesce_snapshot/global-source-write.rock")),
        ("array-source-write", include_str!("fixtures/coalesce_snapshot/array-source-write.rock")),
        ("source-write-then-escape", include_str!("fixtures/coalesce_snapshot/source-write-then-escape.rock")),
    ];
    let mut failures = Vec::new();
    for (name, source) in probes {
        let lua = emit_module(source);
        let output = run_luajit(&format!("{lua}\nprobe()\n"), name);
        if output != "1" {
            failures.push(format!("{name}: expected 1, got {output}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

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

/// A value struct used BOTH by field reads AND as a whole struct (a boxed
/// argument) must keep its cdata: the whole-struct use escapes, so SROA must not split the
/// cell. The hazard is a half-rewrite -- forwarding the field reads to per-field
/// cells while the escaping use blocks the store expansion, leaving the field
/// cells unstored (uninitialized reads). Pins the `is_cand` re-check in
/// `field_rewrites`. Runs under luajit so the uninit read is actually caught.
const VALUE_FIELD_AND_WHOLE_USE: &str = r#"
value Color { r: float, g: float, b: float, a: float }
value FixVec3 { x: float, y: float, z: float }
@nomangle inline(lua) fn __same(a: Color, b: Color) -> bool = "return a.r == b.r and a.g == b.g and a.b == b.b and a.a == b.a"

// `c` is field-read into a local (`let r = c.r`) AND used as a whole struct
// (`__same(c, c2)`; `==` compares leaves). The whole-struct use escapes, so the
// cell must NOT be split and the `Color_ct` cdata must be KEPT for the call. The field read `c.r` itself folds to
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
    let eq = if __same(c, c2) { 1.0 } else { 0.0 }
    return (r + z) + eq
}

@nomangle inline(lua) fn __out(x: float) = "io.write(tostring(x))"
fn main() {
    __out(coexist(2.0))   // 2.0 (c.r) + 30.0 (v.z) + 1.0 (same c c2) = 33
}
"#;

#[test]
fn value_struct_with_whole_use_keeps_cdata() {
    let lua = emit_module(VALUE_FIELD_AND_WHOLE_USE);
    let body = function_body(&lua, "coexist");
    // The whole-struct argument escapes, so the cdata must be KEPT (not SROA'd).
    assert!(
        body.contains("Color_ct("),
        "a value struct used as a whole (boxed argument) must keep its cdata:\n{body}"
    );
    // The separate field read `c.r` folds to its construction operand (no `.r`
    // projection needed), since the value struct is immutable.
    assert!(
        !body.contains(".r"),
        "the immutable field read should fold to the operand, not project `.r`:\n{body}"
    );
    // Runtime guard against a BAD fold (e.g. an uninitialized-cell read): the value
    // must be exactly 33.
    require_luajit();
    let path = std::env::temp_dir().join(format!("skull_coexist_{}.lua", std::process::id()));
    std::fs::write(&path, format!("{lua}\nmain()\n")).expect("write");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "luajit failed:\n{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "33", "coexist must compute correctly");
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
    // A single-leaf element is the leakiest case for the slot form: the slot
    // pointer cdata stays live across the iteration in a function register
    // (loop-carried PHI, 24 B/iter). Leaf addressing takes no slot at all --
    // bench phi-scope/slot_write_single_field.rock is 0 B/iter, JIT on AND off.
    let lua = emit_module(NATIVE_BUFFER_SINGLE_FIELD);
    let body = function_body(&lua, "fill");
    assert!(
        !body.contains("__nb_checked_get("),
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

const NESTED_SCALAR_ABI: &str = r#"
value V2 { x: float, y: float }
value Rect { tl: V2, br: V2 }

@scalar_abi inline(lua) fn nested_leaves(r: Rect) -> float = """
    return r__tl__x + r__br__y
"""

fn main() { let _ = nested_leaves(Rect { tl: V2 { x: 1.0, y: 2.0 }, br: V2 { x: 4.0, y: 8.0 } }) }
"#;

/// The `@scalar_abi` leaf-param namer must be INJECTIVE. `leaf_param_names`
/// recursed into a nested value field with the base unchanged, so `Rect { tl, br }`
/// emitted `function f(r__x, r__y, r__x, r__y)` -- Lua binds the later pair over
/// the earlier, and a body naming its own leaves silently read `br` where it
/// wrote `tl`. `leaf_paths_rec` (the column-key twin) had already solved this
/// with dotted paths; the param namer never got the fix, and the comment there
/// calling the case unreachable was scoped to `nobox` crossings -- an ordinary
/// unmarked nested value reaches it today.
#[test]
fn nested_scalar_abi_leaf_params_are_injective() {
    let lua = emit_module(NESTED_SCALAR_ABI);
    let f = function_body(&lua, "nested_leaves");
    let params = f
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| panic!("no param list in:\n{f}"));
    let names: Vec<&str> = params.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let mut uniq = names.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(
        uniq.len(),
        names.len(),
        "nested value leaf params must be distinct, got {names:?} in:\n{f}"
    );
}

const UNDERSCORE_FIELD_NO_COLLISION: &str = r#"
nobox value V2 { x: float, y: float }
nobox value Ok { tl__z: float, tl: V2 }

@scalar_abi inline(lua) fn fine(r: Ok) -> float = """
    return r__tl__z + r__tl__x * 10.0 + r__tl__y * 100.0
"""

fn apply(f: (Ok) -> float, r: Ok) -> float { return f(r) }

@nomangle inline(lua) fn __out(x: float) = "io.write(tostring(x))"
fn main() {
    let r = Ok { tl__z: 7.0, tl: V2 { x: 1.0, y: 2.0 } }
    __out(apply(fine, r))
}
"#;

const UNMARKED_VALUE_VIA_NULLABLE_FN_VALUE: &str = r#"
value V2 { x: float, y: float }
value Box2 { tl: V2, w: float }

@scalar_abi inline(lua) fn probe(r: Box2) -> float = """
    return r__tl__x + r__tl__y * 10.0 + r__w * 100.0
"""

fn apply(f: ((Box2) -> float)?, r: Box2) -> float {
    if f != null { return f.unwrap()(r) }
    return -1.0
}

fn main() {
    let r = Box2 { tl: V2 { x: 1.0, y: 2.0 }, w: 3.0 }
    print("{apply(probe, r)}")
}
"#;

const UNMARKED_VALUE_VIA_FN_VALUE: &str = r#"
value V2 { x: float, y: float }
value Box2 { tl: V2, w: float }

@scalar_abi inline(lua) fn probe(r: Box2) -> float = """
    return r__tl__x + r__tl__y * 10.0 + r__w * 100.0
"""

fn apply(f: (Box2) -> float, r: Box2) -> float { return f(r) }

fn main() {
    let r = Box2 { tl: V2 { x: 1.0, y: 2.0 }, w: 3.0 }
    print("{apply(probe, r)}")
}
"#;

/// FIXED -- was a sentinel. An UNMARKED `value` struct reaching a `@scalar_abi`
/// fn through a FN VALUE used to compile clean and die at runtime; it is now a
/// spanned rejection where the fn is taken by value.
///
/// THE MECHANISM: the two ends derived the ABI from different sources. A
/// definition reads its `BackendFnAbiPlan`; an indirect call site has only the fn TYPE,
/// so it reads `fn_value_abi`. Those agree by construction for an ordinary body
/// -- the plan NARROWS an address-taken one (`scx.taken_by_ref`) to
/// exactly the `nobox` shapes the type yields, which is what that narrowing
/// exists for. `@scalar_abi` is the one thing that bypasses it, and deliberately:
/// its hand-written `inline(lua)` body spells the leaf params (`r__tl__x`), so
/// narrowing the definition would break the BODY instead of the call.
///
/// So the two cannot be reconciled by re-deriving either side, which is the case
/// the old sentinel text named as the alternative. The definition scalarized to
/// `probe(r__tl__x, r__tl__y, r__w)` while `apply` packed one `Box2_ct(...)`:
/// the first leaf got the whole cdata and the rest went nil.
///
/// The `nobox` sibling below is the positive control and still emits, because
/// there both ends DO agree -- which is exactly the fix the diagnostic suggests.
#[test]
fn unmarked_value_through_fn_value_rejects() {
    let err = crate::common::try_emit_module(UNMARKED_VALUE_VIA_FN_VALUE)
        .expect_err("an unreconcilable fn-value ABI must reject");
    assert!(
        err.message.contains("cannot be taken as a fn value") && err.message.contains("probe"),
        "the rejection must name the fn and the reason, got: {}",
        err.message
    );
}

/// The NULLABLE spelling of [`unmarked_value_through_fn_value_rejects`], and the
/// property the `Nullable` peel in `fn_value_abi` ENDANGERED: before the peel a
/// nullable fn-value type answered `None` and fell to the boxed ABI, so this
/// mismatched for that reason rather than on its merits. After the peel the type
/// answers, and it must still reject -- an UNMARKED value yields no leaves, so
/// `fn_value_abi` returns `None` again and the `@scalar_abi` definition still
/// disagrees with it. A peel that widened past `nobox` would turn this green.
#[test]
fn unmarked_value_through_a_nullable_fn_value_still_rejects() {
    let err = crate::common::try_emit_module(UNMARKED_VALUE_VIA_NULLABLE_FN_VALUE)
        .expect_err("a nullable fn-value reference must not launder an unreconcilable ABI");
    assert!(
        err.message.contains("cannot be taken as a fn value") && err.message.contains("probe"),
        "the rejection must name the fn and the reason, got: {}",
        err.message
    );
}

/// POSITIVE CONTROL for the `value_leaf_name_collisions` rejection: a `__`-bearing
/// field name only collides when it JOINS to an existing nested path, so `tl__z`
/// beside a nested `tl: V2 { x, y }` is unambiguous and must still emit -- and the
/// leaf params must stay distinct. Both routes are exercised (the fn is reached
/// through a fn value, which narrows the ABI to what the fn TYPE alone yields), and
/// the result is checked under luajit so a mis-ordered leaf splat is caught too.
///
/// The REJECTION cases live in `tests/errors/value_leaf_name_collision.rock.err`.
#[test]
fn non_colliding_underscore_field_still_scalarizes() {
    let lua = emit_module(UNDERSCORE_FIELD_NO_COLLISION);
    let f = function_body(&lua, "fine");
    for leaf in ["r__tl__z", "r__tl__x", "r__tl__y"] {
        assert!(f.contains(leaf), "leaf param `{leaf}` must be emitted:\n{f}");
    }
    assert!(
        !f.contains("r__tl__z, r__tl__z"),
        "the non-colliding leaves must stay distinct:\n{f}"
    );
    assert_eq!(
        crate::common::run_luajit(&format!("{lua}\nmain()\n"), "underscore_no_collision"),
        "217",
        "7 + 1*10 + 2*100 -- a wrong leaf order or a duplicate param would shift this"
    );
}

// `Mat4` is a leaf-form cdata: its ctor keys are dotted leaf paths
// (`c0.x`), so a copy of a forwarded `Mat4` reads `Field(Field(ctor, c0), x)`.
// Inside a closure body the binding's cell is forwarded to its ctor, and the
// copy must fold those reads to the ctor's operands instead of building the
// value twice.
const LEAF_FORM_SAVED_IN_CLOSURE: &str = r#"
global ACTIVE: bool = true
global VIEW: Mat4 = mat4_identity()
fn reset() {
    ACTIVE = false
    VIEW = mat4_identity()
}
fn with_fn(f: fn() -> ()) { f() }
fn with_saved(draw: fn() -> ()) {
    with_fn(() => {
        let was = ACTIVE
        let saved = VIEW
        reset()
        draw()
        ACTIVE = was
        VIEW = saved
    })
}
fn main() {
    with_saved(() => { VIEW = mat4_identity() })
    print("{ACTIVE} {VIEW.c3.w}")
}
"#;

#[test]
fn leaf_form_value_copy_in_a_closure_builds_one_box() {
    let lua = emit_module(LEAF_FORM_SAVED_IN_CLOSURE);
    let body = function_body(&lua, "with_saved_fn_ret_unit__lambda0");
    assert_eq!(
        body.matches("Mat4_lct(").count() + body.matches("Mat4_ct(").count(),
        1,
        "restoring the saved Mat4 builds exactly one box:\n{body}"
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "value_scalarize_leaf_form_saved"), "true 1");
}

// Enums are immutable, so a copy of a value that holds an enum box can share
// that box: only the value's own fields need fresh storage. The two arms of
// `identified` copy `item` into a second owner; neither rebuilds its `kind`.
const ENUM_CHILD_OF_A_COPIED_VALUE: &str = r#"
enum QKind { Gold, Key(string), Bandage }
enum QProp { Bound }
value QBook { pages: [int] = [] }
value QItem {
    id: int = 0,
    kind: QKind,
    amount: int = 1,
    property: QProp? = null,
    book: QBook? = null,
}
value QIdent { item: QItem, next: int }
value QInv { stacks: [QItem] = [] }
fn ident(item: QItem, next: int) -> QIdent { return QIdent { item: QItem { ..item, id: next }, next: next + 1 } }
fn set_count(inv: QInv, kind: QKind, amount: int, next: int) -> int {
    let old = inv.stacks[0]
    let item = if amount > 0 { QItem { ..old, kind, amount } } else { QItem { kind, amount: 0 } }
    let identified = if amount > 0 { ident(item, next) } else { QIdent { item, next } }
    inv.stacks[0] = identified.item
    return identified.next
}
fn main() {
    let inv = QInv { stacks: [QItem { kind: QKind::Key("a"), amount: 1 }] }
    let n = set_count(inv, QKind::Key("b"), 3, 7)
    let m = set_count(inv, QKind::Gold, 0, n)
    let k = match inv.stacks[0].kind { QKind::Gold => "gold", QKind::Key(s) => s, QKind::Bandage => "b" }
    print("{n} {m} {inv.stacks[0].amount} {inv.stacks[0].id} {k}")
}
"#;

#[test]
fn a_value_copy_shares_its_enum_children() {
    let lua = emit_module(ENUM_CHILD_OF_A_COPIED_VALUE);
    let body = function_body(&lua, "set_count");
    // `let item` keeps its join as leaves, so the one `kind` box is the
    // stored item's; the copies of `item` add none.
    assert_eq!(body.matches("__ty = \"QKind\"").count(), 1, "set_count rebuilds a QKind box:\n{body}");
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "value_scalarize_enum_child_copy"), "8 8 0 0 gold");
}

/// `W` is wide enough (50 leaves) to cross every boundary as one box.
fn nullable_child_snapshot_source() -> String {
    let fields: Vec<String> = (0..50).map(|i| format!("f{i}: int")).collect();
    let inits: Vec<String> = (0..50).map(|i| format!("f{i}: 1")).collect();
    format!(
        r#"
value W {{ {} }}
value Outer {{ w: W?, xs: [int] }}
struct Holder {{ o: Outer }}
fn mkw() -> W {{ return W {{ {} }} }}
fn main() {{
    let src = Holder {{ o: Outer {{ w: mkw(), xs: [] }} }}
    let o = src.o
    if src.o.w != null {{ src.o.w.f0 = 99 }}
    print(o.w?.f0 ?? -1)
}}
"#,
        fields.join(", "),
        inits.join(", ")
    )
}

/// SENTINEL (inverted): the correct output is `1`, as the C backend prints. A
/// Lua copy keeps its `V?` value child as the source's box until the copy
/// escapes (a store, a return), so a write through the source's child before
/// the copy's last read shows through. Copying the child at the snapshot fixes
/// it but allocates on read-only draw paths. When this fails with `1`, flip it
/// and move the case to tests/value_alias_escape_paths.rock.
#[test]
fn a_copy_reads_a_later_write_through_its_sources_nullable_child() {
    let lua = emit_module(&nullable_child_snapshot_source());
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "value_scalarize_nullable_child_snapshot"), "99");
}

const RECURSIVE_VALUE_COPY: &str = r#"
value Chain { n: int, next: Chain?, xs: [int] }
struct ChainHolder { c: Chain }
fn main() {
    let c = Chain { n: 0, next: Chain { n: 1, next: null, xs: [] }, xs: [] }
    let a = ChainHolder { c: c }
    let b = ChainHolder { c: c }
    if a.c.next != null { a.c.next.n = 99 }
    if b.c.next != null { print(b.c.next.n) }
}
"#;

/// A copy of a recursive value must own its `Chain?` child, and so that
/// child's child, to any depth; the Lua lowering unrolls copies at compile
/// time and has no runtime copier, so it rejects the copy instead of sharing
/// the child. The C backend rejects the type's by-value layout.
#[test]
fn a_recursive_value_copy_rejects() {
    let err = crate::common::try_emit_module(RECURSIVE_VALUE_COPY)
        .expect_err("a recursive value copy must not share its child");
    assert!(
        err.message.contains("recursive value type `Chain`"),
        "the rejection must name the recursive type, got: {}",
        err.message
    );
}

/// The control: reading through a recursive value, and a copy that never
/// escapes, need no copy of the child.
#[test]
fn a_recursive_value_read_compiles() {
    let lua = emit_module(
        r#"
value Chain { n: int, next: Chain?, xs: [int] }
struct ChainHolder { c: Chain }
fn main() {
    let h = ChainHolder { c: Chain { n: 0, next: Chain { n: 1, next: null, xs: [] }, xs: [] } }
    let c = h.c
    print(c.next?.n ?? -1)
}
"#,
    );
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "value_scalarize_recursive_read"), "1");
}
