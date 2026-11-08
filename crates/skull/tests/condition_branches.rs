use crate::common::{definition_names, emit_mir, run_luajit};

#[test]
fn branch_conditions_preserve_short_circuit_order_and_value_uses() {
    let src = r#"
struct Trace { code: int }
fn mark(t: Trace, digit: int, value: bool) -> bool {
    t.code = t.code * 10 + digit
    value
}
fn choose(t: Trace, a: bool, b: bool, c: bool) -> int {
    if !(mark(t, 1, a) && mark(t, 2, b)) && (mark(t, 3, c) || mark(t, 4, a)) { 1 } else { 0 }
}
fn value(a: bool, b: bool, c: bool) -> bool { !(a && b) && (c || a) }
fn statement(t: Trace, a: bool, b: bool, c: bool) {
    if !(mark(t, 1, a) && mark(t, 2, b)) && (mark(t, 3, c) || mark(t, 4, a)) { t.code = t.code * 10 + 5 }
}
fn looping(t: Trace) -> int {
    let n = 0
    while mark(t, 1, n < 4) && !mark(t, 2, n == 3) {
        defer { t.code = t.code * 10 + 3 }
        n += 1
        if n == 1 { continue }
        break
    }
    n
}
fn main() {}
"#;
    let emitted = emit_mir(src);
    let checks = r#"
local expected = {
    {false, false, false, 0, 134}, {false, false, true, 1, 13},
    {false, true, false, 0, 134}, {false, true, true, 1, 13},
    {true, false, false, 1, 1234}, {true, false, true, 1, 123},
    {true, true, false, 0, 12}, {true, true, true, 0, 12},
}
for _, row in ipairs(expected) do
    local t = {code = 0}
    assert(choose(t, row[1], row[2], row[3]) == row[4])
    assert(t.code == row[5])
    assert(value(row[1], row[2], row[3]) == (row[4] == 1))
    t.code = 0
    statement(t, row[1], row[2], row[3])
    assert(t.code == (row[4] == 1 and row[5] * 10 + 5 or row[5]))
end
local t = {code = 0}
assert(looping(t) == 2)
assert(t.code == 123123)
print('ok')
"#;
    let mut checks = checks.to_string();
    for name in ["choose", "value", "statement", "looping"] {
        let prefix = format!("{name}_");
        let symbols: Vec<_> = definition_names(&emitted.lua)
            .into_iter()
            .filter(|symbol| symbol.starts_with(&prefix) && !symbol.contains("__"))
            .collect();
        assert_eq!(symbols.len(), 1, "one callable for {name}: {symbols:?}");
        checks = checks.replace(&format!("{name}("), &format!("{}(", symbols[0]));
    }
    assert_eq!(
        run_luajit(&format!("{}\n{checks}", emitted.lua), "condition_branches"),
        "ok"
    );
}

#[test]
fn branch_conditions_preserve_nullable_presence_guards_and_not_overloads() {
    let src = r#"
struct Trace { code: int }
struct Switch { enabled: bool, trace: Trace }
operator !(s: Switch) -> bool {
    s.trace.code += 1
    s.enabled
}
nobox value Id { n: int }
struct State { id: Id? }
fn nullable(flag: bool?) -> int {
    if flag != null && !flag { return 1 }
    if flag == null || flag { return 2 }
    3
}
fn nullable_loop(flag: bool?) -> int {
    let n = 0
    while flag != null && flag {
        n += 1
        flag = null
    }
    n
}
fn guarded(flag: bool?) -> int {
    match 0 {
        _ if flag != null && !flag => 1,
        _ if flag == null || flag => 2,
        _ => 3,
    }
}
fn guarded_statement(flag: bool?, t: Trace) {
    match 0 {
        _ if flag != null && !flag => { t.code = 1 },
        _ if flag == null || flag => { t.code = 2 },
        _ => { t.code = 3 },
    }
}
fn overloaded(s: Switch) -> int { if !s && !s { 1 } else { 0 } }
fn present(s: State) -> int {
    if !(s.id == null) && s.id.n == 7 { return 1 }
    if s.id == null || s.id.n == 9 { return 2 }
    3
}
fn main() {
    print(nullable(null))
    print(nullable(false))
    print(nullable(true))
    print(guarded(null))
    print(guarded(false))
    print(guarded(true))
    let t = Trace { code: 0 }
    guarded_statement(null, t)
    print(t.code)
    guarded_statement(false, t)
    print(t.code)
    guarded_statement(true, t)
    print(t.code)
    t.code = 0
    print(overloaded(Switch { enabled: false, trace: t }))
    print(t.code)
    t.code = 0
    print(overloaded(Switch { enabled: true, trace: t }))
    print(t.code)
    print(present(State { id: null }))
    print(present(State { id: Id { n: 7 } }))
    print(present(State { id: Id { n: 9 } }))
    print(present(State { id: Id { n: 8 } }))
    print(nullable_loop(null))
    print(nullable_loop(false))
    print(nullable_loop(true))
}
"#;
    let emitted = emit_mir(src);
    assert_eq!(
        run_luajit(&format!("{}\nmain()", emitted.lua), "condition_nullable"),
        "2\n1\n2\n2\n1\n2\n2\n1\n2\n0\n1\n1\n2\n2\n1\n2\n3\n0\n0\n1"
    );
}
