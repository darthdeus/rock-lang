TEST: hover_basic_types
TYPE: hover
---
fn main() {
    let number = 42
    let text = "hello"
    let flag = true
    let array = [1, 2, 3]
    let point = Point { x: 1.0, y: 2.0 }
}

struct Point {
    x: float,
    y: float,
}
---
POSITION: line=1, character=8
EXPECTED:
Type: int
===

TEST: hover_function_types
TYPE: hover
---
fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let result = add(5, 3)
}
---
POSITION: line=0, character=3
EXPECTED:
```rock
fn add(a: int, b: int) -> int
```
===

TEST: hover_struct_types
TYPE: hover
---
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
}
---
POSITION: line=0, character=7
EXPECTED:
struct Point {
  x: float,
  y: float,
}
===

TEST: hover_generic_types
TYPE: hover
---
fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num = identity(42)
    let text = identity("hello")
}
---
POSITION: line=5, character=8
EXPECTED:
Type: int
===

TEST: hover_nullable_types
TYPE: hover
---
fn find_user(id: int) -> string? {
    if id == 1 {
        return "Alice"
    } else {
        return null
    }
}

fn main() {
    let user = find_user(1)
}
---
POSITION: line=9, character=8
EXPECTED:
Type: string?
===

TEST: hover_array_types
TYPE: hover
---
fn main() {
    let numbers = [1, 2, 3, 4, 5]
    let texts = ["hello", "world"]
    let matrix = [[1, 2], [3, 4]]
}
---
POSITION: line=1, character=8
EXPECTED:
Type: [int]
===

TEST: hover_shorthand_x
TYPE: hover
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Shorthand syntax - hover on 'x' field
    let p = Point { «cursor»x, y }
}
---
EXPECTED:
Type: int
===

TEST: hover_shorthand_y
TYPE: hover
SOURCE: hover_shorthand_x
REPLACE: ["    // Shorthand syntax - hover on 'x' field", "    // Shorthand syntax - hover on 'y' field"]
POSITION: line=10, character=23
EXPECTED:
Type: int
===

TEST: hover_shorthand_nested
TYPE: hover
---
struct Point {
    x: int
    y: int
}

struct Rectangle {
    top_left: Point
    width: int
    height: int
}

fn main() {
    let x = 100
    let y = 200
    let width = 50
    let height = 75

    let p = Point { x, y }
    let top_left = p

    // Nested shorthand - hover on 'top_left' field
    let rect = Rectangle { «cursor»top_left, width, height }
}
---
EXPECTED:
Type: struct Point
===

TEST: hover_mixed_shorthand
TYPE: hover
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100

    // Mixed shorthand and explicit - hover on 'x' field
    let p = Point { «cursor»x, y: 200 }
}
---
EXPECTED:
Type: int
===

TEST: hover_for_loop_variable
TYPE: hover
---
struct Item {
    name: string
}

fn main() {
    let items: [Item] = vec_new()
    for «cursor»item in items {
        print(item.name)
    }
}
---
EXPECTED:
Type: struct Item
===

TEST: hover_lambda_type
TYPE: hover
---
fn main() {
    let «cursor»f = (x: int) => x * 2
}
---
EXPECTED:
Type: fn(int) -> int
===

TEST: hover_nullable_type
TYPE: hover
---
fn main() {
    let «cursor»x: int? = null
}
---
EXPECTED:
Type: int?
===

TEST: hover_enum_variant
TYPE: hover
---
enum Color {
    Red,
    Green,
    Blue,
}

fn main() {
    let «cursor»c = Color::Red
}
---
EXPECTED:
Type: enum Color
===

TEST: hover_generic_instance
TYPE: hover
---
struct Box<T> {
    item: T
}

fn main() {
    let «cursor»b = Box { item: 42 }
}
---
EXPECTED:
Type: Box<int>
===

TEST: hover_global_assignment
TYPE: hover
---
global score: int = 0

fn main() {
    «cursor»score = 100
}
---
EXPECTED:
Type: int
===

TEST: hover_safe_nav_result
TYPE: hover
---
struct Player {
    name: string
}

fn main() {
    let p: Player? = null
    let «cursor»n = p?.name
}
---
EXPECTED:
Type: string?
===

TEST: hover_null_coalesce_result
TYPE: hover
---
fn main() {
    let x: int? = null
    let «cursor»y = x ?? 42
}
---
EXPECTED:
Type: int
===

TEST: hover_tuple_destructure
TYPE: hover
---
fn main() {
    let pair = (1, "hello")
    let («cursor»a, b) = pair
}
---
EXPECTED:
Type: int
===

TEST: hover_nested_tuple_destructure
TYPE: hover
---
fn main() {
    let pair = ((1, 2), "hello")
    let ((«cursor»a, b), c) = pair
}
---
EXPECTED:
Type: int
===

TEST: hover_match_record_binding_shorthand
TYPE: hover
---
enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { «cursor»value } => { print(value) },
        Opt::None => {},
    }
}
---
EXPECTED:
Type: int
===

TEST: hover_match_record_binding_explicit
TYPE: hover
SOURCE: hover_match_record_binding_shorthand
REPLACE: ["        Opt::Some { value } => { print(value) },", "        Opt::Some { value: v } => { print(v) },"]
POSITION: line=8, character=27
EXPECTED:
Type: int
===

TEST: hover_match_enum_tuple_binding
TYPE: hover
---
enum Opt {
    Some(int),
    None,
}

fn main() {
    let o = Opt::Some(42)
    match o {
        Opt::Some(«cursor»v) => { print(v) },
        Opt::None => {},
    }
}
---
EXPECTED:
Type: int
===

TEST: hover_while_let_binding
TYPE: hover
---
fn main() {
    let x: int? = 5
    while let «cursor»val = x {
        print(val)
        break
    }
}
---
EXPECTED:
Type: int
===

TEST: hover_if_let_binding
TYPE: hover
---
fn main() {
    let x: int? = 5
    if let «cursor»val = x {
        print(val)
    }
}
---
EXPECTED:
Type: int
===

TEST: hover_match_arm_record_variant
TYPE: hover
---
enum E {
    A(x: int),
    B { y: int, z: int },
}

fn f(e: E) -> int {
    match e {
        E::A(n) => n
        E::«cursor»B { y, z } => y + z
    }
}
---
EXPECTED:
```rock
E::B { y: int, z: int }
```
===

TEST: hover_match_arm_record_variant_field_binding
TYPE: hover
SOURCE: hover_match_arm_record_variant
POSITION: line=8, character=15
EXPECTED:
Type: int
===

TEST: hover_match_record_variant_field_name
TYPE: hover
SOURCE: hover_match_record_binding_explicit
POSITION: line=8, character=20
EXPECTED:
value: int (field of Opt::Some)
===

TEST: hover_struct_pattern_field_name
TYPE: hover
---
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { «cursor»x: a, y: b } => { print(a + b) },
    }
}
---
EXPECTED:
x: int (field of Point)
===
