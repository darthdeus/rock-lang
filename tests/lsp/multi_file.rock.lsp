[[tests]]
name = "rename_across_files_function"
kind = "rename"
position = [0, 3]
primary_file = "math.rock"
request = { transport = "cli", argv = ["rename", "$target", "sum_two_numbers"] }
checks = [
  { op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true },
  { op = "applied_text_equals", path = ["result", "changes"], file = "math.rock", expected = "fn sum_two_numbers(a: int, b: int) -> int {\n    return a + b\n}\n\nfn multiply(a: int, b: int) -> int {\n    return a * b\n}" },
  { op = "applied_text_equals", path = ["result", "changes"], file = "main.rock", expected = "fn main() {\n    let sum = sum_two_numbers(5, 3)\n    let product = multiply(5, 3)\n    print(\"Sum: \" + sum.to_string())\n    print(\"Product: \" + product.to_string())\n}" },
]

[[tests.files]]
path = "math.rock"
text = '''
fn add(a: int, b: int) -> int {
    return a + b
}

fn multiply(a: int, b: int) -> int {
    return a * b
}'''

[[tests.files]]
path = "main.rock"
text = '''
fn main() {
    let sum = add(5, 3)
    let product = multiply(5, 3)
    print("Sum: " + sum.to_string())
    print("Product: " + product.to_string())
}'''

[[tests.files]]
path = "stick.toml"
text = '''
[package]
name = "test"'''

[[tests]]
name = "rename_type_across_modules"
kind = "rename"
position = [0, 7]
primary_file = "types.rock"
request = { transport = "cli", argv = ["rename", "$target", "Point2D"] }

[[tests.checks]]
op = "empty"
path = ["error"]
missing_is_empty = true
null_is_empty = true

[[tests.checks]]
op = "applied_text_equals"
path = ["result", "changes"]
file = "types.rock"
expected = '''
struct Point2D {
    x: float,
    y: float,
}

fn distance(p1: Point2D, p2: Point2D) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}'''

[[tests.checks]]
op = "applied_text_equals"
path = ["result", "changes"]
file = "geometry.rock"
expected = '''
fn create_origin() -> Point2D {
    return Point2D { x: 0.0, y: 0.0 }
}

fn create_unit_square() -> [Point2D] {
    return [
        Point2D { x: 0.0, y: 0.0 },
        Point2D { x: 1.0, y: 0.0 },
        Point2D { x: 1.0, y: 1.0 },
        Point2D { x: 0.0, y: 1.0 },
    ]
}'''

[[tests.checks]]
op = "applied_text_equals"
path = ["result", "changes"]
file = "main.rock"
expected = """
fn main() {
    let origin: Point2D = create_origin()
    let square = create_unit_square()
\u0020\u0020\u0020\u0020
    for p in square {
        let d = distance(origin, p)
        print(\"Distance from origin: \" + d.to_string())
    }
}"""

[[tests.files]]
path = "types.rock"
text = '''
struct Point {
    x: float,
    y: float,
}

fn distance(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}'''

[[tests.files]]
path = "geometry.rock"
text = '''
fn create_origin() -> Point {
    return Point { x: 0.0, y: 0.0 }
}

fn create_unit_square() -> [Point] {
    return [
        Point { x: 0.0, y: 0.0 },
        Point { x: 1.0, y: 0.0 },
        Point { x: 1.0, y: 1.0 },
        Point { x: 0.0, y: 1.0 },
    ]
}'''

[[tests.files]]
path = "main.rock"
text = """
fn main() {
    let origin: Point = create_origin()
    let square = create_unit_square()
\u0020\u0020\u0020\u0020
    for p in square {
        let d = distance(origin, p)
        print(\"Distance from origin: \" + d.to_string())
    }
}"""

[[tests.files]]
path = "stick.toml"
text = '''
[package]
name = "test"'''
