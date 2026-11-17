use crate::common::{InlineOffGuard, callees_named_in, emit_module, function_body};

#[test]
fn generic_defaults_call_their_own_specializations() {
    let _inline = InlineOffGuard::on();
    let lua = emit_module(
        r#"
fn seed<T>() -> T? { return null }
struct Box<T> { value: T? = seed() }
fn main() {
    let a: Box<int> = Box {}
    let b: Box<string> = Box {}
    print("{a.value} {b.value}")
}
"#,
    );
    assert_eq!(callees_named_in(&lua, "main", "seed").len(), 2);
}

#[test]
fn written_fields_do_not_call_their_defaults() {
    let _inline = InlineOffGuard::on();
    let lua = emit_module(
        r#"
fn used_seed() -> int { return 7 }
fn unused_seed() -> int { return 9 }
struct Cfg { n: int = used_seed(), k: int = unused_seed(), m: int }
fn main() {
    let c = Cfg { k: 0, m: 1 }
    print("{c.n + c.k + c.m}")
}
"#,
    );
    assert_eq!(callees_named_in(&lua, "main", "used_seed").len(), 1);
    assert!(callees_named_in(&lua, "main", "unused_seed").is_empty());
}

#[test]
fn synthesized_helpers_are_emitted_at_their_operations() {
    for (helper, src) in [
        (
            "__rock_stride_reserve",
            "nobox value S { a: int, b: int, c: int, d: int, e: int }\n\
             fn main() {\n\
                 let xs: [S] = []\n\
                 xs.push(S { a: 1, b: 2, c: 3, d: 4, e: 5 })\n\
                 print(\"{xs[0].c}\")\n\
             }",
        ),
        (
            "__arena_store_or_empty",
            "struct Row { x: int }\n\
             fn main() {\n\
                 let a: Arena<Row> = arena_new()\n\
                 a.insert(Row { x: 2 })\n\
                 let sum = 0\n\
                 for (i, (e, row)) in enumerate(a) { sum += i + row.x }\n\
                 print(\"{sum}\")\n\
             }",
        ),
    ] {
        let lua = emit_module(src);
        let body = function_body(&lua, "main");
        assert!(body.contains(&format!("{helper}(")), "{body}");
    }
}
