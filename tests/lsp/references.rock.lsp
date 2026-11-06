TEST: find_references_local_variable
TYPE: references
---
fn main() {
    let count = 0
    print(count)
    count = count + 1
    if count > 10 {
        print("Done")
    }
}
---
OPERATION: references
POSITION: line=1, character=8
---
EXPECTED:
file: main.rock
line: 1
character: 8
file: main.rock
line: 2
character: 10
file: main.rock
line: 3
character: 4
file: main.rock
line: 3
character: 12
file: main.rock
line: 4
character: 7
===

TEST: find_references_function
TYPE: references
---
fn calculate(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let x = calculate(5, 3)
    let y = calculate(10, 20)
    print(calculate(x, y))
}
---
OPERATION: references
POSITION: line=0, character=3
---
EXPECTED:
file: main.rock
line: 0
character: 3
file: main.rock
line: 5
character: 12
file: main.rock
line: 6
character: 12
file: main.rock
line: 7
character: 10
===

TEST: find_references_function_used_as_bare_value
TYPE: references
---
fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let result = apply(double, 10)
    print(result)
}
---
OPERATION: references
POSITION: line=9, character=23
---
EXPECTED:
file: main.rock
line: 4
character: 3
file: main.rock
line: 9
character: 23
===

TEST: find_references_struct_type
TYPE: references
---
struct Point {
    x: float,
    y: float,
}

fn distance(p1: Point, p2: Point) -> float {
    return ((p2.x - p1.x) * (p2.x - p1.x) + (p2.y - p1.y) * (p2.y - p1.y)).sqrt()
}

fn main() {
    let origin: Point = Point { x: 0.0, y: 0.0 }
    let p = Point { x: 3.0, y: 4.0 }
}
---
OPERATION: references
POSITION: line=0, character=7
---
EXPECTED:
file: main.rock
line: 0
character: 7
file: main.rock
line: 5
character: 16
file: main.rock
line: 5
character: 27
file: main.rock
line: 10
character: 16
file: main.rock
line: 10
character: 24
file: main.rock
line: 11
character: 12
===

TEST: find_references_struct_field
TYPE: references
---
struct Person {
    name: string,
    age: int,
}

fn main() {
    let p = Person { name: "Alice", age: 30 }
    print(p.name)
    let n = p.name
    if p.age > 18 {
        print("Adult: " + p.name)
    }
}
---
OPERATION: references
POSITION: line=1, character=4
---
EXPECTED:
file: main.rock
line: 1
character: 4
file: main.rock
line: 6
character: 21
file: main.rock
line: 7
character: 12
file: main.rock
line: 8
character: 14
file: main.rock
line: 10
character: 28
===

TEST: find_references_parameter
TYPE: references
---
fn greet(name: string, times: int) {
    for i in 0..times {
        print("Hello, " + name)
    }
    if times > 0 {
        print("Greeted " + name + " " + times.to_string() + " times")
    }
}

fn main() {
    greet("World", 3)
}
---
OPERATION: references
POSITION: line=0, character=9
---
EXPECTED:
file: main.rock
line: 0
character: 9
file: main.rock
line: 2
character: 26
file: main.rock
line: 5
character: 27
===
TEST: find_references_macro_param_ident_fragment
TYPE: references
---
macro decl(Name: ident, Elem: ty) {
    struct Name { v: Elem }
}
---
OPERATION: references
POSITION: line=0, character=11
---
EXPECTED:
file: main.rock
line: 0
character: 11
file: main.rock
line: 1
character: 11
===

TEST: find_references_macro_param_ty_fragment
TYPE: references
---
macro decl(Name: ident, Elem: ty) {
    struct Name { v: Elem }
}
---
OPERATION: references
POSITION: line=0, character=24
---
EXPECTED:
file: main.rock
line: 0
character: 24
file: main.rock
line: 1
character: 21
===
