use super::*;

/// (case, source, cursor, new name, replacements that turn the source into the result)
type Case<'a> = (
    &'a str,
    &'a str,
    (u32, u32),
    &'a str,
    &'a [(&'a str, &'a str)],
);

#[track_caller]
fn assert_renames(cases: &[Case]) {
    for &(case, source, at, new_name, replacements) in cases {
        let project = Project::single(source);
        let reply = project.cli("rename", Some(at), &[new_name]);
        assert!(reply["error"].is_null(), "{case}: {reply}");
        let expected = replacements
            .iter()
            .fold(source.to_owned(), |text, (from, to)| {
                assert!(text.contains(from), "{case}: no {from:?} to replace");
                text.replace(from, to)
            });
        assert_eq!(project.renamed(&reply, "main.rock"), expected, "{case}");
    }
}

#[track_caller]
fn assert_refused(project: &Project, at: (u32, u32), new_name: &str, why: &str) {
    let reply = project.cli("rename", Some(at), &[new_name]);
    contains_text(string(&reply["error"]), why);
    let changes = &reply["changes"];
    assert!(
        changes.is_null()
            || object(changes)
                .values()
                .all(|edits| array(edits).is_empty()),
        "{reply}"
    );
}

const SHORTHAND: &str = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    let p = Point { x, y }
    print(x)
    print(y)
}"#;

const NEWTYPE: &str = r#"newtype UserId(int)

fn greet(id: UserId) {
    print(id)
}

fn main() {
    let id = UserId(42)
    greet(id)
}"#;

const MACRO: &str = r#"macro double(x: expr) -> int { x + x }

fn main() {
    print(double(5))
}"#;

const INLINE_LUA: &str = r#"inline(lua) fn probe(x: int) -> int = "return x + 1"

fn main() {
    print(probe(1))
}"#;

const NAMED_LABELS: &str = r#"fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(count: "a")
}"#;

#[test]
fn renames_locals_params_and_functions() {
    assert_renames(&[
        (
            "local after a non-BMP character on the line",
            "fn main() {\n    let foo = 42\n    print(\"\u{1f319}\") print(foo)\n}",
            (1, 8),
            "bar",
            &[("foo", "bar")],
        ),
        (
            "function parameter",
            r#"fn calculate(x: int, y: int) -> int {
    return x + y * x
}

fn main() {
    let result = calculate(10, 20)
    print(result)
}"#,
            (0, 13),
            "value",
            &[("x", "value")],
        ),
        (
            "function from its declaration",
            r#"struct Circle {
    radius: float,
}

fn area(c: Circle) -> float {
    return 3.14159 * c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 5.0 }
    print(area(c))
}"#,
            (4, 3),
            "calculate_area",
            &[("area", "calculate_area")],
        ),
        (
            "function used as a bare value",
            r#"fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(double, 10)
    print(result)
}"#,
            (9, 23),
            "triple",
            &[("double", "triple")],
        ),
        (
            "function at a safe-navigation call",
            r#"struct Mob {
    hp: int
}

fn mob_heal(m: Mob) -> int {
    return m.hp
}

fn main() {
    let m: Mob? = Mob { hp: 1 }
    print(m?.mob_heal() ?? 0)
}"#,
            (4, 3),
            "mob_restore",
            &[("mob_heal", "mob_restore")],
        ),
        (
            "function generic parameter stays in its function",
            r#"fn identity<T>(value: T) -> T {
    return value
}

fn map_pair<T, U>(first: T, second: U) -> (T, U) {
    return (first, second)
}"#,
            (0, 12),
            "Element",
            &[(
                "fn identity<T>(value: T) -> T {",
                "fn identity<Element>(value: Element) -> Element {",
            )],
        ),
        (
            "parameter updates named-argument labels",
            r#"fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(4, size: 2.0)
}"#,
            (0, 12),
            "amount",
            &[("count", "amount")],
        ),
        (
            "parameter from a caller's named label",
            NAMED_LABELS,
            (10, 13),
            "amount",
            &[
                ("spawn_it(count: int", "spawn_it(amount: int"),
                (
                    "print(count)\n    print(size)",
                    "print(amount)\n    print(size)",
                ),
                ("spawn_it(count: 3", "spawn_it(amount: 3"),
            ],
        ),
        (
            "parameter of the other overload from its named label",
            NAMED_LABELS,
            (11, 13),
            "label",
            &[
                (
                    "spawn_it(count: string) {\n    print(count)",
                    "spawn_it(label: string) {\n    print(label)",
                ),
                ("spawn_it(count: \"a\")", "spawn_it(label: \"a\")"),
            ],
        ),
        (
            "overloads from a call",
            r#"fn process(x: int) -> int {
    return x * 2
}

