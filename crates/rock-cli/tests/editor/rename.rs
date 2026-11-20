use super::*;

// <|> marks the request position. [[old]] expects the new name and [[old=>text]]
// expects an explicit replacement, such as the expansion of a shorthand field.
fn rename_fixture(marked: &str, new_name: &str) -> (String, String) {
    let mut source = String::new();
    let mut expected = String::new();
    let mut rest = marked;
    while let Some((head, edit)) = rest.split_once("[[") {
        let (edit, tail) = edit.split_once("]]").expect("unclosed rename edit");
        let (before, after) = edit.split_once("=>").unwrap_or((edit, new_name));
        source.push_str(head);
        source.push_str(before);
        expected.push_str(&head.replace("<|>", ""));
        expected.push_str(after);
        rest = tail;
    }
    source.push_str(rest);
    expected.push_str(&rest.replace("<|>", ""));
    (source, expected)
}

fn unmark(mut marked: &str) -> (String, Vec<(u32, u32)>) {
    let mut source = String::new();
    let mut positions = Vec::new();
    while let Some((head, tail)) = marked.split_once("<|>") {
        source.push_str(head);
        let line = source.matches('\n').count() as u32;
        let col = source.rsplit('\n').next().unwrap().encode_utf16().count() as u32;
        positions.push((line, col));
        marked = tail;
    }
    source.push_str(marked);
    (source, positions)
}

#[track_caller]
fn assert_renames(cases: &[(&str, &str, &str)]) {
    for &(case, marked, new_name) in cases {
        let (marked, expected) = rename_fixture(marked, new_name);
        let (source, cursors) = unmark(&marked);
        assert!(!cursors.is_empty(), "{case}: missing cursor");
        let project = Project::single(&source);
        for at in cursors {
            let reply = project.cli("rename", Some(at), &[new_name]);
            assert!(reply["error"].is_null(), "{case} at {at:?}: {reply}");
            assert_eq!(
                project.renamed(&reply, "main.rock"),
                expected,
                "{case} at {at:?}"
            );
        }
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

#[test]
fn renames_locals_params_and_functions() {
    assert_renames(&[
        (
            "local use after a non-BMP character",
            "fn main() {\n    let [[<|>foo]] = 42\n    print(\"\u{1f319}\") print([[foo]])\n}",
            "bar",
        ),
        (
            "function parameter",
            r#"fn calculate([[<|>x]]: int, y: int) -> int {
    return [[x]] + y * [[x]]
}

fn main() {
    let result = calculate(10, 20)
    print(result)
}"#,
            "value",
        ),
        (
            "function from its declaration",
            r#"struct Circle {
    radius: float,
}

fn [[<|>area]](c: Circle) -> float {
    return 3.14159 * c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 5.0 }
    print([[area]](c))
}"#,
            "calculate_area",
        ),
        (
            "function used as a bare value",
            r#"fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn [[double]](x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply([[<|>double]], 10)
    print(result)
}"#,
            "triple",
        ),
        (
            "function referenced by safe navigation",
            r#"struct Mob {
    hp: int
}

fn [[<|>mob_heal]](m: Mob) -> int {
    return m.hp
}

fn main() {
    let m: Mob? = Mob { hp: 1 }
    print(m?.[[mob_heal]]() ?? 0)
}"#,
            "mob_restore",
        ),
        (
            "function generic parameter stays in its function",
            r#"fn identity<[[<|>T]]>(value: [[T]]) -> [[T]] {
    return value
}

fn map_pair<T, U>(first: T, second: U) -> (T, U) {
    return (first, second)
}"#,
            "Element",
        ),
        (
            "parameter updates named-argument labels",
            r#"fn spawn_it([[<|>count]]: int, size: float) {
    print([[count]])
    print(size)
}

fn main() {
    spawn_it([[count]]: 3, size: 1.0)
    spawn_it(4, size: 2.0)
}"#,
            "amount",
        ),
        (
            "parameter from a caller's named label",
            r#"fn spawn_it([[count]]: int, size: float) {
    print([[count]])
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it([[<|>count]]: 3, size: 1.0)
    spawn_it(count: "a")
}"#,
            "amount",
        ),
        (
            "parameter of the other overload from its named label",
            r#"fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it([[count]]: string) {
    print([[count]])
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it([[<|>count]]: "a")
}"#,
            "label",
        ),
        (
            "overloads from a call",
            r#"fn [[process]](x: int) -> int {
    return x * 2
}

