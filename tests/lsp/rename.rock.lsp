TEST: rename_local_variable_simple
TYPE: rename
---
fn main() {
    let «start»foo«end» = 42
    print(foo)
}
---
NEW_NAME: bar
EXPECTED:
REPLACE: ["foo", "bar"]
===

TEST: rename_function_parameter
TYPE: rename
---
fn calculate(«start»x«end»: int, y: int) -> int {
    return x + y * x
}

fn main() {
    let result = calculate(10, 20)
    print(result)
}
---
NEW_NAME: value
EXPECTED:
REPLACE: ["x", "value"]
===

TEST: rename_struct_field
TYPE: rename
---
struct Point {
    «start»x«end»: float,
    y: float,
}

fn distance(p: Point) -> float {
    return (p.x * p.x + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
    print(p.x)
}
---
NEW_NAME: x_coord
EXPECTED:
REPLACE: ["x", "x_coord"]
===

TEST: rename_function_used_as_bare_value
TYPE: rename
---
fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(«start»double«end», 10)
    print(result)
}
---
NEW_NAME: triple
EXPECTED:
REPLACE: ["double", "triple"]
===

TEST: rename_function_name
TYPE: rename
---
struct Circle {
    radius: float,
}

fn «start»area«end»(c: Circle) -> float {
    return 3.14159 * c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 5.0 }
    print(area(c))
}
---
NEW_NAME: calculate_area
EXPECTED:
REPLACE: ["area", "calculate_area"]
===

TEST: rename_to_keyword_error
TYPE: rename
SOURCE: rename_local_variable_simple
POSITION: line=1, character=8
NEW_NAME: fn
EXPECTED:
REFUSED: cannot rename to `fn`: not an identifier
===

TEST: rename_to_several_tokens_error
TYPE: rename
SOURCE: rename_local_variable_simple
POSITION: line=1, character=8
NEW_NAME: 1 bad
EXPECTED:
REFUSED: cannot rename to `1 bad`: not an identifier
===

TEST: rename_generic_type_parameter
TYPE: rename
---
fn identity<«start»T«end»>(value: T) -> T {
    return value
}

fn map_pair<T, U>(first: T, second: U) -> (T, U) {
    return (first, second)
}
---
NEW_NAME: Element
EXPECTED:
REPLACE: ["fn identity<T>(value: T) -> T {", "fn identity<Element>(value: Element) -> Element {"]
===

TEST: rename_in_pattern_match
TYPE: rename
---
enum Option<T> {
    Some(T),
    None,
}

fn my_unwrap<T>(opt: Option<T>) -> T? {
    match opt {
        Option::Some(«start»value«end») => value,
        Option::None => null,
    }
}
---
NEW_NAME: inner
EXPECTED:
REPLACE: ["value", "inner"]
===

TEST: rename_shorthand_var_x
TYPE: rename
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Rename on shorthand variable 'x'
    let p = Point { «cursor»x, y }
    print(x)
}
---
NEW_NAME: x_coord
EXPECTED:
REPLACE: ["    let x = 100", "    let x_coord = 100"]
REPLACE: ["    let p = Point { x, y }", "    let p = Point { x: x_coord, y }"]
REPLACE: ["    print(x)", "    print(x_coord)"]
===

TEST: rename_shorthand_var_y
TYPE: rename
SOURCE: rename_shorthand_var_x
REPLACE: ["    // Rename on shorthand variable 'x'", "    // Rename on shorthand variable 'y'"]
REPLACE: ["    print(x)", "    print(y)"]
POSITION: line=10, character=23
NEW_NAME: y_coord
EXPECTED:
REPLACE: ["    let y = 200", "    let y_coord = 200"]
REPLACE: ["    let p = Point { x, y }", "    let p = Point { x, y: y_coord }"]
REPLACE: ["    print(y)", "    print(y_coord)"]
===