fn process(x: float) -> float {
    return x * 2.0
}

fn process(x: string) -> string {
    return x + "_processed"
}

fn main() {
    let i = process(42)
    let f = process(3.14)
    let s = process("test")
}"#,
            (13, 12),
            "convert",
            &[("process(", "convert(")],
        ),
        (
            "parameter of one overload",
            r#"fn calculate(x: int, y: int) -> int {
    return x + y
}

fn calculate(values: [int]) -> int {
    let sum = 0
    for v in values {
        sum = sum + v
    }
    return sum
}

fn main() {
    let result1 = calculate(5, 10)
    let result2 = calculate([1, 2, 3, 4, 5])
}"#,
            (0, 13),
            "a",
            &[(
                "(x: int, y: int) -> int {\n    return x",
                "(a: int, y: int) -> int {\n    return a",
            )],
        ),
        (
            "generic overloads",
            r#"fn convert<T>(items: [T], mapper: fn(T) -> T) -> [T] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn convert<K, V>(dict: Map<K, V>, fallback: V) -> V {
    return fallback
}

fn main() {
    let numbers = convert([1, 2, 3], x => x * 2)
    let d: Map<int, int> = map_new()
    let single = convert(d, 0)
}"#,
            (0, 3),
            "transform",
            &[("convert", "transform")],
        ),
        (
            "inner shadowing local",
            r#"fn main() {
    let x = 1
    if true {
        let x = 2
        print(x)
    }
    print(x)
}"#,
            (3, 12),
            "y",
            &[
                ("        let x = 2", "        let y = 2"),
                ("        print(x)", "        print(y)"),
            ],
        ),
        (
            "local shadowed in the same block",
            r#"fn main() {
    let x = 1
    let x = x + 1
    print(x)
}"#,
            (1, 8),
            "y",
            &[
                ("    let x = 1", "    let y = 1"),
                ("    let x = x + 1", "    let x = y + 1"),
            ],
        ),
        (
            "local inside string interpolation",
            "fn main() {\n    let n = 5\n    print(\"n = {n}\")\n}",
            (1, 8),
            "count",
            &[("let n", "let count"), ("{n}", "{count}")],
        ),
        (
            "lambda parameter",
            "fn main() {\n    let f = (a: int) => a + 1\n    print(f(1))\n}",
            (1, 13),
            "b",
            &[("(a: int) => a + 1", "(b: int) => b + 1")],
        ),
        (
            "function named in a string literal",
            r#"fn helper(x: int) -> int {
    return x * 2
}

fn main() {
    print("helper")
    print(helper(1))
}"#,
            (0, 3),
            "helper2",
            &[
                ("fn helper(", "fn helper2("),
                ("print(helper(1))", "print(helper2(1))"),
            ],
        ),
    ]);
}

#[test]
fn renames_fields_structs_and_patterns() {
    assert_renames(&[
        (
            "struct field",
            r#"struct Point {
    x: float,
    y: float,
}

fn distance(p: Point) -> float {
    return (p.x * p.x + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
    print(p.x)
}"#,
            (1, 4),
            "x_coord",
            &[("x", "x_coord")],
        ),
        (
            "shorthand variable in a struct literal",
            SHORTHAND,
            (9, 20),
            "x_coord",
            &[
                ("let x = 100", "let x_coord = 100"),
                ("Point { x, y }", "Point { x: x_coord, y }"),
                ("print(x)", "print(x_coord)"),
            ],
        ),
        (
            "second shorthand variable in a struct literal",
            SHORTHAND,
            (9, 23),
            "y_coord",
            &[
                ("let y = 200", "let y_coord = 200"),
                ("Point { x, y }", "Point { x, y: y_coord }"),
                ("print(y)", "print(y_coord)"),
            ],
        ),
        (
            "field used by a shorthand literal",
            r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 1
    let y = 2
    let p = Point { x, y }
    print(p.x)
}"#,
            (1, 4),
            "px",
            &[
                ("    x: int", "    px: int"),
                ("Point { x, y }", "Point { px: x, y }"),
                ("p.x", "p.px"),
            ],
        ),
        (
            "shorthand binding in a struct pattern",
            r#"struct Point {
    x: int
    y: int
}

fn get_sum(p: Point) -> int {
    match p {
        Point { x, y } => x + y,
    }
}"#,
            (7, 16),
            "qx",
            &[("Point { x, y } => x + y", "Point { x: qx, y } => qx + y")],
        ),
        (
            "binding in an enum pattern",
            r#"enum Option<T> {
    Some(T),
    None,
}

fn my_unwrap<T>(opt: Option<T>) -> T? {
    match opt {
        Option::Some(value) => value,
        Option::None => null,
    }
}"#,
            (7, 21),
            "inner",
            &[("value", "inner")],
        ),
        (
            "struct in nullable annotations",
            r#"struct Mob {
    hp: int
}