fn [[process]](x: float) -> float {
    return x * 2.0
}

fn [[process]](x: string) -> string {
    return x + "_processed"
}

fn main() {
    let i = [[<|>process]](42)
    let f = [[process]](3.14)
    let s = [[process]]("test")
}"#,
            "convert",
        ),
        (
            "parameter of one overload",
            r#"fn calculate([[<|>x]]: int, y: int) -> int {
    return [[x]] + y
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
            "a",
        ),
        (
            "generic overloads",
            r#"fn [[<|>convert]]<T>(items: [T], mapper: fn(T) -> T) -> [T] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn [[convert]]<K, V>(dict: Map<K, V>, fallback: V) -> V {
    return fallback
}

fn main() {
    let numbers = [[convert]]([1, 2, 3], x => x * 2)
    let d: Map<int, int> = map_new()
    let single = [[convert]](d, 0)
}"#,
            "transform",
        ),
        (
            "inner shadowing local",
            r#"fn main() {
    let x = 1
    if true {
        let [[<|>x]] = 2
        print([[x]])
    }
    print(x)
}"#,
            "y",
        ),
        (
            "local shadowed in the same block",
            r#"fn main() {
    let [[<|>x]] = 1
    let x = [[x]] + 1
    print(x)
}"#,
            "y",
        ),
        (
            "local inside string interpolation",
            "fn main() {\n    let [[<|>n]] = 5\n    print(\"n = {[[n]]}\")\n}",
            "count",
        ),
        (
            "lambda parameter",
            "fn main() {\n    let f = ([[<|>a]]: int) => [[a]] + 1\n    print(f(1))\n}",
            "b",
        ),
        (
            "function named in a string literal",
            r#"fn [[<|>helper]](x: int) -> int {
    return x * 2
}

fn main() {
    print("helper")
    print([[helper]](1))
}"#,
            "helper2",
        ),
    ]);
}

#[test]
fn renames_fields_structs_and_patterns() {
    assert_renames(&[
        (
            "struct field",
            r#"struct Point {
    [[<|>x]]: float,
    y: float,
}

fn distance(p: Point) -> float {
    return (p.[[x]] * p.[[x]] + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { [[x]]: 3.0, y: 4.0 }
    print(p.[[x]])
}"#,
            "x_coord",
        ),
        (
            "shorthand variable in a struct literal",
            r#"struct Point {
    x: int
    y: int
}

fn main() {
    let [[x]] = 100
    let y = 200

    let p = Point { [[<|>x=>x: x_coord]], y }
    print([[x]])
    print(y)
}"#,
            "x_coord",
        ),
        (
            "second shorthand variable in a struct literal",
            r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let [[y]] = 200

    let p = Point { x, [[<|>y=>y: y_coord]] }
    print(x)
    print([[y]])
}"#,
            "y_coord",
        ),
        (
            "field used by a shorthand literal",
            r#"struct Point {
    [[<|>x]]: int
    y: int
}

fn main() {
    let x = 1
    let y = 2
    let p = Point { [[x=>px: x]], y }
    print(p.[[x]])
}"#,
            "px",
        ),
        (
            "shorthand binding in a struct pattern",
            r#"struct Point {
    x: int
    y: int
}

fn get_sum(p: Point) -> int {
    match p {
        Point { [[<|>x=>x: qx]], y } => [[x]] + y,
    }
}"#,
            "qx",
        ),
        (
            "binding in an enum pattern",
            r#"enum Option<T> {
    Some(T),
    None,
}

fn my_unwrap<T>(opt: Option<T>) -> T? {
    match opt {
        Option::Some([[<|>value]]) => [[value]],
        Option::None => null,
    }
}"#,
            "inner",
        ),
        (
            "struct in nullable annotations",
            r#"struct [[<|>Mob]] {
    hp: int
}

global CACHED: [[Mob]]? = null

fn take(m: [[Mob]]?) {
    print(m)
}

fn give() -> [[Mob]]? {
    let held: [[Mob]]? = null
    return held
}"#,
            "Beast",
        ),
        (
            "field through safe navigation",
            r#"struct Inner {
    [[<|>text]]: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o = Outer { inner: null }
    print(o.inner?.[[text]] ?? "none")
}"#,
            "label",
        ),
        (
            "field referenced by a sibling default",
            r#"struct Spell {
    [[<|>power]]: int,
    boosted: int = .[[power]],
}

