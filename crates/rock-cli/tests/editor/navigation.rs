use super::*;

#[test]
fn hover_cases() {
    let cases = [
        (
            "receiver parameter of a dotted-form function",
            "struct Mob { hp: int }\n\nfn Mob.take_damage(m, amount: int) -> int {\n    return m.hp - amount\n}",
            (3, 11),
            "Type: struct Mob",
        ),
        (
            "parameter while another function has a syntax error",
            "fn calculate(num: int) -> int {\n    return num * 2\n}\n\nfn main() {\n    let broken = this is invalid syntax\n}",
            (1, 12),
            "Type: int",
        ),
    ];
    for (case, source, at, expected) in cases {
        assert_eq!(Project::single(source).hover(at), expected, "{case}");
    }
}

#[test]
fn type_definition_cases() {
    let mob = "struct Mob {\n    health: int\n}\n\nfn spawn() -> Mob {\n    return Mob { health: 100 }\n}\n\nfn find() -> Mob? {\n    return null\n}\n\nfn main() {\n    let mob = spawn()\n    let other = mob\n    let m = find()\n}";
    let cases: &[(&str, &str, (u32, u32), Option<(u64, u64)>)] = &[
        ("local bound to a struct", mob, (13, 8), Some((0, 7))),
        ("use of the local", mob, (14, 16), Some((0, 7))),
        ("nullable peeled to its struct", mob, (15, 8), Some((0, 7))),
        (
            "field access",
            "struct Inner { value: int }\nstruct Outer { inner: Inner }\n\nfn main() {\n    let o = Outer { inner: Inner { value: 1 } }\n    let i = o.inner\n}",
            (5, 14),
            Some((0, 7)),
        ),
        (
            "enum value",
            "enum Color { Red, Green, Blue }\n\nfn main() {\n    let c = Color::Red\n}",
            (3, 8),
            Some((0, 5)),
        ),
        (
            "coroutine goes to its yield type",
            "struct CoFrame { wait: bool }\n\nco fn maker() -> Coroutine<CoFrame> {\n    yield CoFrame { wait: true }\n}\n\nfn main() {\n    let c = maker()\n}",
            (7, 8),
            Some((0, 7)),
        ),
        ("primitive", "fn main() {\n    let n = 42\n}", (1, 8), None),
        (
            "coroutine of a primitive",
            "co fn nums() -> Coroutine<int> {\n    yield 1\n}\n\nfn main() {\n    let c = nums()\n}",
            (5, 8),
            None,
        ),
    ];
    for &(case, source, at, expected) in cases {
        let found = Project::single(source).definition("type-definition", at);
        assert_eq!(found, expected, "{case}");
    }
    let vec = Project::single("fn main() {\n    let xs = [1, 2, 3]\n}");
    assert!(
        vec.definition("type-definition", (1, 8)).is_some(),
        "Vec reaches the stdlib"
    );
}

#[test]
fn references_cases() {
    let macro_fragments = "macro decl(Name: ident, Elem: ty) {\n    struct Name { v: Elem }\n}";
    let cases: &[(&str, &str, (u32, u32), &[(u32, u32)])] = &[
        (
            "local",
            "fn main() {\n    let count = 0\n    print(count)\n    count = count + 1\n    if count > 10 {\n        print(\"Done\")\n    }\n}",
            (1, 8),
            &[(1, 8), (2, 10), (3, 4), (3, 12), (4, 7)],
        ),
        (
            "function",
            "fn calculate(a: int, b: int) -> int {\n    return a + b\n}\n\nfn main() {\n    let x = calculate(5, 3)\n    let y = calculate(10, 20)\n    print(calculate(x, y))\n}",
            (0, 3),
            &[(0, 3), (5, 12), (6, 12), (7, 10)],
        ),
        (
            "function used as a bare value",
            "fn apply(f: fn(int) -> int, x: int) -> int {\n    return f(x)\n}\n\nfn double(x: int) -> int {\n    return x * 2\n}\n\nfn main() {\n    let result = apply(double, 10)\n    print(result)\n}",
            (9, 23),
            &[(4, 3), (9, 23)],
        ),
        (
            "struct type",
            "struct Point {\n    x: float,\n    y: float,\n}\n\nfn distance(p1: Point, p2: Point) -> float {\n    return p2.x - p1.x\n}\n\nfn main() {\n    let origin: Point = Point { x: 0.0, y: 0.0 }\n    let p = Point { x: 3.0, y: 4.0 }\n}",
            (0, 7),
            &[(0, 7), (5, 16), (5, 27), (10, 16), (10, 24), (11, 12)],
        ),
        (
            "struct field",
            "struct Person {\n    name: string,\n    age: int,\n}\n\nfn main() {\n    let p = Person { name: \"Alice\", age: 30 }\n    print(p.name)\n    let n = p.name\n    if p.age > 18 {\n        print(\"Adult: \" + p.name)\n    }\n}",
            (1, 4),
            &[(1, 4), (6, 21), (7, 12), (8, 14), (10, 28)],
        ),
        (
            "parameter",
            "fn greet(name: string, times: int) {\n    for i in 0..times {\n        print(\"Hello, \" + name)\n    }\n    if times > 0 {\n        print(\"Greeted \" + name)\n    }\n}",
            (0, 9),
            &[(0, 9), (2, 26), (5, 27)],
        ),
        (
            "UFCS method",
            "fn inc(x: int) -> int { x + 1 }\nfn main() {\n    let a = inc(5)\n    let b = 5.inc()\n}",
            (3, 14),
            &[(0, 3), (2, 12), (3, 14)],
        ),
        (
            "fn-typed parameter called",
            "fn apply(g: fn() -> int) -> int {\n    g() + g()\n}",
            (1, 10),
            &[(0, 9), (1, 4), (1, 10)],
        ),
        (
            "macro ident fragment",
            macro_fragments,
            (0, 11),
            &[(0, 11), (1, 11)],
        ),
        (
            "macro ty fragment",
            macro_fragments,
            (0, 24),
            &[(0, 24), (1, 21)],
        ),
        (
            "macro ident in nested path patterns",
            "macro choose(Name: ident, value: expr) {\n    match value {\n        Outer::Wrap(Name::Leaf) => 1,\n        Name::Record { child: Name::Leaf } => 2,\n        _ => 0\n    }\n}\nfn main() {}",
            (0, 13),
            &[(0, 13), (2, 20), (3, 8), (3, 30)],
        ),
    ];
    for &(case, source, at, expected) in cases {
        let reply = Project::single(source).cli("references", Some(at), &[]);
        reference_starts(case, &reply["references"], expected);
    }
}
