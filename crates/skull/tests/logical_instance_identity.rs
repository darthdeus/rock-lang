use crate::common::{InlineOffGuard, callees_named_in, definitions_of, emit_module};

#[test]
fn structural_generic_arguments_emit_distinct_called_definitions() {
    let _inline = InlineOffGuard::on();
    let cases = [
        (
            "same-shape nominals",
            r#"
value P { x: float, y: float }
value Q { x: float, y: float }
fn ident<T>(v: T) -> T { return v }
fn main() {
    let p = ident(P { x: 1.0, y: 2.0 })
    let q = ident(Q { x: 1.0, y: 2.0 })
    print("{p.x} {q.y}")
}
"#,
            2,
        ),
        (
            "function-typed",
            r#"
fn ident<T>(x: T) -> T { return x }
fn nudge(a: int) -> int { return a + 1 }

fn main() {
    let g: fn(int) -> int = nudge
    let f = ident(g)
    let t = ident((1, 2))
    let a = ident([1, 2])
    let n: int? = 3
    let o = ident(n)
    let m = ident(1)
    print("{t} {a} {o} {m} {f(1)}")
}
"#,
            5,
        ),
        (
            "coroutine-typed",
            r#"
fn ident<T>(x: T) -> T { return x }

co fn counter() -> Coroutine<int, int, ()> {
    yield 1
    return 2
}

fn main() {
    let c = counter()
    let a = ident(c)
    let m: Map<string, Coroutine<int, int>> = map_new()
    let b = ident(m)
    print("{b}")
    let _ = a
}
"#,
            2,
        ),
        (
            "nested application",
            r#"
fn ident<T>(x: T) -> T { return x }

fn main() {
    let inner: [int] = [1]
    let outer: [[int]] = [[1]]
    let a = ident(inner)
    let b = ident(outer)
    print("{a} {b}")
}
"#,
            2,
        ),
    ];
    for (case, src, expected) in cases {
        let lua = emit_module(src);
        let definitions = definitions_of(&lua, "ident");
        assert_eq!(definitions.len(), expected, "{case}: {definitions:?}");
        assert_eq!(
            callees_named_in(&lua, "main", "ident"),
            {
                let mut sorted = definitions;
                sorted.sort();
                sorted
            },
            "{case}: each instantiation must call its own definition",
        );
    }
}
