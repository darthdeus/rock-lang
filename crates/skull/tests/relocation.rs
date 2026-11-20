use crate::common::bone_rejection_errors;

fn rejected_at(src: &str, at: &str) -> String {
    let errors = bone_rejection_errors(src);
    let relocations: Vec<_> = errors
        .iter()
        .filter(|e| e.message.contains("relocat"))
        .collect();
    assert_eq!(relocations.len(), 1);
    let error = relocations[0];
    let expected = src
        .find(at)
        .unwrap_or_else(|| panic!("`{at}` is not in the source"));
    assert_eq!(error.location.offset, expected);
    error.message.clone()
}

#[test]
fn element_ref_into_a_call_that_grows_the_same_vec_is_rejected() {
    let message = rejected_at(
        r#"
value Cell { v: int, tag: string }

fn grow_then_write(cell: ref Cell, cells: [Cell]) {
    for i in 0..64 { cells.push(Cell { v: i, tag: "x" }) }
    cell.v = 7
}

fn main() {
    let cells: [Cell] = [Cell { v: 1, tag: "a" }]
    grow_then_write(cells[0], cells)
}
"#,
        "cells[0], cells)",
    );
    assert!(message.contains("grow_then_write"));
}

#[test]
fn element_refs_into_relocating_calls_are_rejected_at_the_argument() {
    for (case, src, at) in [
        (
            "element ref is rejected when a later argument grows its vec",
            r#"
value Cell { v: int }

fn grow(cells: [Cell]) -> int {
    cells.push(Cell { v: 2 })
    return 1
}

fn write(cell: ref Cell, v: int) { cell.v = v }

fn main() {
    let cells: [Cell] = [Cell { v: 1 }]
    write(cells[0], grow(cells))
}
"#,
            "cells[0], grow",
        ),
        (
            "map element ref into a call that inserts",
            r#"
value Cell { v: int }

fn insert_then_write(cell: ref Cell, cells: Map<string, Cell>) {
    cells["b"] = Cell { v: 2 }
    cell.v = 7
}

fn main() {
    let cells: Map<string, Cell> = map_new()
    cells["a"] = Cell { v: 1 }
    insert_then_write(cells["a"], cells)
}
"#,
            "cells[\"a\"], cells)",
        ),
        (
            "element ref into a call that reaches the vec through a field",
            r#"
value Cell { v: int }
struct World { cells: [Cell] }

fn grow_world(cell: ref Cell, world: World) {
    world.cells.push(Cell { v: 2 })
    cell.v = 7
}

fn main() {
    let world = World { cells: [Cell { v: 1 }] }
    grow_world(world.cells[0], world)
}
"#,
            "world.cells[0], world)",
        ),
        (
            "element ref into a call that clears a global vec",
            r#"
value Cell { v: int }
global CELLS: [Cell] = []

fn reset(cell: ref Cell) {
    CELLS.clear()
    cell.v = 7
}

fn main() {
    CELLS.push(Cell { v: 1 })
    reset(CELLS[0])
}
"#,
            "CELLS[0])",
        ),
        (
            "element ref through a call chain is rejected at the outer call",
            r#"
value Cell { v: int }

fn inner(cell: ref Cell, cells: [Cell]) {
    let _ = cells.remove_at(0)
    cell.v = 7
}

fn outer(cell: ref Cell, cells: [Cell]) { inner(cell, cells) }

fn main() {
    let cells: [Cell] = [Cell { v: 1 }, Cell { v: 2 }]
    outer(cells[1], cells)
}
"#,
            "cells[1], cells)",
        ),
        (
            "element ref into a call that runs an unknown closure",
            r#"
value Cell { v: int }

fn visit(cell: ref Cell, then: fn()) {
    then()
    cell.v = 7
}

fn main() {
    let cells: [Cell] = [Cell { v: 1 }]
    visit(cells[0], () => { cells.push(Cell { v: 2 }) })
}
"#,
            "cells[0], () =>",
        ),
        (
            "element ref into a foreign function",
            r#"
value Cell { v: int }

inline(lua) fn poke(cell: ref Cell) = "cell.v = 7"

fn main() {
    let cells: [Cell] = [Cell { v: 1 }]
    poke(cells[0])
}
"#,
            "cells[0])",
        ),
    ] {
        assert!(rejected_at(src, at).contains("relocat"), "{case}");
    }
}

#[test]
fn element_refs_that_cannot_observe_a_relocation_are_accepted() {
    for (case, src) in [
        (
            "element ref used before the vec grows",
            r#"
value Cell { v: int }

fn bump(cell: ref Cell) { cell.v += 1 }

fn main() {
    let cells: [Cell] = [Cell { v: 1 }]
    bump(cells[0])
    cells.push(Cell { v: 2 })
    bump(cells[1])
}
"#,
        ),
        (
            "arena row ref across an insert",
            r#"
value Cell { v: int }
struct Holder { cell: Cell }

fn grow_rows(cell: ref Cell, rows: Arena<Holder>) {
    let _ = rows.insert(Holder { cell: Cell { v: 2 } })
    cell.v = 7
}

fn main() {
    let rows: Arena<Holder> = arena_new()
    let first = rows.insert(Holder { cell: Cell { v: 1 } })
    let row = first.get() ?? return
    grow_rows(row.cell, rows)
}
"#,
        ),
        (
            "element ref into a call that grows a different vec",
            r#"
value Cell { v: int }
value Mark { at: int }

fn mark_then_write(cell: ref Cell, marks: [Mark]) {
    marks.push(Mark { at: cell.v })
    cell.v = 7
}

fn write(cell: ref Cell) { cell.v = 3 }

fn main() {
    let cells: [Cell] = [Cell { v: 1 }]
    let others: [Cell] = []
    let marks: [Mark] = []
    mark_then_write(cells[0], marks)
    write(cells[0])
    others.push(Cell { v: 2 })
}
"#,
        ),
        (
            "element ref into a call that grows a vec of an underscored type",
            r#"
value Cell { v: int }
value Mark_row { at: int }

fn grow(marks: [Mark_row]) { marks.push(Mark_row { at: 1 }) }

fn grow_then_write(cell: ref Cell, marks: [Mark_row]) {
    grow(marks)
    cell.v = 7
}

fn main() {
    let cells: [Cell] = [Cell { v: 1 }]
    let marks: [Mark_row] = []
    grow_then_write(cells[0], marks)
}
"#,
        ),
    ] {
        assert!(bone_rejection_errors(src).is_empty(), "{case}");
    }
}

#[test]
fn map_element_ref_cannot_be_retained() {
    let errors = bone_rejection_errors(
        r#"
value Cell { v: int }

fn main() {
    let cells: Map<string, Cell> = map_new()
    cells["a"] = Cell { v: 1 }
    let cell: ref Cell = cells["a"]
    cells["b"] = Cell { v: 2 }
    cell.v = 5
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("relocatable Vec or Map storage"))
    );
}

#[test]
fn relocates_marker_is_reserved_for_the_stdlib() {
    let errors = bone_rejection_errors(
        r#"
@relocates
fn fake(values: [int]) {}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`@relocates` is reserved"))
    );
}
