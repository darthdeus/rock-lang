use super::*;

#[test]
fn signature_help_cases() {
    let cases = [
        (
            "inner call of a nested call",
            "fn add(a: int, b: int) -> int {\n    return a + b\n}\n\nfn multiply(x: int, y: int) -> int {\n    return x * y\n}\n\nfn main() {\n    let result = multiply(add(2, 3), 4)\n}",
            (9, 30),
            "add(a: int, b: int) -> int",
        ),
        (
            "active overload follows the argument",
            "fn process(x: int) -> int {\n    return x * 2\n}\n\nfn process(x: float) -> float {\n    return x * 2.0\n}\n\nfn main() {\n    let i = process(42)\n    let f = process(3.14)\n}",
            (10, 20),
            "fn process(x: float) -> float",
        ),
        (
            "generic function",
            "fn map<T, U>(items: [T], mapper: fn(T) -> U) -> [U] {\n    return []\n}\n\nfn main() {\n    let numbers = map([1, 2, 3], (x) => x * 2)\n}",
            (5, 22),
            "fn map(items: [T], mapper: fn(T) -> U) -> [U]",
        ),
        (
            "inside a lambda argument",
            "fn filter<T>(items: [T], predicate: fn(T) -> bool) -> [T] {\n    return items\n}\n\nfn main() {\n    let evens = filter([1, 2, 3, 4], (x) => x % 2 == 0)\n}",
            (5, 37),
            "fn filter(items: [T], predicate: fn(T) -> bool) -> [T]",
        ),
        (
            "struct literal",
            "struct Point {\n    x: float,\n    y: float,\n}\n\nfn main() {\n    let p = Point { x: 3.0, y: 4.0 }\n}",
            (6, 21),
            "Point { x: float, y: float }",
        ),
    ];
    for (case, source, at, label) in cases {
        let reply = Project::single(source).cli("signature-help", Some(at), &[]);
        let active = reply["activeSignature"].as_u64().unwrap_or(0) as usize;
        let found = string(&reply["signatures"][active]["label"]);
        assert!(found.contains(label), "{case}: {reply}");
    }
}