fn main() {
    let s = Spell { [[power]]: 2 }
    print(s.boosted)
}"#,
            "strength",
        ),
        (
            "field at a field-reference argument",
            r#"struct Mob {
    [[<|>health]]: int,
}

fn main() {
    let m = Mob { [[health]]: 10 }
    print("{field_get(m, .[[health]])}")
}"#,
            "vitality",
        ),
        (
            "field no inline body names",
            r#"struct Mob {
    [[<|>hp]]: int,
    mana: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.mana"

fn main() {
    let m = Mob { [[hp]]: 4, mana: 2 }
    print(m.[[hp]] + peek(m))
}"#,
            "health",
        ),
    ]);
}

#[test]
fn renames_enums() {
    assert_renames(&[
        (
            "variant from declaration, pattern and construction",
            r#"enum Color {
    [[<|>Red]],
    Green,
}

fn describe(c: Color) -> int {
    match c {
        Color::[[<|>Red]] => 1,
        Color::Green => 2,
    }
}

fn main() {
    let c = Color::[[<|>Red]]
    print(describe(c))
}"#,
            "Crimson",
        ),
        (
            "enum name",
            r#"enum [[<|>Color]] {
    Red,
    Green,
}

fn describe(c: [[Color]]) -> int {
    match c {
        [[Color]]::Red => 1,
        [[Color]]::Green => 2,
    }
}

fn main() {
    let c = [[Color]]::Red
    print(describe(c))
}"#,
            "Hue",
        ),
        (
            "tuple variant",
            r#"enum Shape {
    [[<|>Circle]](float),
    Square(float),
}

fn area(s: Shape) -> float {
    match s {
        Shape::[[Circle]](r) => r * r * 3.14,
        Shape::Square(w) => w * w,
    }
}

fn main() {
    let s = Shape::[[Circle]](2.0)
    print(area(s))
}"#,
            "Round",
        ),
        (
            "same-named variant of another enum is untouched",
            r#"enum Color {
    [[<|>Red]],
    Green,
}

enum Paint {
    Red,
    Blue,
}

fn main() {
    let c = Color::[[Red]]
    let p = Paint::Red
    print(c)
    print(p)
}"#,
            "Crimson",
        ),
        (
            "record variant field from its declaration",
            r#"enum Item {
    Potion { [[<|>strength]]: int },
    Coin,
}

fn main() {
    let i = Item::Potion { [[strength]]: 3 }
    match i {
        Item::Potion { [[strength=>power: strength]] } => print(strength),
        Item::Coin => print(0),
    }
}"#,
            "power",
        ),
        (
            "record variant field from a literal",
            r#"enum Item {
    Potion { [[strength]]: int },
}

fn main() {
    let i = Item::Potion { [[<|>strength]]: 3 }
    print(i)
}"#,
            "power",
        ),
    ]);
}

#[test]
fn renames_globals_newtypes_decorators_and_macro_sites() {
    assert_renames(&[
        (
            "global from its declaration",
            r#"global [[<|>SCORE]]: int = 0

fn bump() {
    [[SCORE]] = [[SCORE]] + 1
}

fn main() {
    bump()
    print("s {[[SCORE]]}")
}"#,
            "POINTS",
        ),
        (
            "global from a use leaves a longer name alone",
            r#"global [[SCORE]]: int = 0
global SCOREBOARD: int = 1

fn bump() {
    [[<|>SCORE]] = [[SCORE]] + 1
    SCOREBOARD = 2
}"#,
            "POINTS",
        ),
        (
            "newtype from an annotation",
            "newtype [[UserId]](int)\n\nfn greet(id: [[<|>UserId]]) {\n    print(id)\n}\n\nfn main() {\n    let id = [[UserId]](42)\n    greet(id)\n}",
            "PlayerId",
        ),
        (
            "decorator from its declaration",
            r#"deco [[<|>cool]] {
    print(1)
}

@[[cool]]
fn thing() {
    print(1)
}

@[[cool]]
fn other() {
    print(2)
}"#,
            "fancy",
        ),
        (
            "decorator from an application",
            "deco [[cool]] {\n    print(1)\n}\n\n@[[<|>cool]]\nfn thing() {\n    print(1)\n}",
            "fancy",
        ),
        (
            "macro parameter",
            "macro double([[<|>x]]: expr) -> int { [[x]] + [[x]] }\n\nfn main() {\n    print(double(5))\n}",
            "v",
        ),
        (
            "local passed as a macro argument",
            "macro double(x: expr) -> int { x + x }\n\nfn main() {\n    let [[<|>n]] = 5\n    print(double([[n]]))\n}",
            "m",
        ),
        (
            "function called inside a macro body",
            r#"fn [[<|>helper]](x: int) -> int {
    return x + 1
}

macro twice(x: expr) -> int { [[helper]]([[helper]](x)) }

fn main() {
    print(twice(1))
    print([[helper]](2))
}"#,
            "inc",
        ),
        (
            "inline function name",
            "inline(lua) fn [[<|>probe]](x: int) -> int = \"return x + 1\"\n\nfn main() {\n    print([[probe]](1))\n}",
            "probe2",
        ),
        (
            "parameter spelled like another inline body's text",
            "inline(lua) fn probe(x: int) -> int = \"return x + 1\"\n\nfn twice([[<|>x]]: int) -> int {\n    return probe([[x]]) * 2\n}",
            "n",
        ),
    ]);
}