TEST: rename_enum_variant_from_decl
TYPE: rename
---
enum Color {
    «start»Red«end»,
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
}
---
NEW_NAME: Crimson
EXPECTED:
REPLACE: ["Red", "Crimson"]
===

TEST: rename_enum_variant_from_use
TYPE: rename
---
enum Color {
    Red,
    Green,
}

fn main() {
    let c = Color::«cursor»Red
    print(c)
}
---
NEW_NAME: Crimson
EXPECTED:
REPLACE: ["Red", "Crimson"]
===

TEST: rename_enum_variant_from_pattern
TYPE: rename
---
enum Color {
    Red,
    Green,
}

fn describe(c: Color) -> int {
    match c {
        Color::«cursor»Red => 1,
        Color::Green => 2,
    }
}

fn main() {
    print(describe(Color::Red))
}
---
NEW_NAME: Crimson
EXPECTED:
REPLACE: ["Red", "Crimson"]
===

TEST: rename_enum_tuple_variant
TYPE: rename
---
enum Shape {
    «start»Circle«end»(float),
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
}
---
NEW_NAME: Round
EXPECTED:
REPLACE: ["Circle", "Round"]
===

TEST: rename_enum_variant_same_name_other_enum_untouched
TYPE: rename
---
enum Color {
    «start»Red«end»,
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
}
---
NEW_NAME: Crimson
EXPECTED:
enum Color {
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
}
===

TEST: rename_enum_name
TYPE: rename
---
enum «start»Color«end» {
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
}
---
NEW_NAME: Hue
EXPECTED:
REPLACE: ["Color", "Hue"]
===

TEST: rename_generic_type_param_scoped
TYPE: rename
---
fn first<«start»T«end»>(x: T, y: T) -> T {
    return x
}

fn second<T>(v: T) -> T {
    return v
}
---
NEW_NAME: Item
EXPECTED:
REPLACE: ["fn first<T>(x: T, y: T) -> T {", "fn first<Item>(x: Item, y: Item) -> Item {"]
===

TEST: rename_pattern_shorthand_binding
TYPE: rename
---
struct Point {
    x: int
    y: int
}

fn get_sum(p: Point) -> int {
    match p {
        Point { «start»x«end», y } => x + y,
    }
}
---
NEW_NAME: qx
EXPECTED:
REPLACE: ["        Point { x, y } => x + y,", "        Point { x: qx, y } => qx + y,"]
===

TEST: rename_struct_field_with_shorthand_literal
TYPE: rename
---
struct Point {
    «start»x«end»: int
    y: int
}

fn main() {
    let x = 1
    let y = 2
    let p = Point { x, y }
    print(p.x)
}
---
NEW_NAME: px
EXPECTED:
REPLACE: ["    x: int", "    px: int"]
REPLACE: ["    let p = Point { x, y }", "    let p = Point { px: x, y }"]
REPLACE: ["    print(p.x)", "    print(p.px)"]
===

TEST: rename_struct_in_nullable_annotation
TYPE: rename
---
struct «start»Mob«end» {
    hp: int
}

global CACHED: Mob? = null

fn take(m: Mob?) {
    print(m)
}

fn give() -> Mob? {
    let held: Mob? = null
    return held
}
---
NEW_NAME: Beast
EXPECTED:
REPLACE: ["Mob", "Beast"]
===

TEST: rename_field_through_safe_nav
TYPE: rename
---
struct Inner {
    «start»text«end»: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o = Outer { inner: null }
    print(o.inner?.text ?? "none")
}
---
NEW_NAME: label
EXPECTED:
REPLACE: ["text", "label"]
===

TEST: rename_fn_at_safe_nav_call_site
TYPE: rename
---
struct Mob {
    hp: int
}

fn «start»mob_heal«end»(m: Mob) -> int {
    return m.hp
}

fn main() {
    let m: Mob? = Mob { hp: 1 }
    print(m?.mob_heal() ?? 0)
}
---
NEW_NAME: mob_restore
EXPECTED:
REPLACE: ["mob_heal", "mob_restore"]
===

