use super::*;

#[test]
fn inlay_hint_cases() {
    let cases: &[(&str, &str, &[(u64, &str)])] = &[
        (
            "let bindings",
            "fn main() {\n    let x = 42\n    let y = \"hello\"\n    let z = [1, 2, 3]\n    let w = true\n}",
            &[(1, "int"), (2, "string"), (3, "[int]"), (4, "bool")],
        ),
        (
            "struct, vec and inferred map types",
            "struct Point {\n    x: float,\n    y: float,\n}\n\nfn main() {\n    let p = Point { x: 1.0, y: 2.0 }\n    let points = [p, Point { x: 3.0, y: 4.0 }]\n    let map = map_new()\n    map.insert(\"origin\", p)\n}",
            &[(6, "Point"), (7, "[Point]"), (8, "Map<string, Point>")],
        ),
        (
            "generic results and parameter names",
            "fn identity<T>(value: T) -> T {\n    return value\n}\n\nfn main() {\n    let num = identity(42)\n    let text = identity(\"hello\")\n    let list = identity([1, 2, 3])\n}",
            &[
                (5, "int"),
                (5, "value:"),
                (6, "string"),
                (6, "value:"),
                (7, "[int]"),
            ],
        ),
        (
            "nullable results",
            "fn find_user(id: int) -> string? {\n    return null\n}\n\nfn main() {\n    let user = find_user(1)\n}",
            &[(5, "string?"), (5, "id:")],
        ),
        (
            "closure",
            "fn main() {\n    let multiply = (x: int, y: int) => { return x * y }\n}",
            &[(1, "fn(int, int) -> int")],
        ),
        (
            "for loop variable",
            "struct Item { name: string }\n\nfn main() {\n    let items: [Item] = vec_new()\n    for item in items {\n        print(item.name)\n    }\n}",
            &[(4, "Item")],
        ),
        (
            "match arm binding",
            "struct Point { x: int, y: int }\n\nenum Opt {\n    None,\n    Some(Point),\n}\n\nfn main() {\n    let o = Opt::Some(Point { x: 1, y: 2 })\n    match o {\n        Opt::Some(p) => print(p.x),\n        Opt::None => {},\n    }\n}",
            &[(8, "Opt"), (10, "Point")],
        ),
        (
            "dotted-form receivers",
            "struct Mob { health: float }\n\nfn Mob.take_damage(m, amount: float) {\n    m.health = m.health - amount\n}\n\nfn Vec<T>.first(v) -> T {\n    return v[0]\n}",
            &[(2, "Mob"), (6, "Vec<T>")],
        ),
        (
            "while another function has a syntax error",
            "fn process(value: int) -> string {\n    return value.to_string()\n}\n\nfn get_result() -> string {\n    let result = process(123)\n    return result\n}\n\nfn main() {\n    let broken = invalid syntax here!!!\n}",
            &[(5, "string"), (5, "value:")],
        ),
    ];
    for &(case, source, expected) in cases {
        let reply = Project::single(source).cli("inlay-hints", None, &[]);
        let hints: Vec<_> = array(&reply["hints"])
            .iter()
            .map(|hint| {
                let label = string(&hint["label"]).trim();
                let label = label.strip_prefix(':').unwrap_or(label).trim();
                (hint["position"]["line"].as_u64().unwrap(), label)
            })
            .collect();
        for hint in expected {
            assert!(
                hints.contains(hint),
                "{case}: missing {hint:?} in {hints:?}"
            );
        }
    }
}
