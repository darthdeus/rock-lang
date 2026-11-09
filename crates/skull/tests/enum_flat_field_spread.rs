use crate::common::compile_and_run;

const STRUCT_UPDATE: &str = r#"
nobox value Spec { x: float, y: float }
enum Shape { A(Spec), B(Spec) }
struct Holder { id: int, flag: bool, shape: Shape }

fn main() {
    let h = Holder { id: 17, flag: true, shape: Shape::A(Spec { x: 3.0, y: 5.0 }) }
    let c = Holder { ..h, flag: false }
    match c.shape {
        Shape::A(s) => print("{c.id} {c.flag} {s.x as int} {s.y as int}"),
        Shape::B(s) => print("wrong {s.x}"),
    }
}
"#;

const CALL_SPREAD: &str = r#"
nobox value Spec { x: float, y: float }
enum Shape { A(Spec), B(Spec) }
struct Holder { id: int, flag: bool, shape: Shape }

fn consume(id: int, flag: bool, shape: Shape) {
    match shape {
        Shape::A(s) => print("{id} {flag} {s.x as int} {s.y as int}"),
        Shape::B(s) => print("wrong {s.x}"),
    }
}

fn main() {
    let h = Holder { id: 19, flag: true, shape: Shape::A(Spec { x: 7.0, y: 11.0 }) }
    consume(..h)
}
"#;

const NOBOX_CHILD_CALL_SPREAD: &str = r#"
nobox value Spec { x: float, y: float }
struct Holder { p: Spec, id: int }

fn consume(p: Spec, id: int) {
    print("{id} {p.x as int} {p.y as int}")
}

fn main() {
    let h = Holder { p: Spec { x: 23.0, y: 29.0 }, id: 31 }
    consume(..h)
}
"#;

const NULLABLE_NOBOX_CHILD_CALL_SPREAD: &str = r#"
nobox value Spec { x: float, y: float }
struct Holder { p: Spec?, id: int }

fn consume(p: Spec?, id: int) {
    if p == null {
        print("{id} absent")
        return
    }
    let s = p
    print("{id} {s.x as int} {s.y as int}")
}

fn main() {
    let present = Holder { p: Spec { x: 37.0, y: 41.0 }, id: 43 }
    let absent = Holder { p: null, id: 47 }
    consume(..present)
    consume(..absent)
}
"#;

#[test]
fn struct_update_carries_a_flat_enum_field_from_its_physical_slots() {
    assert_eq!(compile_and_run(STRUCT_UPDATE).trim(), "17 false 3 5");
}

#[test]
fn call_spread_carries_a_flat_enum_field_from_its_physical_slots() {
    assert_eq!(compile_and_run(CALL_SPREAD).trim(), "19 true 7 11");
}

#[test]
fn call_spread_carries_a_nobox_child_from_its_physical_slots() {
    assert_eq!(compile_and_run(NOBOX_CHILD_CALL_SPREAD).trim(), "31 23 29");
}

#[test]
fn call_spread_carries_nullable_nobox_presence_and_leaves() {
    assert_eq!(
        compile_and_run(NULLABLE_NOBOX_CHILD_CALL_SPREAD).trim(),
        "43 37 41\n47 absent"
    );
}
