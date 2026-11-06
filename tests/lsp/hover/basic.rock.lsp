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
OPERATION: hover
POSITION: line=1, character=8
---
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
OPERATION: hover
POSITION: line=0, character=3
---
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
OPERATION: hover
POSITION: line=0, character=7
---
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
OPERATION: hover
POSITION: line=5, character=8
---
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
OPERATION: hover
POSITION: line=9, character=8
---
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
OPERATION: hover
POSITION: line=1, character=8
---
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
OPERATION: hover
---
EXPECTED:
Type: int
===

TEST: hover_shorthand_y
TYPE: hover
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Shorthand syntax - hover on 'y' field
    let p = Point { x, «cursor»y }
}
---
OPERATION: hover
---
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
---
EXPECTED:
Type: int
===

TEST: hover_match_record_binding_explicit
TYPE: hover
---
enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { value: «cursor»v } => { print(v) },
        Opt::None => {},
    }
}
---
OPERATION: hover
---
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
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
OPERATION: hover
---
EXPECTED:
```rock
E::B { y: int, z: int }
```
===

TEST: hover_match_arm_record_variant_field_binding
TYPE: hover
---
enum E {
    A(x: int),
    B { y: int, z: int },
}

fn f(e: E) -> int {
    match e {
        E::A(n) => n
        E::B { «cursor»y, z } => y + z
    }
}
---
OPERATION: hover
---
EXPECTED:
Type: int
===

TEST: hover_match_record_variant_field_name
TYPE: hover
---
enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { «cursor»value: v } => { print(v) },
        Opt::None => {},
    }
}
---
OPERATION: hover
---
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
OPERATION: hover
---
EXPECTED:
x: int (field of Point)
===
