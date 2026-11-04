use crate::common::{emit_module, function_body, run_luajit};

const SOURCE: &str = r#"
value Row { text: string, n: int }
struct Counter { calls: int = 0 }
@nomangle fn next_row(c: Counter) -> Row? {
    c.calls += 1
    if c.calls == 1 { return Row { text: "first", n: 7 } }
    return null
}
@nomangle fn read_once(c: Counter) -> int { return next_row(c)?.n ?? -1 }
fn main() {
    let c = Counter {}
    print("{read_once(c)} {c.calls}")
}
"#;

#[test]
fn optional_receiver_call_is_evaluated_once() {
    let lua = emit_module(SOURCE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "optional_receiver_once"),
        "7 1"
    );
}

#[test]
fn optional_receiver_fallback_does_not_emit_a_discarded_call() {
    let lua = emit_module(SOURCE);
    let body = function_body(&lua, "read_once");
    assert_eq!(body.matches("next_row(").count(), 1, "{body}");
}

#[test]
fn optional_receiver_representations_preserve_absence_and_selected_values() {
    let src = r#"
value Pair { x: int, y: int }
nobox value Flat { x: int, y: int }
struct RefRow { n: int }
struct Owner { row: Row? = null, pair: Pair? = null, flat: Flat? = null }
value Row { text: string, n: int }
fn scalar_pair(present: bool) -> Pair? {
    if present { return Pair { x: 13, y: 17 } }
    return null
}
fn tuple_pair(present: bool) -> (int, int)? {
    if present { return (19, 23) }
    return null
}
fn reference_row(present: bool) -> RefRow? {
    if present { return RefRow { n: 29 } }
    return null
}
fn main() {
    for present in [false, true] {
        let h = Owner {}
        if present {
            h.row = Row { text: "row", n: 31 }
            h.pair = Pair { x: 37, y: 41 }
            h.flat = Flat { x: 43, y: 47 }
        }
        print("{scalar_pair(present)?.x ?? -1} {tuple_pair(present)?.0 ?? -1} {reference_row(present)?.n ?? -1}")
        print("{h.row?.n ?? -1} {h.pair?.y ?? -1} {h.flat?.x ?? -1}")
    }
}
"#;
    let lua = emit_module(src);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "optional_receiver_shapes"),
        "-1 -1 -1\n-1 -1 -1\n13 19 29\n31 41 43"
    );
}