global CACHED: Mob? = null

fn take(m: Mob?) {
    print(m)
}

fn give() -> Mob? {
    let held: Mob? = null
    return held
}"#,
            (0, 7),
            "Beast",
            &[("Mob", "Beast")],
        ),
        (
            "field through safe navigation",
            r#"struct Inner {
    text: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o = Outer { inner: null }
    print(o.inner?.text ?? "none")
}"#,
            (1, 4),
            "label",
            &[("text", "label")],
        ),
        (
            "field referenced by a sibling default",
            r#"struct Spell {
    power: int,
    boosted: int = .power,
}

fn main() {
    let s = Spell { power: 2 }
    print(s.boosted)
}"#,
            (1, 4),
            "strength",
            &[("power", "strength")],
        ),
        (
            "field at a field-reference argument",
            r#"struct Mob {
    health: int,
}

fn main() {
    let m = Mob { health: 10 }
    print("{field_get(m, .health)}")
}"#,
            (1, 4),
            "vitality",
            &[("health", "vitality")],
        ),
        (
            "field no inline body names",
            r#"struct Mob {
    hp: int,
    mana: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.mana"

fn main() {
    let m = Mob { hp: 4, mana: 2 }
    print(m.hp + peek(m))
}"#,
            (1, 4),
            "health",
            &[("hp", "health")],
        ),
    ]);
}

#[test]
fn renames_enums() {
    let color = r#"enum Color {
    Red,
    Green,
}

fn describe(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
    }
}

fn main() {
    let c = Color::Red
    print(describe(c))
}"#;
    assert_renames(&[
        (
            "variant from its declaration",
            color,
            (1, 4),
            "Crimson",
            &[("Red", "Crimson")],
        ),
        (
            "variant from a pattern",
            color,
            (7, 15),
            "Crimson",
            &[("Red", "Crimson")],
        ),
        (
            "variant from a construction",
            color,
            (13, 19),
            "Crimson",
            &[("Red", "Crimson")],
        ),
        ("enum name", color, (0, 5), "Hue", &[("Color", "Hue")]),
        (
            "tuple variant",
            r#"enum Shape {
    Circle(float),
    Square(float),
}

fn area(s: Shape) -> float {
    match s {
        Shape::Circle(r) => r * r * 3.14,
        Shape::Square(w) => w * w,
    }
}

fn main() {
    let s = Shape::Circle(2.0)
    print(area(s))
}"#,
            (1, 4),
            "Round",
            &[("Circle", "Round")],
        ),
        (
            "same-named variant of another enum is untouched",
            r#"enum Color {
    Red,
    Green,
}

enum Paint {
    Red,
    Blue,
}

fn main() {
    let c = Color::Red
    let p = Paint::Red
    print(c)
    print(p)
}"#,
            (1, 4),
            "Crimson",
            &[
                ("    Red,\n    Green", "    Crimson,\n    Green"),
                ("Color::Red", "Color::Crimson"),
            ],
        ),
        (
            "record variant field from its declaration",
            r#"enum Item {
    Potion { strength: int },
    Coin,
}

fn main() {
    let i = Item::Potion { strength: 3 }
    match i {
        Item::Potion { strength } => print(strength),
        Item::Coin => print(0),
    }
}"#,
            (1, 13),
            "power",
            &[
                ("Potion { strength: int }", "Potion { power: int }"),
                ("Potion { strength: 3 }", "Potion { power: 3 }"),
                ("Potion { strength } =>", "Potion { power: strength } =>"),
            ],
        ),
        (
            "record variant field from a literal",
            r#"enum Item {
    Potion { strength: int },
}