TEST: rename_field_with_sibling_ref_default
TYPE: rename
---
struct Spell {
    «start»power«end»: int,
    boosted: int = .power,
}

fn main() {
    let s = Spell { power: 2 }
    print(s.boosted)
}
---
NEW_NAME: strength
EXPECTED:
REPLACE: ["power", "strength"]
===

TEST: rename_param_updates_named_arg_call_sites
TYPE: rename
---
fn spawn_it(«start»count«end»: int, size: float) {
    print(count)
    print(size)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(4, size: 2.0)
}
---
NEW_NAME: amount
EXPECTED:
REPLACE: ["count", "amount"]
===

TEST: rename_field_at_fieldref_key_path_arg
TYPE: rename
---
struct Mob {
    «start»health«end»: int,
}

fn main() {
    let m = Mob { health: 10 }
    print("{field_get(m, .health)}")
}
---
NEW_NAME: vitality
EXPECTED:
REPLACE: ["health", "vitality"]
===

TEST: rename_global_from_decl
TYPE: rename
---
global «start»SCORE«end»: int = 0

fn bump() {
    SCORE = SCORE + 1
}

fn main() {
    bump()
    print("s {SCORE}")
}
---
NEW_NAME: POINTS
EXPECTED:
REPLACE: ["SCORE", "POINTS"]
===

TEST: rename_global_from_use_site
TYPE: rename
---
global SCORE: int = 0
global SCOREBOARD: int = 1

fn bump() {
    «cursor»SCORE = SCORE + 1
    SCOREBOARD = 2
}
---
NEW_NAME: POINTS
EXPECTED:
REPLACE: ["global SCORE: int = 0", "global POINTS: int = 0"]
REPLACE: ["    SCORE = SCORE + 1", "    POINTS = POINTS + 1"]
===

TEST: rename_newtype_from_decl
TYPE: rename
---
newtype «start»UserId«end»(int)

fn greet(id: UserId) {
    print(id)
}

fn main() {
    let id = UserId(42)
    greet(id)
}
---
NEW_NAME: PlayerId
EXPECTED:
REPLACE: ["UserId", "PlayerId"]
===

TEST: rename_newtype_from_annotation
TYPE: rename
SOURCE: rename_newtype_from_decl
POSITION: line=2, character=13
NEW_NAME: PlayerId
EXPECTED:
REPLACE: ["UserId", "PlayerId"]
===

TEST: rename_record_variant_field_from_decl
TYPE: rename
---
enum Item {
    Potion { «start»strength«end»: int },
    Coin,
}

fn main() {
    let i = Item::Potion { strength: 3 }
    match i {
        Item::Potion { strength } => print(strength),
        Item::Coin => print(0),
    }
}
---
NEW_NAME: power
EXPECTED:
REPLACE: ["    Potion { strength: int },", "    Potion { power: int },"]
REPLACE: ["    let i = Item::Potion { strength: 3 }", "    let i = Item::Potion { power: 3 }"]
REPLACE: ["        Item::Potion { strength } => print(strength),", "        Item::Potion { power: strength } => print(strength),"]
===

TEST: rename_record_variant_field_from_literal
TYPE: rename
---
enum Item {
    Potion { strength: int },
}

fn main() {
    let i = Item::Potion { «cursor»strength: 3 }
    print(i)
}
---
NEW_NAME: power
EXPECTED:
REPLACE: ["strength", "power"]
===

TEST: rename_decorator_from_decl
TYPE: rename
---
deco «start»cool«end» {
    print(1)
}

@cool
fn thing() {
    print(1)
}

@cool
fn other() {
    print(2)
}
---
NEW_NAME: fancy
EXPECTED:
REPLACE: ["cool", "fancy"]
===

TEST: rename_decorator_from_application
TYPE: rename
---
deco cool {
    print(1)
}

@«cursor»cool
fn thing() {
    print(1)
}
---
NEW_NAME: fancy
EXPECTED:
REPLACE: ["cool", "fancy"]
===
