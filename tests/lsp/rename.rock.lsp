TEST: rename_local_variable_simple
TYPE: rename
---
fn main() {
    let «start»foo«end» = 42
    print(foo)
}
---
OPERATION: rename
NEW_NAME: bar
---
EXPECTED:
fn main() {
    let bar = 42
    print(bar)
}
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
OPERATION: rename
NEW_NAME: value
---
EXPECTED:
fn calculate(value: int, y: int) -> int {
    return value + y * value
}

fn main() {
    let result = calculate(10, 20)
    print(result)
}
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
OPERATION: rename
NEW_NAME: x_coord
---
EXPECTED:
struct Point {
    x_coord: float,
    y: float,
}

fn distance(p: Point) -> float {
    return (p.x_coord * p.x_coord + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { x_coord: 3.0, y: 4.0 }
    print(p.x_coord)
}
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
OPERATION: rename
NEW_NAME: triple
---
EXPECTED:
fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn triple(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(triple, 10)
    print(result)
}
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
OPERATION: rename
NEW_NAME: calculate_area
---
EXPECTED:
struct Circle {
    radius: float,
}

fn calculate_area(c: Circle) -> float {
    return 3.14159 * c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 5.0 }
    print(calculate_area(c))
}
===

TEST: rename_to_keyword_error
TYPE: rename
---
let «start»foo«end» = 42
---
OPERATION: rename
NEW_NAME: fn
---
EXPECTED:
let foo = 42
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
OPERATION: rename
NEW_NAME: Element
---
EXPECTED:
fn identity<Element>(value: Element) -> Element {
    return value
}

fn map_pair<T, U>(first: T, second: U) -> (T, U) {
    return (first, second)
}
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
OPERATION: rename
NEW_NAME: inner
---
EXPECTED:
enum Option<T> {
    Some(T),
    None,
}

fn my_unwrap<T>(opt: Option<T>) -> T? {
    match opt {
        Option::Some(inner) => inner,
        Option::None => null,
    }
}
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
OPERATION: rename
NEW_NAME: x_coord
---
EXPECTED:
struct Point {
    x: int
    y: int
}

fn main() {
    let x_coord = 100
    let y = 200

    // Rename on shorthand variable 'x'
    let p = Point { x: x_coord, y }
    print(x_coord)
}
===

TEST: rename_shorthand_var_y
TYPE: rename
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Rename on shorthand variable 'y'
    let p = Point { x, «cursor»y }
    print(y)
}
---
OPERATION: rename
NEW_NAME: y_coord
---
EXPECTED:
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y_coord = 200

    // Rename on shorthand variable 'y'
    let p = Point { x, y: y_coord }
    print(y_coord)
}
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
OPERATION: rename
NEW_NAME: Crimson
---
EXPECTED:
enum Color {
    Crimson,
    Green,
    Blue,
}

fn describe(c: Color) -> int {
    match c {
        Color::Crimson => 1,
        Color::Green => 2,
        Color::Blue => 3,
    }
}

fn main() {
    let c = Color::Crimson
    print(describe(c))
}
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
OPERATION: rename
NEW_NAME: Crimson
---
EXPECTED:
enum Color {
    Crimson,
    Green,
}

fn main() {
    let c = Color::Crimson
    print(c)
}
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
OPERATION: rename
NEW_NAME: Crimson
---
EXPECTED:
enum Color {
    Crimson,
    Green,
}

fn describe(c: Color) -> int {
    match c {
        Color::Crimson => 1,
        Color::Green => 2,
    }
}

fn main() {
    print(describe(Color::Crimson))
}
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
OPERATION: rename
NEW_NAME: Round
---
EXPECTED:
enum Shape {
    Round(float),
    Square(float),
}

fn area(s: Shape) -> float {
    match s {
        Shape::Round(r) => r * r * 3.14,
        Shape::Square(w) => w * w,
    }
}

fn main() {
    let s = Shape::Round(2.0)
    print(area(s))
}
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
OPERATION: rename
NEW_NAME: Crimson
---
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
OPERATION: rename
NEW_NAME: Hue
---
EXPECTED:
enum Hue {
    Red,
    Green,
}

fn describe(c: Hue) -> int {
    match c {
        Hue::Red => 1,
        Hue::Green => 2,
    }
}

