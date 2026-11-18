[[tests]]
name = "hover_basic_types"
kind = "hover"
position = [1, 8]
source = '''
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
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_function_types"
kind = "hover"
position = [0, 3]
source = '''
fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let result = add(5, 3)
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nfn add(a: int, b: int) -> int\n```" }]

[[tests]]
name = "hover_struct_types"
kind = "hover"
position = [0, 7]
source = '''
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "struct Point {\n  x: float,\n  y: float,\n}" }]

[[tests]]
name = "hover_generic_types"
kind = "hover"
position = [5, 8]
source = '''
fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num = identity(42)
    let text = identity("hello")
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_nullable_types"
kind = "hover"
position = [9, 8]
source = '''
fn find_user(id: int) -> string? {
    if id == 1 {
        return "Alice"
    } else {
        return null
    }
}

fn main() {
    let user = find_user(1)
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: string?" }]

[[tests]]
name = "hover_array_types"
kind = "hover"
position = [1, 8]
source = '''
fn main() {
    let numbers = [1, 2, 3, 4, 5]
    let texts = ["hello", "world"]
    let matrix = [[1, 2], [3, 4]]
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: [int]" }]

[[tests]]
name = "hover_shorthand_x"
kind = "hover"
position = [10, 20]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Shorthand syntax - hover on 'x' field
    let p = Point { x, y }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_shorthand_y"
kind = "hover"
position = [10, 23]
source = { from_test = "hover_shorthand_x", replace = [["    // Shorthand syntax - hover on 'x' field", "    // Shorthand syntax - hover on 'y' field"]] }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_shorthand_nested"
kind = "hover"
position = [21, 27]
source = '''
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
    let rect = Rectangle { top_left, width, height }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: struct Point" }]

[[tests]]
name = "hover_mixed_shorthand"
kind = "hover"
position = [9, 20]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100

    // Mixed shorthand and explicit - hover on 'x' field
    let p = Point { x, y: 200 }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_for_loop_variable"
kind = "hover"
position = [6, 8]
source = '''
struct Item {
    name: string
}

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        print(item.name)
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: struct Item" }]

[[tests]]
name = "hover_lambda_type"
kind = "hover"
position = [1, 8]
source = '''
fn main() {
    let f = (x: int) => x * 2
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: fn(int) -> int" }]

[[tests]]
name = "hover_nullable_type"
kind = "hover"
position = [1, 8]
source = '''
fn main() {
    let x: int? = null
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int?" }]

[[tests]]
name = "hover_enum_variant"
kind = "hover"
position = [7, 8]
source = '''
enum Color {
    Red,
    Green,
    Blue,
}

fn main() {
    let c = Color::Red
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: enum Color" }]

[[tests]]
name = "hover_generic_instance"
kind = "hover"
position = [5, 8]
source = '''
struct Box<T> {
    item: T
}

fn main() {
    let b = Box { item: 42 }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: Box<int>" }]

[[tests]]
name = "hover_global_assignment"
kind = "hover"
position = [3, 4]
source = '''
global score: int = 0

fn main() {
    score = 100
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_safe_nav_result"
kind = "hover"
position = [6, 8]
source = '''
struct Player {
    name: string
}

fn main() {
    let p: Player? = null
    let n = p?.name
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: string?" }]

[[tests]]
name = "hover_null_coalesce_result"
kind = "hover"
position = [2, 8]
source = '''
fn main() {
    let x: int? = null
    let y = x ?? 42
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_tuple_destructure"
kind = "hover"
position = [2, 9]
source = '''
fn main() {
    let pair = (1, "hello")
    let (a, b) = pair
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_nested_tuple_destructure"
kind = "hover"
position = [2, 10]
source = '''
fn main() {
    let pair = ((1, 2), "hello")
    let ((a, b), c) = pair
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_match_record_binding_shorthand"
kind = "hover"
position = [8, 20]
source = '''
enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { value } => { print(value) },
        Opt::None => {},
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_match_record_binding_explicit"
kind = "hover"
position = [8, 27]
source = { from_test = "hover_match_record_binding_shorthand", replace = [["        Opt::Some { value } => { print(value) },", "        Opt::Some { value: v } => { print(v) },"]] }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_match_enum_tuple_binding"
kind = "hover"
position = [8, 18]
source = '''
enum Opt {
    Some(int),
    None,
}

fn main() {
    let o = Opt::Some(42)
    match o {
        Opt::Some(v) => { print(v) },
        Opt::None => {},
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_while_let_binding"
kind = "hover"
position = [2, 14]
source = '''
fn main() {
    let x: int? = 5
    while let val = x {
        print(val)
        break
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_if_let_binding"
kind = "hover"
position = [2, 11]
source = '''
fn main() {
    let x: int? = 5
    if let val = x {
        print(val)
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_match_arm_record_variant"
kind = "hover"
position = [8, 11]
source = '''
enum E {
    A(x: int),
    B { y: int, z: int },
}

fn f(e: E) -> int {
    match e {
        E::A(n) => n
        E::B { y, z } => y + z
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "```rock\nE::B { y: int, z: int }\n```" }]

[[tests]]
name = "hover_match_arm_record_variant_field_binding"
kind = "hover"
position = [8, 15]
source = { from_test = "hover_match_arm_record_variant" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int" }]

[[tests]]
name = "hover_match_record_variant_field_name"
kind = "hover"
position = [8, 20]
source = { from_test = "hover_match_record_binding_explicit" }
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "value: int (field of Opt::Some)" }]

[[tests]]
name = "hover_struct_pattern_field_name"
kind = "hover"
position = [5, 16]
source = '''
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { x: a, y: b } => { print(a + b) },
    }
}'''
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "x: int (field of Point)" }]