fn main() {
    let i = Item::Potion { strength: 3 }
    print(i)
}"#,
            (5, 27),
            "power",
            &[("strength", "power")],
        ),
    ]);
}

#[test]
fn renames_globals_newtypes_decorators_and_macro_sites() {
    assert_renames(&[
        (
            "global from its declaration",
            r#"global SCORE: int = 0

fn bump() {
    SCORE = SCORE + 1
}

fn main() {
    bump()
    print("s {SCORE}")
}"#,
            (0, 7),
            "POINTS",
            &[("SCORE", "POINTS")],
        ),
        (
            "global from a use leaves a longer name alone",
            r#"global SCORE: int = 0
global SCOREBOARD: int = 1

fn bump() {
    SCORE = SCORE + 1
    SCOREBOARD = 2
}"#,
            (4, 4),
            "POINTS",
            &[
                ("SCORE:", "POINTS:"),
                ("SCORE = SCORE + 1", "POINTS = POINTS + 1"),
            ],
        ),
        (
            "newtype from an annotation",
            NEWTYPE,
            (2, 13),
            "PlayerId",
            &[("UserId", "PlayerId")],
        ),
        (
            "decorator from its declaration",
            r#"deco cool {
    print(1)
}

@cool
fn thing() {
    print(1)
}

@cool
fn other() {
    print(2)
}"#,
            (0, 5),
            "fancy",
            &[("cool", "fancy")],
        ),
        (
            "decorator from an application",
            "deco cool {\n    print(1)\n}\n\n@cool\nfn thing() {\n    print(1)\n}",
            (4, 1),
            "fancy",
            &[("cool", "fancy")],
        ),
        (
            "macro parameter",
            MACRO,
            (0, 13),
            "v",
            &[("(x: expr) -> int { x + x }", "(v: expr) -> int { v + v }")],
        ),
        (
            "local passed as a macro argument",
            "macro double(x: expr) -> int { x + x }\n\nfn main() {\n    let n = 5\n    print(double(n))\n}",
            (3, 8),
            "m",
            &[("let n", "let m"), ("double(n)", "double(m)")],
        ),
        (
            "function called inside a macro body",
            r#"fn helper(x: int) -> int {
    return x + 1
}

macro twice(x: expr) -> int { helper(helper(x)) }

fn main() {
    print(twice(1))
    print(helper(2))
}"#,
            (0, 3),
            "inc",
            &[("helper", "inc")],
        ),
        (
            "inline function name",
            INLINE_LUA,
            (0, 15),
            "probe2",
            &[("probe", "probe2")],
        ),
        (
            "parameter spelled like another inline body's text",
            &INLINE_LUA.replace(
                "fn main() {\n    print(probe(1))",
                "fn twice(x: int) -> int {\n    return probe(x) * 2",
            ),
            (2, 9),
            "n",
            &[("twice(x", "twice(n"), ("probe(x)", "probe(n)")],
        ),
    ]);
}

