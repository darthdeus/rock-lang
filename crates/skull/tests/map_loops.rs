use crate::common::{emit_module, function_body, run_luajit};

const SOURCE: &str = r#"
struct Row { x: int }
nobox value Column { x: int, y: int }

@nomangle fn scalar_sum(m: Map<int, int>) -> int {
    let sum = 0
    for (k, v) in m {
        if k == 2 { continue }
        sum += v
        if k == 99 { break }
    }
    return sum
}
@nomangle fn column_count(m: Map<int, Column>) -> int {
    let count = 0
    for (k, unused) in m { count += 1 }
    return count
}
@nomangle fn mutation_and_lengths() {
    let m: Map<int, Row> = map_new()
    let c: Map<int, Column> = map_new()
    assert(m.len() == 0 && c.len() == 0 && m.is_empty() && c.is_empty())
    for i in 0..30 {
        m[i] = Row { x: i }
        c[i] = Column { x: i, y: i + 1 }
    }
    m[2] = Row { x: 40 }
    c[2] = Column { x: 40, y: 41 }
    assert(m.len() == 30 && c.len() == 30)
    m.remove(2)
    c.remove(2)
    m.remove(2)
    c.remove(2)
    assert(m.len() == 29 && c.len() == 29)
    let sum = 0
    for (k, v) in m {
        sum += v.x
        m.remove(k)
    }
    assert(sum == 433 && m.len() == 0 && m.is_empty())
    sum = 0
    for (k, v) in c {
        sum += v.x
        c.remove(k)
    }
    assert(sum == 433 && c.len() == 0 && c.is_empty())
    for i in 0..30 {
        m[i] = Row { x: i }
        c[i] = Column { x: i, y: i + 1 }
    }
    // Replacing an existing entry must be visible on later iterations.
    let seen = 0
    for (k, v) in m {
        if seen == 0 {
            for (other, _) in m { m[other] = Row { x: 100 } }
        } else { assert(v.x == 100) }
        seen += 1
    }
    assert(seen == 30)
    let closures: [fn() -> int] = []
    for (k, v) in c { closures.push(() => k + v.x) }
    let total = 0
    for f in closures { total += f() }
    assert(total == 870)
    m.clear()
    c.clear()
    assert(m.len() == 0 && c.len() == 0 && m.is_empty() && c.is_empty())
    m[7] = Row { x: 7 }
    c[7] = Column { x: 7, y: 8 }
    assert(m.len() == 1 && c.len() == 1)
}
"#;

#[test]
fn map_loops_use_lua_generic_for() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "scalar_sum");
    assert!(body.contains(" in next, "), "{body}");
}

#[test]
fn key_only_column_loop_does_not_read_values() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "column_count");
    assert!(!body.contains(".data"), "{body}");
}

#[test]
fn map_mutation_lengths_and_loop_control_are_preserved() {
    let lua = emit_module(SOURCE);
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nmutation_and_lengths()\nassert(scalar_sum({{[1]=10,[2]=20,[3]=30}}) == 40)\nprint('ok')"
        )),
        "ok"
    );
}
