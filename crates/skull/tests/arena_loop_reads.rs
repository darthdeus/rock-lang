use crate::common::{emit_module, function_body, run_luajit};

const SOURCE: &str = r#"
struct Row { x: int }
nobox value Column { x: int, y: int }
nobox value Strided { x: int, y: int, a: int, b: int, c: int }

@nomangle fn column_field(xs: [Column]) -> int {
    let sum = 0
    for x in xs { sum += x.x }
    return sum
}
@nomangle fn strided_field(xs: [Strided]) -> int {
    let sum = 0
    for x in xs { sum += x.x }
    return sum
}
@nomangle fn unused_vec_element() {
    let xs = [Column { x: 1, y: 2 }, Column { x: 3, y: 4 }]
    for x in xs { xs.clear() }
}

@nomangle fn unused_entity(a: Arena<Row>) -> int {
    let sum = 0
    for (e, row) in a { sum += row.x }
    return sum
}
@nomangle fn unused_pair_entity(a: Arena<Row>) -> int {
    let sum = 0
    for pair in a { sum += pair.1.x }
    return sum
}
@nomangle fn unused_column(a: Arena<Column>) -> int {
    let count = 0
    for (e, row) in a { count += 1 }
    return count
}
@nomangle fn unused_enum(a: Arena<Row>) -> int {
    let count = 0
    for (i, (e, row)) in enumerate(a) { count += 1 }
    return count
}
@nomangle fn used_entity(a: Arena<Row>) -> int {
    let sum = 0
    for (e, row) in a { sum += e._generation + row.x }
    return sum
}
fn pair_sum(pair: (Entity<Row>, Row)) -> int { return pair.0._generation + pair.1.x }
@nomangle fn pair_uses(a: Arena<Row>) {
    let sum = 0
    let fs: [fn() -> int] = []
    for pair in a {
        sum += pair_sum(pair)
        fs.push(() => pair.1.x)
    }
    assert(sum == 6 && fs[0]() == 1 && fs[1]() == 3)
    let values: [fn() -> int] = []
    for pair in a { values.push(() => pair.1.x) }
    assert(values[0]() == 1 && values[1]() == 3)
}
@nomangle fn check_walks() {
    let a: Arena<Row> = arena_new()
    a.insert(Row { x: 1 })
    let stale = a.insert(Row { x: 2 })
    a.insert(Row { x: 3 })
    a.remove(stale)
    assert(stale.get() == null)
    assert(unused_entity(a) == 4 && unused_pair_entity(a) == 4)
    assert(unused_enum(a) == 2 && used_entity(a) == 6)
    pair_uses(a)
    let c: Arena<Column> = arena_new()
    c.insert(Column { x: 1, y: 2 })
    let removed = c.insert(Column { x: 3, y: 4 })
    c.insert(Column { x: 5, y: 6 })
    let count = 0
    for (e, row) in c {
        if count == 0 { c.remove(removed) }
        count += 1
    }
    assert(count == 2 && unused_column(c) == 2)
    arena_destroy(a)
    assert(unused_entity(a) == 0 && unused_pair_entity(a) == 0 && unused_enum(a) == 0)
}
"#;

#[test]
fn iterators_read_only_live_leaves_but_keep_bounds_and_removals() {
    let lua = emit_module(SOURCE);
    for name in ["unused_entity", "unused_pair_entity", "unused_enum"] {
        let body = function_body(&lua, name);
        assert!(!body.contains(".generations"));
    }
    assert!(function_body(&lua, "used_entity").contains(".generations"));

    for name in ["unused_column", "unused_enum"] {
        let body = function_body(&lua, name);
        assert!(!body.contains(".data"));
        assert!(body.contains(".occupied"));
    }

    assert_eq!(
        run_luajit(&format!("{lua}\ncheck_walks()\nprint('ok')")),
        "ok"
    );

    let column = function_body(&lua, "column_field");
    assert!(!column.contains(".y"));
    assert!(column.contains(".x)[(c"));
    assert!(!column.contains("__vec_checked_index"));
    let strided = function_body(&lua, "strided_field");
    assert!(!strided.contains("__vec_checked_index"));
    let unused = function_body(&lua, "unused_vec_element");
    assert!(unused.contains("__vec_checked_index"));
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nlocal ok, err = pcall(unused_vec_element)\nassert(not ok and tostring(err):find('out of bounds'))\nprint('ok')"
        )),
        "ok"
    );
}