#[test]
fn refuses_unsafe_renames_and_invalid_names() {
    let local = "fn main() {\n    let foo = 42\n    print(foo)\n}";
    let cases: &[(&str, (u32, u32), &str, &str)] = &[
        (
            local,
            (1, 8),
            "fn",
            "cannot rename to `fn`: not an identifier",
        ),
        (
            local,
            (1, 8),
            "1 bad",
            "cannot rename to `1 bad`: not an identifier",
        ),
        (MACRO, (0, 6), "twice", "cannot rename a macro"),
        (
            "macro bump() { ^count = ^count + 1 }\n\nfn main() {\n    let count = 0\n    bump()\n    print(count)\n}",
            (3, 8),
            "total",
            ".rock:5 that rename cannot rewrite",
        ),
        (
            "global SCORE: int = 0\n\nmacro bump() { _G.SCORE = _G.SCORE + 1 }\n\nfn main() {\n    bump()\n    print(SCORE)\n}",
            (0, 7),
            "POINTS",
            ".rock:6 that rename cannot rewrite",
        ),
        (
            &INLINE_LUA.replace(
                "inline(lua) fn probe(x: int) -> int = \"return x + 1\"",
                "inline(js) fn probe(x: int) -> int = \"return x + 1;\"",
            ),
            (0, 20),
            "y",
            "the opaque inline(js) body of `probe`",
        ),
        (
            "@nomangle\nfn helper(x: int) -> int {\n    return x * 2\n}\n\ninline(lua) fn call_helper(x: int) -> int = \"return helper(x)\"\n\nfn main() {\n    print(call_helper(1))\n}",
            (1, 3),
            "helper2",
            "cannot rename `helper`: `@nomangle` keeps its emitted name",
        ),
        (
            "@nomangle\nfn helper(x: int) -> int {\n    return x * 2\n}\n\nfn main() {\n    print(helper(1))\n}",
            (6, 10),
            "helper2",
            "`@nomangle` keeps its emitted name, which code outside Rock calls",
        ),
        (
            "struct Mob {\n    hp: int,\n}\n\ninline(lua) fn peek(m: Mob) -> int = \"return m.hp\"\n\nfn main() {\n    let m = Mob { hp: 4 }\n    print(peek(m))\n}",
            (1, 4),
            "health",
            "the opaque inline(lua) body of `peek`",
        ),
        (
            "global SCORE: int = 0\n\ninline(lua) fn peek() -> int = \"return SCORE\"\n\nfn main() {\n    print(peek())\n}",
            (0, 7),
            "POINTS",
            "the opaque inline(lua) body of `peek`",
        ),
        (
            "struct Mob {\n    hp: int,\n}\n\ninline(lua) fn is_mob(v: any) -> bool = \"return v.__ty == 'Mob'\"\n\nfn main() {\n    print(is_mob(Mob { hp: 1 }))\n}",
            (0, 7),
            "Beast",
            "the opaque inline(lua) body of `is_mob`",
        ),
    ];
    for &(source, at, new_name, why) in cases {
        assert_refused(&Project::single(source), at, new_name, why);
    }
}

#[track_caller]
fn assert_package_rename(files: &[(&str, &str)], at: (u32, u32), from: &str, to: &str) {
    let project = Project::package(files[0].0, files);
    let reply = project.cli("rename", Some(at), &[to]);
    assert!(reply["error"].is_null(), "{reply}");
    for (name, source) in files {
        assert_eq!(
            project.renamed(&reply, name),
            source.replace(from, to),
            "{name}"
        );
    }
}

#[test]
fn renames_across_package_files() {
    assert_package_rename(
        &[
            (
                "math.rock",
                "fn add(a: int, b: int) -> int {\n    return a + b\n}\n\nfn multiply(a: int, b: int) -> int {\n    return a * b\n}",
            ),
            (
                "main.rock",
                "fn main() {\n    let sum = add(5, 3)\n    let product = multiply(5, 3)\n    print(\"{sum} {product}\")\n}",
            ),
        ],
        (0, 3),
        "add",
        "sum_two_numbers",
    );
    assert_package_rename(
        &[
            (
                "types.rock",
                "struct Point {\n    x: float,\n    y: float,\n}\n\nfn distance(p1: Point, p2: Point) -> float {\n    let dx = p2.x - p1.x\n    let dy = p2.y - p1.y\n    return (dx * dx + dy * dy).sqrt()\n}",
            ),
            (
                "geometry.rock",
                "fn create_origin() -> Point {\n    return Point { x: 0.0, y: 0.0 }\n}\n\nfn create_unit_square() -> [Point] {\n    return [Point { x: 0.0, y: 0.0 }, Point { x: 1.0, y: 1.0 }]\n}",
            ),
            (
                "main.rock",
                "fn main() {\n    let origin: Point = create_origin()\n    for p in create_unit_square() {\n        print(distance(origin, p))\n    }\n}",
            ),
        ],
        (0, 7),
        "Point",
        "Point2D",
    );
    let main = "inline(lua) fn peek() -> int = \"return 1\"\n\nfn main() {\n    SCORE = 2\n    print(peek())\n}";
    let files = [("state.rock", "global SCORE: int = 0"), ("main.rock", main)];
    assert_package_rename(&files, (0, 7), "SCORE", "POINTS");
    let reader = main.replace("return 1", "return SCORE");
    let files = [("state.rock", files[0].1), ("main.rock", &reader)];
    assert_refused(
        &Project::package("state.rock", &files),
        (0, 7),
        "POINTS",
        "the opaque inline(lua) body of `peek` (main.rock:1)",
    );
}
