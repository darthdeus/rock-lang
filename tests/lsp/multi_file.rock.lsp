TEST: rename_across_files_function
TYPE: rename
FILES:
---
FILE: math.rock
---
fn «start»add«end»(a: int, b: int) -> int {
    return a + b
}

fn multiply(a: int, b: int) -> int {
    return a * b
}
---
FILE: main.rock
---
fn main() {
    let sum = add(5, 3)
    let product = multiply(5, 3)
    print("Sum: " + sum.to_string())
    print("Product: " + product.to_string())
}
---
FILE: project.toml
---
[package]
name = "test"
---
OPERATION: rename
NEW_NAME: sum_two_numbers
---
EXPECTED:
FILE: math.rock
---
fn sum_two_numbers(a: int, b: int) -> int {
    return a + b
}

fn multiply(a: int, b: int) -> int {
    return a * b
}
---
FILE: main.rock
---
fn main() {
    let sum = sum_two_numbers(5, 3)
    let product = multiply(5, 3)
    print("Sum: " + sum.to_string())
    print("Product: " + product.to_string())
}
---
FILE: project.toml
---
[package]
name = "test"
===

TEST: rename_type_across_modules
TYPE: rename
FILES:
---
FILE: types.rock
---
struct «start»Point«end» {
    x: float,
    y: float,
}

fn distance(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}
---
FILE: geometry.rock
---
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
}
---
FILE: main.rock
---
fn main() {
    let origin: Point = create_origin()
    let square = create_unit_square()
    
    for p in square {
        let d = distance(origin, p)
        print("Distance from origin: " + d.to_string())
    }
}
---
FILE: project.toml
---
[package]
name = "test"
---
OPERATION: rename
NEW_NAME: Point2D
---
EXPECTED:
FILE: types.rock
---
struct Point2D {
    x: float,
    y: float,
}

fn distance(p1: Point2D, p2: Point2D) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}
---
FILE: geometry.rock
---
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
}
---
FILE: main.rock
---
fn main() {
    let origin: Point2D = create_origin()
    let square = create_unit_square()
    
    for p in square {
        let d = distance(origin, p)
        print("Distance from origin: " + d.to_string())
    }
}
---
FILE: project.toml
---
[package]
name = "test"
===