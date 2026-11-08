//! GC-tier `value` construction: a call to the chunk-level `<Name>_vt(..)`
//! ctor whose body builds the table, while the cdata tier calls its
//! `ffi.typeof` ctor (`<Name>_ct(..)`). The runtime half -- a value type in a
//! record, equality, field evaluation order -- is
//! `tests/values/ctor_splice.rock`, which the `rock test tests/` gate runs.

use skull::mir_emit::emit_full;

use crate::common::{function_body, run_luajit, ws_with_stdlib};

/// `Bag` carries a `[int]`, so it is GC-carrying and materializes as a table
/// (`_vt`). `Pt` is FFI-clean, so it materializes as cdata (`_ct`) and must be
/// left alone: `ffi.typeof` has no Lua body to splice. Nullable returns carry
/// leaves; an escaping Holder's fields force both storage tiers to box.
const SRC: &str = "\
value Bag { xs: [int], n: int }
value Pt { x: float, y: float }
struct Holder { bag: Bag?, pt: Pt? }
global HELD: Holder? = null
global ABSENT_HELD: Holder? = null

fn find_bag(want: int) -> Bag? {
    if want == 0 { return null }
    return Bag { xs: [want, want * 2], n: want }
}

fn find_pt(want: int) -> Pt? {
    if want == 0 { return null }
    return Pt { x: 1.0, y: 2.0 }
}

fn main() {
    let b = find_bag(3)
    let p = find_pt(3)
    let h = Holder { bag: b, pt: p }
    let absent = Holder { bag: find_bag(0), pt: find_pt(0) }
    HELD = h
    ABSENT_HELD = absent
    if b != null { print(b.n) }
    if p != null { print(p.y) }
    print(h.bag == b)
    print(h.pt == p)
    print(absent.bag == null)
    print(absent.pt == null)
}
";

fn emit() -> String {
    let ws = ws_with_stdlib(SRC);
    let mid = skull::user_module_id(&ws).unwrap();
    emit_full(&ws, mid).expect("emit_full").0.lua
}

/// Lines carrying a construction, not the `<Name>_vt` definition itself.
fn construction_sites(lua: &str, needle: &str) -> usize {
    lua.lines()
        .filter(|l| l.contains(needle) && !l.contains("_vt = function("))
        .count()
}

#[test]
fn gc_table_and_cdata_constructions_call_their_ctors() {
    let lua = emit();
    assert!(
        construction_sites(&lua, "Bag_vt(") > 0,
        "fixture builds no GC-table value:\n{lua}"
    );
    assert!(
        function_body(&lua, "main").contains("Pt_ct("),
        "fixture stores no cdata value"
    );
    for (name, ctor) in [("find_bag", "Bag_vt("), ("find_pt", "Pt_ct(")] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains(ctor),
            "nullable return boxes before its field sink:\n{body}"
        );
    }
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "value_ctor_boxed_sink"),
        "3\n2\ntrue\ntrue\ntrue\ntrue"
    );
}

/// The ctor DEFINITION binds its table to a local and returns that: a
/// `return setmetatable(..)` body is a `CALLT` into C, which a root trace
/// started at the ctor's entry cannot leave through (`rec_ret` needs a real
/// `RET`), so every such root aborted `leaving loop in root trace`. Measured
/// 166 -> 0 on the shipped LuaJIT.
#[test]
fn the_ctor_definition_returns_through_a_local_not_a_tail_call() {
    let lua = emit();
    let def_start = lua.find("Bag_vt = function(").expect("Bag_vt definition");
    let def = &lua[def_start..];
    let def = &def[..def.find("\nend\n").expect("ctor end") + 5];
    assert!(
        def.contains("local __rock_ret = setmetatable(") && def.contains("\n  return __rock_ret\n"),
        "the ctor body must bind then return, not tail-call setmetatable:\n{def}"
    );
    assert!(!def.contains("return setmetatable("), "{def}");
}
