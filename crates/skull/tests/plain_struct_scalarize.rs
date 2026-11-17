use crate::common::{emit_module, function_body};

const PLAIN_FIELD_LOOP: &str = r#"
struct Props { a: int, b: int, c: int }

@inline fn mk(x: int) -> Props { return Props { a: x, b: x * 2, c: x * 3 } }

fn hot(w: int, h: int) -> int {
    let acc = 0
    for gy in 0..h {
        for gx in 0..w {
            let p = mk(gx + gy)
            let kind = ((gx * 7) + (gy * 13)) % 3
            if kind == 0 {
                acc = (acc + (p.a * 2)) - p.b
            } else if kind == 1 {
                acc = (acc - p.b) + p.c
            } else {
                acc = (acc + p.a) + p.c
            }
        }
    }
    return acc
}

fn main() {
    let _ = hot(2, 2)
}
"#;

#[test]
fn plain_struct_field_reads_dont_materialize_table() {
    let lua = emit_module(PLAIN_FIELD_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("__ty = \"Props\""));
}

const PLAIN_MUTATE_LOOP: &str = r#"
struct Acc { sum: int, n: int }

fn hot(w: int) -> int {
    let total = 0
    for i in 0..w {
        let a = Acc { sum: 0, n: 0 }
        a.sum = i * 2
        a.n = i
        total = total + (a.sum + a.n)
    }
    return total
}

fn main() {
    let _ = hot(4)
}
"#;

#[test]
fn plain_struct_field_writes_scalarize() {
    let lua = emit_module(PLAIN_MUTATE_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(!hot.contains("__ty = \"Acc\""));
}

const PLAIN_WHOLE_USE: &str = r#"
struct Box { w: int, h: int }

fn area(b: Box) -> int { return b.w * b.h }

fn use_it() -> int {
    let b = Box { w: 3, h: 4 }
    let direct = b.w
    return area(b) + direct
}

fn main() {
    let _ = use_it()
}
"#;

#[test]
fn plain_struct_whole_use_keeps_table() {
    let lua = emit_module(PLAIN_WHOLE_USE);
    let body = function_body(&lua, "use_it");
    assert!(body.contains("__ty = \"Box\""));
    assert!(body.contains(".w"));
}