fn main() {
    let c = Hue::Red
    print(describe(c))
}
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
OPERATION: rename
NEW_NAME: Item
---
EXPECTED:
fn first<Item>(x: Item, y: Item) -> Item {
    return x
}

fn second<T>(v: T) -> T {
    return v
}
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
OPERATION: rename
NEW_NAME: qx
---
EXPECTED:
struct Point {
    x: int
    y: int
}

fn get_sum(p: Point) -> int {
    match p {
        Point { x: qx, y } => qx + y,
    }
}
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
OPERATION: rename
NEW_NAME: px
---
EXPECTED:
struct Point {
    px: int
    y: int
}

fn main() {
    let x = 1
    let y = 2
    let p = Point { px: x, y }
    print(p.px)
}
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
OPERATION: rename
NEW_NAME: Beast
---
EXPECTED:
struct Beast {
    hp: int
}

global CACHED: Beast? = null

fn take(m: Beast?) {
    print(m)
}

fn give() -> Beast? {
    let held: Beast? = null
    return held
}
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
OPERATION: rename
NEW_NAME: label
---
EXPECTED:
struct Inner {
    label: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o = Outer { inner: null }
    print(o.inner?.label ?? "none")
}
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
OPERATION: rename
NEW_NAME: mob_restore
---
EXPECTED:
struct Mob {
    hp: int
}

fn mob_restore(m: Mob) -> int {
    return m.hp
}

fn main() {
    let m: Mob? = Mob { hp: 1 }
    print(m?.mob_restore() ?? 0)
}
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
OPERATION: rename
NEW_NAME: strength
---
EXPECTED:
struct Spell {
    strength: int,
    boosted: int = .strength,
}

fn main() {
    let s = Spell { strength: 2 }
    print(s.boosted)
}
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
OPERATION: rename
NEW_NAME: amount
---
EXPECTED:
fn spawn_it(amount: int, size: float) {
    print(amount)
    print(size)
}

fn main() {
    spawn_it(amount: 3, size: 1.0)
    spawn_it(4, size: 2.0)
}
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
OPERATION: rename
NEW_NAME: vitality
---
EXPECTED:
struct Mob {
    vitality: int,
}

fn main() {
    let m = Mob { vitality: 10 }
    print("{field_get(m, .vitality)}")
}
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
OPERATION: rename
NEW_NAME: POINTS
---
EXPECTED:
global POINTS: int = 0

fn bump() {
    POINTS = POINTS + 1
}

fn main() {
    bump()
    print("s {POINTS}")
}
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
OPERATION: rename
NEW_NAME: POINTS
---
EXPECTED:
global POINTS: int = 0
global SCOREBOARD: int = 1

fn bump() {
    POINTS = POINTS + 1
    SCOREBOARD = 2
}
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
OPERATION: rename
NEW_NAME: PlayerId
---
EXPECTED:
newtype PlayerId(int)

fn greet(id: PlayerId) {
    print(id)
}

fn main() {
    let id = PlayerId(42)
    greet(id)
}
===

TEST: rename_newtype_from_annotation
TYPE: rename
---
newtype UserId(int)

fn greet(id: «cursor»UserId) {
    print(id)
}

fn main() {
    let id = UserId(42)
    greet(id)
}
---
OPERATION: rename
NEW_NAME: PlayerId
---
EXPECTED:
newtype PlayerId(int)

fn greet(id: PlayerId) {
    print(id)
}

fn main() {
    let id = PlayerId(42)
    greet(id)
}
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
OPERATION: rename
NEW_NAME: power
---
EXPECTED:
enum Item {
    Potion { power: int },
    Coin,
}

fn main() {
    let i = Item::Potion { power: 3 }
    match i {
        Item::Potion { power: strength } => print(strength),
        Item::Coin => print(0),
    }
}
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
OPERATION: rename
NEW_NAME: power
---
EXPECTED:
enum Item {
    Potion { power: int },
}

fn main() {
    let i = Item::Potion { power: 3 }
    print(i)
}
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
OPERATION: rename
NEW_NAME: fancy
---
EXPECTED:
deco fancy {
    print(1)
}

@fancy
fn thing() {
    print(1)
}

@fancy
fn other() {
    print(2)
}
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
OPERATION: rename
NEW_NAME: fancy
---
EXPECTED:
deco fancy {
    print(1)
}

@fancy
fn thing() {
    print(1)
}
===
