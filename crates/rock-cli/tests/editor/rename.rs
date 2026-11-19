use super::*;

const RENAME_LOCAL_VARIABLE_SIMPLE: &str = r#"fn main() {
    let foo = 42
    print(foo)
}"#;

#[test]
fn rename_local_variable_simple() {
    let source = RENAME_LOCAL_VARIABLE_SIMPLE;
    let project = Project::single("rename_local_variable_simple", source);
    let reply = project.cli("rename", Some((1, 8)), &["bar"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("foo", "bar").trim_end()
    );
}

#[test]
fn rename_function_parameter() {
    let source = r#"fn calculate(x: int, y: int) -> int {
    return x + y * x
}

fn main() {
    let result = calculate(10, 20)
    print(result)
}"#;
    let project = Project::single("rename_function_parameter", source);
    let reply = project.cli("rename", Some((0, 13)), &["value"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("x", "value").trim_end()
    );
}

#[test]
fn rename_struct_field() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

fn distance(p: Point) -> float {
    return (p.x * p.x + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
    print(p.x)
}"#;
    let project = Project::single("rename_struct_field", source);
    let reply = project.cli("rename", Some((1, 4)), &["x_coord"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("x", "x_coord").trim_end()
    );
}

#[test]
fn rename_function_used_as_bare_value() {
    let source = r#"fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(double, 10)
    print(result)
}"#;
    let project = Project::single("rename_function_used_as_bare_value", source);
    let reply = project.cli("rename", Some((9, 23)), &["triple"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("double", "triple").trim_end()
    );
}

#[test]
fn rename_function_name() {
    let source = r#"struct Circle {
    radius: float,
}

fn area(c: Circle) -> float {
    return 3.14159 * c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 5.0 }
    print(area(c))
}"#;
    let project = Project::single("rename_function_name", source);
    let reply = project.cli("rename", Some((4, 3)), &["calculate_area"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("area", "calculate_area").trim_end()
    );
}

#[test]
fn rename_to_keyword_error() {
    let source = RENAME_LOCAL_VARIABLE_SIMPLE;
    let project = Project::single("rename_to_keyword_error", source);
    let reply = project.cli("rename", Some((1, 8)), &["fn"]);
    contains_text(
        string(&reply["error"]),
        &["cannot rename to `fn`: not an identifier"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_to_several_tokens_error() {
    let source = RENAME_LOCAL_VARIABLE_SIMPLE;
    let project = Project::single("rename_to_several_tokens_error", source);
    let reply = project.cli("rename", Some((1, 8)), &["1 bad"]);
    contains_text(
        string(&reply["error"]),
        &["cannot rename to `1 bad`: not an identifier"],
    );
    assert_no_rename_changes(&reply);
}

#[test]
fn rename_generic_type_parameter() {
    let source = r#"fn identity<T>(value: T) -> T {
    return value
}

fn map_pair<T, U>(first: T, second: U) -> (T, U) {
    return (first, second)
}"#;
    let project = Project::single("rename_generic_type_parameter", source);
    let reply = project.cli("rename", Some((0, 12)), &["Element"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace(
                "fn identity<T>(value: T) -> T {",
                "fn identity<Element>(value: Element) -> Element {"
            )
            .trim_end()
    );
}

#[test]
fn rename_in_pattern_match() {
    let source = r#"enum Option<T> {
    Some(T),
    None,
}

fn my_unwrap<T>(opt: Option<T>) -> T? {
    match opt {
        Option::Some(value) => value,
        Option::None => null,
    }
}"#;
    let project = Project::single("rename_in_pattern_match", source);
    let reply = project.cli("rename", Some((7, 21)), &["inner"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("value", "inner").trim_end()
    );
}

const RENAME_SHORTHAND_VAR_X: &str = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Rename on shorthand variable 'x'
    let p = Point { x, y }
    print(x)
}"#;

#[test]
fn rename_shorthand_var_x() {
    let source = RENAME_SHORTHAND_VAR_X;
    let project = Project::single("rename_shorthand_var_x", source);
    let reply = project.cli("rename", Some((10, 20)), &["x_coord"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace("    let x = 100", "    let x_coord = 100")
            .replace(
                "    let p = Point { x, y }",
                "    let p = Point { x: x_coord, y }"
            )
            .replace("    print(x)", "    print(x_coord)")
            .trim_end()
    );
}

#[test]
fn rename_shorthand_var_y() {
    let source = &RENAME_SHORTHAND_VAR_X
        .replace(
            "    // Rename on shorthand variable 'x'",
            "    // Rename on shorthand variable 'y'",
        )
        .replace("    print(x)", "    print(y)");
    let project = Project::single("rename_shorthand_var_y", source);
    let reply = project.cli("rename", Some((10, 23)), &["y_coord"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace("    let y = 200", "    let y_coord = 200")
            .replace(
                "    let p = Point { x, y }",
                "    let p = Point { x, y: y_coord }"
            )
            .replace("    print(y)", "    print(y_coord)")
            .trim_end()
    );
}

#[test]
fn rename_enum_variant_from_decl() {
    let source = r#"enum Color {
    Red,
    Green,
    Blue,
}

fn describe(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    }
}

fn main() {
    let c = Color::Red
    print(describe(c))
}"#;
    let project = Project::single("rename_enum_variant_from_decl", source);
    let reply = project.cli("rename", Some((1, 4)), &["Crimson"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("Red", "Crimson").trim_end()
    );
}

#[test]
fn rename_enum_variant_from_use() {
    let source = r#"enum Color {
    Red,
    Green,
}

fn main() {
    let c = Color::Red
    print(c)
}"#;
    let project = Project::single("rename_enum_variant_from_use", source);
    let reply = project.cli("rename", Some((6, 19)), &["Crimson"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("Red", "Crimson").trim_end()
    );
}

#[test]
fn rename_enum_variant_from_pattern() {
    let source = r#"enum Color {
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
    print(describe(Color::Red))
}"#;
    let project = Project::single("rename_enum_variant_from_pattern", source);
    let reply = project.cli("rename", Some((7, 15)), &["Crimson"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("Red", "Crimson").trim_end()
    );
}

#[test]
fn rename_enum_tuple_variant() {
    let source = r#"enum Shape {
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
}"#;
    let project = Project::single("rename_enum_tuple_variant", source);
    let reply = project.cli("rename", Some((1, 4)), &["Round"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("Circle", "Round").trim_end()
    );
}

#[test]
fn rename_enum_variant_same_name_other_enum_untouched() {
    let source = r#"enum Color {
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
}"#;
    let project = Project::single("rename_enum_variant_same_name_other_enum_untouched", source);
    let reply = project.cli("rename", Some((1, 4)), &["Crimson"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        (r#"enum Color {
    Crimson,
    Green,
}

enum Paint {
    Red,
    Blue,
}

fn main() {
    let c = Color::Crimson
    let p = Paint::Red
    print(c)
    print(p)
}"#)
        .trim_end()
    );
}

#[test]
fn rename_enum_name() {
    let source = r#"enum Color {
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
    let project = Project::single("rename_enum_name", source);
    let reply = project.cli("rename", Some((0, 5)), &["Hue"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("Color", "Hue").trim_end()
    );
}

#[test]
fn rename_generic_type_param_scoped() {
    let source = r#"fn first<T>(x: T, y: T) -> T {
    return x
}

fn second<T>(v: T) -> T {
    return v
}"#;
    let project = Project::single("rename_generic_type_param_scoped", source);
    let reply = project.cli("rename", Some((0, 9)), &["Item"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace(
                "fn first<T>(x: T, y: T) -> T {",
                "fn first<Item>(x: Item, y: Item) -> Item {"
            )
            .trim_end()
    );
}

#[test]
fn rename_pattern_shorthand_binding() {
    let source = r#"struct Point {
    x: int
    y: int
}

fn get_sum(p: Point) -> int {
    match p {
        Point { x, y } => x + y,
    }
}"#;
    let project = Project::single("rename_pattern_shorthand_binding", source);
    let reply = project.cli("rename", Some((7, 16)), &["qx"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace(
                "        Point { x, y } => x + y,",
                "        Point { x: qx, y } => qx + y,"
            )
            .trim_end()
    );
}

#[test]
fn rename_struct_field_with_shorthand_literal() {
    let source = r#"struct Point {
    x: int
    y: int
}

fn main() {
    let x = 1
    let y = 2
    let p = Point { x, y }
    print(p.x)
}"#;
    let project = Project::single("rename_struct_field_with_shorthand_literal", source);
    let reply = project.cli("rename", Some((1, 4)), &["px"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace("    x: int", "    px: int")
            .replace(
                "    let p = Point { x, y }",
                "    let p = Point { px: x, y }"
            )
            .replace("    print(p.x)", "    print(p.px)")
            .trim_end()
    );
}

#[test]
fn rename_struct_in_nullable_annotation() {
    let source = r#"struct Mob {
    hp: int
}

global CACHED: Mob? = null

fn take(m: Mob?) {
    print(m)
}

fn give() -> Mob? {
    let held: Mob? = null
    return held
}"#;
    let project = Project::single("rename_struct_in_nullable_annotation", source);
    let reply = project.cli("rename", Some((0, 7)), &["Beast"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("Mob", "Beast").trim_end()
    );
}

#[test]
fn rename_field_through_safe_nav() {
    let source = r#"struct Inner {
    text: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o = Outer { inner: null }
    print(o.inner?.text ?? "none")
}"#;
    let project = Project::single("rename_field_through_safe_nav", source);
    let reply = project.cli("rename", Some((1, 4)), &["label"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("text", "label").trim_end()
    );
}

#[test]
fn rename_fn_at_safe_nav_call_site() {
    let source = r#"struct Mob {
    hp: int
}

fn mob_heal(m: Mob) -> int {
    return m.hp
}

fn main() {
    let m: Mob? = Mob { hp: 1 }
    print(m?.mob_heal() ?? 0)
}"#;
    let project = Project::single("rename_fn_at_safe_nav_call_site", source);
    let reply = project.cli("rename", Some((4, 3)), &["mob_restore"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("mob_heal", "mob_restore").trim_end()
    );
}

#[test]
fn rename_field_with_sibling_ref_default() {
    let source = r#"struct Spell {
    power: int,
    boosted: int = .power,
}

fn main() {
    let s = Spell { power: 2 }
    print(s.boosted)
}"#;
    let project = Project::single("rename_field_with_sibling_ref_default", source);
    let reply = project.cli("rename", Some((1, 4)), &["strength"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("power", "strength").trim_end()
    );
}

#[test]
fn rename_param_updates_named_arg_call_sites() {
    let source = r#"fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(4, size: 2.0)
}"#;
    let project = Project::single("rename_param_updates_named_arg_call_sites", source);
    let reply = project.cli("rename", Some((0, 12)), &["amount"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("count", "amount").trim_end()
    );
}

#[test]
fn rename_field_at_fieldref_key_path_arg() {
    let source = r#"struct Mob {
    health: int,
}

fn main() {
    let m = Mob { health: 10 }
    print("{field_get(m, .health)}")
}"#;
    let project = Project::single("rename_field_at_fieldref_key_path_arg", source);
    let reply = project.cli("rename", Some((1, 4)), &["vitality"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("health", "vitality").trim_end()
    );
}

#[test]
fn rename_global_from_decl() {
    let source = r#"global SCORE: int = 0

fn bump() {
    SCORE = SCORE + 1
}

fn main() {
    bump()
    print("s {SCORE}")
}"#;
    let project = Project::single("rename_global_from_decl", source);
    let reply = project.cli("rename", Some((0, 7)), &["POINTS"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("SCORE", "POINTS").trim_end()
    );
}

#[test]
fn rename_global_from_use_site() {
    let source = r#"global SCORE: int = 0
global SCOREBOARD: int = 1

fn bump() {
    SCORE = SCORE + 1
    SCOREBOARD = 2
}"#;
    let project = Project::single("rename_global_from_use_site", source);
    let reply = project.cli("rename", Some((4, 4)), &["POINTS"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace("global SCORE: int = 0", "global POINTS: int = 0")
            .replace("    SCORE = SCORE + 1", "    POINTS = POINTS + 1")
            .trim_end()
    );
}

const RENAME_NEWTYPE_FROM_DECL: &str = r#"newtype UserId(int)

fn greet(id: UserId) {
    print(id)
}

fn main() {
    let id = UserId(42)
    greet(id)
}"#;

#[test]
fn rename_newtype_from_decl() {
    let source = RENAME_NEWTYPE_FROM_DECL;
    let project = Project::single("rename_newtype_from_decl", source);
    let reply = project.cli("rename", Some((0, 8)), &["PlayerId"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("UserId", "PlayerId").trim_end()
    );
}

#[test]
fn rename_newtype_from_annotation() {
    let source = RENAME_NEWTYPE_FROM_DECL;
    let project = Project::single("rename_newtype_from_annotation", source);
    let reply = project.cli("rename", Some((2, 13)), &["PlayerId"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("UserId", "PlayerId").trim_end()
    );
}

#[test]
fn rename_record_variant_field_from_decl() {
    let source = r#"enum Item {
    Potion { strength: int },
    Coin,
}

fn main() {
    let i = Item::Potion { strength: 3 }
    match i {
        Item::Potion { strength } => print(strength),
        Item::Coin => print(0),
    }
}"#;
    let project = Project::single("rename_record_variant_field_from_decl", source);
    let reply = project.cli("rename", Some((1, 13)), &["power"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source
            .replace(
                "    Potion { strength: int },",
                "    Potion { power: int },"
            )
            .replace(
                "    let i = Item::Potion { strength: 3 }",
                "    let i = Item::Potion { power: 3 }"
            )
            .replace(
                "        Item::Potion { strength } => print(strength),",
                "        Item::Potion { power: strength } => print(strength),"
            )
            .trim_end()
    );
}

#[test]
fn rename_record_variant_field_from_literal() {
    let source = r#"enum Item {
    Potion { strength: int },
}

fn main() {
    let i = Item::Potion { strength: 3 }
    print(i)
}"#;
    let project = Project::single("rename_record_variant_field_from_literal", source);
    let reply = project.cli("rename", Some((5, 27)), &["power"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("strength", "power").trim_end()
    );
}

#[test]
fn rename_decorator_from_decl() {
    let source = r#"deco cool {
    print(1)
}

@cool
fn thing() {
    print(1)
}

@cool
fn other() {
    print(2)
}"#;
    let project = Project::single("rename_decorator_from_decl", source);
    let reply = project.cli("rename", Some((0, 5)), &["fancy"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("cool", "fancy").trim_end()
    );
}

#[test]
fn rename_decorator_from_application() {
    let source = r#"deco cool {
    print(1)
}

@cool
fn thing() {
    print(1)
}"#;
    let project = Project::single("rename_decorator_from_application", source);
    let reply = project.cli("rename", Some((4, 1)), &["fancy"]);
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("cool", "fancy").trim_end()
    );
}

#[test]
fn rename_reference_after_non_bmp_text() {
    let source = "fn main() {\n    let foo = 42\n    print(\"\u{1f319}\") print(foo)\n}";
    let project = Project::single("rename_reference_after_non_bmp_text", source);
    let reply = project.cli("rename", Some((1, 8)), &["bar"]);
    assert_eq!(
        project.renamed(&reply, None).trim_end(),
        source.replace("foo", "bar").trim_end()
    );
}
