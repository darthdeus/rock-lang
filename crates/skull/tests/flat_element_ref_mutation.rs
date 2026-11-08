//! Flat-stored Lua container elements (value columns, strided rows) passed
//! through `ref` write the production element in place and leave its
//! neighbours alone. Each program's oracle is the value read back through the
//! `ref` after the write, compared with the container and with values read
//! before it. The behavior matrix that also runs on C is
//! `tests/values/flat_element_ref.rock`.

use crate::common::{require_luajit, run_luajit, try_emit_module};

const SRC: &str = r#"
value Cell { v: int }
fn bump(c: ref Cell) -> int {
    c.v += 1
    return c.v
}
fn main() {
    let xs: Vec<Cell> = []
    xs.push(Cell { v: 10 })
    xs.push(Cell { v: 20 })
    let vec_before = xs[0].v
    let vec_neighbor = xs[1].v
    let vec_result = bump(xs[0])

    let m: Map<string, Cell> = map_new()
    m["a"] = Cell { v: 30 }
    m["b"] = Cell { v: 40 }
    let map_before = m["a"].v
    let map_neighbor = m["b"].v
    let map_result = bump(m["a"])

    let a: Arena<Cell> = arena_new()
    let first = a.insert(Cell { v: 50 })
    let second = a.insert(Cell { v: 60 })
    let arena_before = a.get(first).unwrap().v
    let arena_neighbor = a.get(second).unwrap().v
    let arena_result = bump(a.borrow(first).unwrap())

    let vec_target_matches = vec_before != vec_result && xs[0].v == vec_result
    let vec_neighbor_matches = xs[1].v == vec_neighbor
    let map_target_matches = map_before != map_result && m["a"].v == map_result
    let map_neighbor_matches = m["b"].v == map_neighbor
    let arena_target_matches = arena_before != arena_result && a.get(first).unwrap().v == arena_result
    let arena_neighbor_matches = a.get(second).unwrap().v == arena_neighbor
    print("{vec_target_matches} {vec_neighbor_matches} {map_target_matches} {map_neighbor_matches} {arena_target_matches} {arena_neighbor_matches}")
}
"#;

#[test]
fn flat_container_element_ref_mutates_only_the_selected_element_on_lua() {
    require_luajit();
    let lua = try_emit_module(SRC).expect("flat element ref should emit on Lua");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "flat_element_ref"),
        "true true true true true true"
    );
}

/// Production emission of `source`, whose only frontend diagnostic must be the
/// relocatable-storage rule for a retained `ref` let: that rule is the
/// language's, and this test pins the backend boundary below it.
fn emit_past_relocatable_ref_rule(source: &str) -> String {
    let mut modules: Vec<_> = rock_stdlib::stdlib_files()
        .iter()
        .map(|file| {
            skull_parser::parse_module_clean(
                file.contents,
                ustr::Ustr::from(rock_stdlib::stdlib_file_id(file.name).as_str()),
            )
            .expect("stdlib parse")
        })
        .collect();
    modules.push(
        skull_parser::parse_module_clean(source, ustr::Ustr::from("<test>"))
            .expect("fixture parse"),
    );
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let errors: Vec<String> = ws
        .inferences
        .iter()
        .flatten()
        .flat_map(|inf| inf.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten()),
        )
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .map(|e| e.message.clone())
        .collect();
    assert!(
        errors
            .iter()
            .all(|m| m.contains("cannot retain a reference into relocatable")),
        "unexpected frontend errors: {errors:#?}"
    );
    let module = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (emission, _) = skull::mir_emit::emit_full(&ws, module).expect("production emit");
    emission.lua
}