#[test]
fn refuses_unsafe_renames_and_invalid_names() {
    for (marked, new_name, why) in [
        (
            "fn main() {\n    let <|>foo = 42\n    print(foo)\n}",
            "fn",
            "cannot rename to `fn`: not an identifier",
        ),
        (
            "fn main() {\n    let <|>foo = 42\n    print(foo)\n}",
            "1 bad",
            "cannot rename to `1 bad`: not an identifier",
        ),
        (
            "macro <|>double(x: expr) -> int { x + x }\n\nfn main() {\n    print(double(5))\n}",
            "twice",
            "cannot rename a macro",
        ),
        (
            "macro bump() { ^count = ^count + 1 }\n\nfn main() {\n    let <|>count = 0\n    bump()\n    print(count)\n}",
            "total",
            ".rock:5 that rename cannot rewrite",
        ),
        (
            "global <|>SCORE: int = 0\n\nmacro bump() { _G.SCORE = _G.SCORE + 1 }\n\nfn main() {\n    bump()\n    print(SCORE)\n}",
            "POINTS",
            ".rock:6 that rename cannot rewrite",
        ),
        (
            "inline(js) fn probe(<|>x: int) -> int = \"return x + 1;\"\n\nfn main() {\n    print(probe(1))\n}",
            "y",
            "the opaque inline(js) body of `probe`",
        ),
        (
            "@nomangle\nfn <|>helper(x: int) -> int {\n    return x * 2\n}\n\ninline(lua) fn call_helper(x: int) -> int = \"return helper(x)\"\n\nfn main() {\n    print(call_helper(1))\n}",
            "helper2",
            "cannot rename `helper`: `@nomangle` keeps its emitted name",
        ),
        (
            "@nomangle\nfn helper(x: int) -> int {\n    return x * 2\n}\n\nfn main() {\n    print(<|>helper(1))\n}",
            "helper2",
            "`@nomangle` keeps its emitted name, which code outside Rock calls",
        ),
        (
            "struct Mob {\n    <|>hp: int,\n}\n\ninline(lua) fn peek(m: Mob) -> int = \"return m.hp\"\n\nfn main() {\n    let m = Mob { hp: 4 }\n    print(peek(m))\n}",
            "health",
            "the opaque inline(lua) body of `peek`",
        ),
        (
            "global <|>SCORE: int = 0\n\ninline(lua) fn peek() -> int = \"return SCORE\"\n\nfn main() {\n    print(peek())\n}",
            "POINTS",
            "the opaque inline(lua) body of `peek`",
        ),
        (
            "struct <|>Mob {\n    hp: int,\n}\n\ninline(lua) fn is_mob(v: any) -> bool = \"return v.__ty == 'Mob'\"\n\nfn main() {\n    print(is_mob(Mob { hp: 1 }))\n}",
            "Beast",
            "the opaque inline(lua) body of `is_mob`",
        ),
    ] {
        let (source, cursors) = unmark(marked);
        let [at] = cursors[..] else {
            panic!("expected one refusal cursor: {marked}");
        };
        assert_refused(&Project::single(&source), at, new_name, why);
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
