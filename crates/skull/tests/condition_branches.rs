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
    assert_eq!(run_luajit(&format!("{}\n{checks}", emitted.lua)), "ok");
}