#[test]
fn nested_field_ref_let_mutates_the_owned_map_value_on_lua() {
    require_luajit();
    let lua = emit_past_relocatable_ref_rule(
        r#"
value Inner { v: int }
value Cell { inner: Inner, w: int }
fn main() {
    let m: Map<string, Cell> = map_new()
    m["a"] = Cell { inner: Inner { v: 10 }, w: 77 }
    m["b"] = Cell { inner: Inner { v: 20 }, w: 88 }
    let before = m["a"].inner.v
    let owner_sibling = m["a"].w
    let neighbor = m["b"].inner.v
    let inner: ref Inner = m["a"].inner
    inner.v += 1
    let result = inner.v
    let target_matches = result != before && m["a"].inner.v == result
    let owner_sibling_matches = m["a"].w == owner_sibling
    let neighbor_matches = m["b"].inner.v == neighbor
    print("{target_matches} {owner_sibling_matches} {neighbor_matches}")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "nested_field_ref_let"),
        "true true true"
    );
}

#[test]
fn whole_element_ref_lets_mutate_the_vec_and_map_element_on_lua() {
    require_luajit();
    let lua = emit_past_relocatable_ref_rule(
        r#"
value Cell { v: int, w: int }
fn main() {
    let m: Map<string, Cell> = map_new()
    m["a"] = Cell { v: 10, w: 11 }
    m["b"] = Cell { v: 20, w: 21 }
    let xs: [Cell] = []
    xs.push(Cell { v: 30, w: 31 })
    xs.push(Cell { v: 40, w: 41 })
    let map_before = m["b"]
    let map_neighbor = m["a"]
    let vec_before = xs[1]
    let vec_neighbor = xs[0]
    let mc: ref Cell = m["b"]
    mc.v += 1
    let map_seen = mc.v
    let vc: ref Cell = xs[1]
    vc.v += 1
    let vec_seen = vc.v
    let map_ok = map_seen != map_before.v && m["b"].v == map_seen && m["b"].w == map_before.w
    let map_neighbor_ok = m["a"].v == map_neighbor.v && m["a"].w == map_neighbor.w
    let vec_ok = vec_seen != vec_before.v && xs[1].v == vec_seen && xs[1].w == vec_before.w
    let vec_neighbor_ok = xs[0].v == vec_neighbor.v && xs[0].w == vec_neighbor.w
    print("{map_ok} {map_neighbor_ok} {vec_ok} {vec_neighbor_ok}")
}
"#,
    );
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "whole_element_ref_let"),
        "true true true true"
    );
}

/// A `ref` to a missing Map key has no element to write; it raises at the
/// borrow rather than writing a detached copy (C aborts the same program).
#[test]
fn ref_to_a_missing_map_key_raises_on_lua() {
    require_luajit();
    let lua = try_emit_module(
        r#"
value Cell { v: int }
fn bump(c: ref Cell) { c.v += 1 }
fn main() {
    let m: Map<string, Cell> = map_new()
    m["a"] = Cell { v: 1 }
    bump(m["b"])
    print("unreachable")
}
"#,
    )
    .expect("emits");
    let out = run_luajit(
        &format!("{lua}\nlocal ok, err = pcall(main)\nprint(ok, err ~= nil)\n"),
        "missing_key_ref",
    );
    assert_eq!(
        out, "false\ttrue",
        "the borrow of a missing key must raise: {out}"
    );
}

/// Recursion settles a `ref` result only when every return agrees: a recursive
/// call that hands back a differently stored `ref` still rejects with a span.
#[test]
fn recursive_ref_return_of_differently_stored_ref_rejects() {
    let source = r#"
value Cell { v: int }
struct Holder { cell: Cell }
fn pick(c: ref Cell, h: ref Holder, n: int) -> ref Cell {
    if n == 0 { return c }
    return pick(h.cell, h, n - 1)
}
fn bump(c: ref Cell) -> int { c.v += 1
return c.v }
fn main() {
    let xs: [Cell] = []
    xs.push(Cell { v: 10 })
    let h = Holder { cell: Cell { v: 1 } }
    print("{bump(pick(xs[0], h, 2))}")
}
"#;
    let err = match try_emit_module(source) {
        Ok(_) => panic!("a flat and a boxed `ref` return of one callee must not emit"),
        Err(err) => err,
    };
    assert!(
        err.message.contains("`pick`") && err.message.contains("differently stored reference"),
        "{err:?}"
    );
    assert!(
        !err.labels.is_empty(),
        "the rejection must carry a source span: {err:?}"
    );
}
