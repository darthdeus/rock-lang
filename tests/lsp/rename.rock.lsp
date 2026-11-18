[[tests]]
name = "rename_local_variable_simple"
kind = "rename"
position = [1, 8]
source = '''
fn main() {
    let foo = 42
    print(foo)
}'''
request = { transport = "cli", argv = ["rename", "$target", "bar"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["foo", "bar"]] } }]

[[tests]]
name = "rename_function_parameter"
kind = "rename"
position = [0, 13]
source = '''
fn calculate(x: int, y: int) -> int {
    return x + y * x
}

fn main() {
    let result = calculate(10, 20)
    print(result)
}'''
request = { transport = "cli", argv = ["rename", "$target", "value"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["x", "value"]] } }]

[[tests]]
name = "rename_struct_field"
kind = "rename"
position = [1, 4]
source = '''
struct Point {
    x: float,
    y: float,
}

fn distance(p: Point) -> float {
    return (p.x * p.x + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
    print(p.x)
}'''
request = { transport = "cli", argv = ["rename", "$target", "x_coord"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["x", "x_coord"]] } }]

[[tests]]
name = "rename_function_used_as_bare_value"
kind = "rename"
position = [9, 23]
source = '''
fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(double, 10)
    print(result)
}'''
request = { transport = "cli", argv = ["rename", "$target", "triple"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["double", "triple"]] } }]

[[tests]]
name = "rename_function_name"
kind = "rename"
position = [4, 3]
source = '''
struct Circle {
    radius: float,
}

fn area(c: Circle) -> float {
    return 3.14159 * c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 5.0 }
    print(area(c))
}'''
request = { transport = "cli", argv = ["rename", "$target", "calculate_area"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["area", "calculate_area"]] } }]

[[tests]]
name = "rename_to_keyword_error"
kind = "rename"
position = [1, 8]
source = { from_test = "rename_local_variable_simple" }
request = { transport = "cli", argv = ["rename", "$target", "fn"] }
checks = [{ op = "contains", path = ["error", "message"], value = "cannot rename to `fn`: not an identifier" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_to_several_tokens_error"
kind = "rename"
position = [1, 8]
source = { from_test = "rename_local_variable_simple" }
request = { transport = "cli", argv = ["rename", "$target", "1 bad"] }
checks = [{ op = "contains", path = ["error", "message"], value = "cannot rename to `1 bad`: not an identifier" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_generic_type_parameter"
kind = "rename"
position = [0, 12]
source = '''
fn identity<T>(value: T) -> T {
    return value
}

fn map_pair<T, U>(first: T, second: U) -> (T, U) {
    return (first, second)
}'''
request = { transport = "cli", argv = ["rename", "$target", "Element"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["fn identity<T>(value: T) -> T {", "fn identity<Element>(value: Element) -> Element {"]] } }]

[[tests]]
name = "rename_in_pattern_match"
kind = "rename"
position = [7, 21]
source = '''
enum Option<T> {
    Some(T),
    None,
}

fn my_unwrap<T>(opt: Option<T>) -> T? {
    match opt {
        Option::Some(value) => value,
        Option::None => null,
    }
}'''
request = { transport = "cli", argv = ["rename", "$target", "inner"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["value", "inner"]] } }]

[[tests]]
name = "rename_shorthand_var_x"
kind = "rename"
position = [10, 20]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Rename on shorthand variable 'x'
    let p = Point { x, y }
    print(x)
}'''
request = { transport = "cli", argv = ["rename", "$target", "x_coord"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    let x = 100", "    let x_coord = 100"], ["    let p = Point { x, y }", "    let p = Point { x: x_coord, y }"], ["    print(x)", "    print(x_coord)"]] } }]

[[tests]]
name = "rename_shorthand_var_y"
kind = "rename"
position = [10, 23]
source = { from_test = "rename_shorthand_var_x", replace = [["    // Rename on shorthand variable 'x'", "    // Rename on shorthand variable 'y'"], ["    print(x)", "    print(y)"]] }
request = { transport = "cli", argv = ["rename", "$target", "y_coord"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    let y = 200", "    let y_coord = 200"], ["    let p = Point { x, y }", "    let p = Point { x, y: y_coord }"], ["    print(y)", "    print(y_coord)"]] } }]

[[tests]]
name = "rename_enum_variant_from_decl"
kind = "rename"
position = [1, 4]
source = '''
enum Color {
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
}'''
request = { transport = "cli", argv = ["rename", "$target", "Crimson"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["Red", "Crimson"]] } }]

[[tests]]
name = "rename_enum_variant_from_use"
kind = "rename"
position = [6, 19]
source = '''
enum Color {
    Red,
    Green,
}

fn main() {
    let c = Color::Red
    print(c)
}'''
request = { transport = "cli", argv = ["rename", "$target", "Crimson"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["Red", "Crimson"]] } }]

[[tests]]
name = "rename_enum_variant_from_pattern"
kind = "rename"
position = [7, 15]
source = '''
enum Color {
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
}'''
request = { transport = "cli", argv = ["rename", "$target", "Crimson"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["Red", "Crimson"]] } }]

[[tests]]
name = "rename_enum_tuple_variant"
kind = "rename"
position = [1, 4]
source = '''
enum Shape {
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
}'''
request = { transport = "cli", argv = ["rename", "$target", "Round"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["Circle", "Round"]] } }]

[[tests]]
name = "rename_enum_variant_same_name_other_enum_untouched"
kind = "rename"
position = [1, 4]
source = '''
enum Color {
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
}'''
request = { transport = "cli", argv = ["rename", "$target", "Crimson"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = "enum Color {\n    Crimson,\n    Green,\n}\n\nenum Paint {\n    Red,\n    Blue,\n}\n\nfn main() {\n    let c = Color::Crimson\n    let p = Paint::Red\n    print(c)\n    print(p)\n}" }]

[[tests]]
name = "rename_enum_name"
kind = "rename"
position = [0, 5]
source = '''
enum Color {
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
}'''
request = { transport = "cli", argv = ["rename", "$target", "Hue"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["Color", "Hue"]] } }]

[[tests]]
name = "rename_generic_type_param_scoped"
kind = "rename"
position = [0, 9]
source = '''
fn first<T>(x: T, y: T) -> T {
    return x
}

fn second<T>(v: T) -> T {
    return v
}'''
request = { transport = "cli", argv = ["rename", "$target", "Item"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["fn first<T>(x: T, y: T) -> T {", "fn first<Item>(x: Item, y: Item) -> Item {"]] } }]

[[tests]]
name = "rename_pattern_shorthand_binding"
kind = "rename"
position = [7, 16]
source = '''
struct Point {
    x: int
    y: int
}

fn get_sum(p: Point) -> int {
    match p {
        Point { x, y } => x + y,
    }
}'''
request = { transport = "cli", argv = ["rename", "$target", "qx"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["        Point { x, y } => x + y,", "        Point { x: qx, y } => qx + y,"]] } }]

[[tests]]
name = "rename_struct_field_with_shorthand_literal"
kind = "rename"
position = [1, 4]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 1
    let y = 2
    let p = Point { x, y }
    print(p.x)
}'''
request = { transport = "cli", argv = ["rename", "$target", "px"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    x: int", "    px: int"], ["    let p = Point { x, y }", "    let p = Point { px: x, y }"], ["    print(p.x)", "    print(p.px)"]] } }]

[[tests]]
name = "rename_struct_in_nullable_annotation"
kind = "rename"
position = [0, 7]
source = '''
struct Mob {
    hp: int
}

global CACHED: Mob? = null

fn take(m: Mob?) {
    print(m)
}

fn give() -> Mob? {
    let held: Mob? = null
    return held
}'''
request = { transport = "cli", argv = ["rename", "$target", "Beast"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["Mob", "Beast"]] } }]

[[tests]]
name = "rename_field_through_safe_nav"
kind = "rename"
position = [1, 4]
source = '''
struct Inner {
    text: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o = Outer { inner: null }
    print(o.inner?.text ?? "none")
}'''
request = { transport = "cli", argv = ["rename", "$target", "label"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["text", "label"]] } }]

[[tests]]
name = "rename_fn_at_safe_nav_call_site"
kind = "rename"
position = [4, 3]
source = '''
struct Mob {
    hp: int
}

fn mob_heal(m: Mob) -> int {
    return m.hp
}

fn main() {
    let m: Mob? = Mob { hp: 1 }
    print(m?.mob_heal() ?? 0)
}'''
request = { transport = "cli", argv = ["rename", "$target", "mob_restore"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["mob_heal", "mob_restore"]] } }]

[[tests]]
name = "rename_field_with_sibling_ref_default"
kind = "rename"
position = [1, 4]
source = '''
struct Spell {
    power: int,
    boosted: int = .power,
}

fn main() {
    let s = Spell { power: 2 }
    print(s.boosted)
}'''
request = { transport = "cli", argv = ["rename", "$target", "strength"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["power", "strength"]] } }]

[[tests]]
name = "rename_param_updates_named_arg_call_sites"
kind = "rename"
position = [0, 12]
source = '''
fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(4, size: 2.0)
}'''
request = { transport = "cli", argv = ["rename", "$target", "amount"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["count", "amount"]] } }]

[[tests]]
name = "rename_field_at_fieldref_key_path_arg"
kind = "rename"
position = [1, 4]
source = '''
struct Mob {
    health: int,
}

fn main() {
    let m = Mob { health: 10 }
    print("{field_get(m, .health)}")
}'''
request = { transport = "cli", argv = ["rename", "$target", "vitality"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["health", "vitality"]] } }]

[[tests]]
name = "rename_global_from_decl"
kind = "rename"
position = [0, 7]
source = '''
global SCORE: int = 0

fn bump() {
    SCORE = SCORE + 1
}

fn main() {
    bump()
    print("s {SCORE}")
}'''
request = { transport = "cli", argv = ["rename", "$target", "POINTS"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["SCORE", "POINTS"]] } }]

[[tests]]
name = "rename_global_from_use_site"
kind = "rename"
position = [4, 4]
source = '''
global SCORE: int = 0
global SCOREBOARD: int = 1

fn bump() {
    SCORE = SCORE + 1
    SCOREBOARD = 2
}'''
request = { transport = "cli", argv = ["rename", "$target", "POINTS"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["global SCORE: int = 0", "global POINTS: int = 0"], ["    SCORE = SCORE + 1", "    POINTS = POINTS + 1"]] } }]

[[tests]]
name = "rename_newtype_from_decl"
kind = "rename"
position = [0, 8]
source = '''
newtype UserId(int)

fn greet(id: UserId) {
    print(id)
}

fn main() {
    let id = UserId(42)
    greet(id)
}'''
request = { transport = "cli", argv = ["rename", "$target", "PlayerId"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["UserId", "PlayerId"]] } }]

[[tests]]
name = "rename_newtype_from_annotation"
kind = "rename"
position = [2, 13]
source = { from_test = "rename_newtype_from_decl" }
request = { transport = "cli", argv = ["rename", "$target", "PlayerId"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["UserId", "PlayerId"]] } }]

[[tests]]
name = "rename_record_variant_field_from_decl"
kind = "rename"
position = [1, 13]
source = '''
enum Item {
    Potion { strength: int },
    Coin,
}

fn main() {
    let i = Item::Potion { strength: 3 }
    match i {
        Item::Potion { strength } => print(strength),
        Item::Coin => print(0),
    }
}'''
request = { transport = "cli", argv = ["rename", "$target", "power"] }

[[tests.checks]]
op = "empty"
path = ["error"]
missing_is_empty = true
null_is_empty = true

[[tests.checks]]
op = "applied_text_equals"
path = ["result", "changes"]
file = "$primary"
expected = { from_fixture = true, replace = [["    Potion { strength: int },", "    Potion { power: int },"], ["    let i = Item::Potion { strength: 3 }", "    let i = Item::Potion { power: 3 }"], ["        Item::Potion { strength } => print(strength),", "        Item::Potion { power: strength } => print(strength),"]] }

[[tests]]
name = "rename_record_variant_field_from_literal"
kind = "rename"
position = [5, 27]
source = '''
enum Item {
    Potion { strength: int },
}

fn main() {
    let i = Item::Potion { strength: 3 }
    print(i)
}'''
request = { transport = "cli", argv = ["rename", "$target", "power"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["strength", "power"]] } }]

[[tests]]
name = "rename_decorator_from_decl"
kind = "rename"
position = [0, 5]
source = '''
deco cool {
    print(1)
}

@cool
fn thing() {
    print(1)
}

@cool
fn other() {
    print(2)
}'''
request = { transport = "cli", argv = ["rename", "$target", "fancy"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["cool", "fancy"]] } }]

[[tests]]
name = "rename_decorator_from_application"
kind = "rename"
position = [4, 1]
source = '''
deco cool {
    print(1)
}

@cool
fn thing() {
    print(1)
}'''
request = { transport = "cli", argv = ["rename", "$target", "fancy"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["cool", "fancy"]] } }]
