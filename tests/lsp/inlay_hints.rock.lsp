TEST: inlay_hints_let_bindings
TYPE: inlay_hints
---
fn main() {
    let x = 42
    let y = "hello"
    let z = [1, 2, 3]
    let w = true
}
---
OPERATION: inlay_hints
---
EXPECTED:
fn main() {
    let x«hint:: int» = 42
    let y«hint:: string» = "hello"
    let z«hint:: [int]» = [1, 2, 3]
    let w«hint:: bool» = true
}
===

TEST: inlay_hints_complex_types
TYPE: inlay_hints
---
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let points = [p, Point { x: 3.0, y: 4.0 }]
    let map = map_new()
    map.insert("origin", p)
}
---
OPERATION: inlay_hints
---
EXPECTED:
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p«hint:: Point» = Point { x: 1.0, y: 2.0 }
    let points«hint:: [Point]» = [p, Point { x: 3.0, y: 4.0 }]
    let map«hint:: Map<string, Point>» = map_new()
    map.insert("origin", p)
}
===

TEST: inlay_hints_function_parameters
TYPE: inlay_hints
---
fn calculate(x: int, y: int, operation: string) -> int {
    if operation == "add" {
        return x + y
    } else {
        return x - y
    }
}

fn main() {
    let result = calculate(10, 20, "add")
}
---
OPERATION: inlay_hints
---
EXPECTED:
fn calculate(x: int, y: int, operation: string) -> int {
    if operation == "add" {
        return x + y
    } else {
        return x - y
    }
}

fn main() {
    let result«hint:: int» = calculate(«hint::x:»10, «hint::y:»20, «hint::operation:»"add")
}
===

TEST: inlay_hints_generic_types
TYPE: inlay_hints
---
fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num = identity(42)
    let text = identity("hello")
    let list = identity([1, 2, 3])
}
---
OPERATION: inlay_hints
---
EXPECTED:
fn identity<T>(value: T) -> T {
    return value
}

fn main() {
    let num«hint:: int» = identity(«hint::value:»42)
    let text«hint:: string» = identity(«hint::value:»"hello")
    let list«hint:: [int]» = identity([1, 2, 3])
}
===

TEST: inlay_hints_nullable_types
TYPE: inlay_hints
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
    let missing = find_user(999)
}
---
OPERATION: inlay_hints
---
EXPECTED:
fn find_user(id: int) -> string? {
    if id == 1 {
        return "Alice"
    } else {
        return null
    }
}

fn main() {
    let user«hint:: string?» = find_user(«hint::id:»1)
    let missing«hint:: string?» = find_user(«hint::id:»999)
}
===

TEST: inlay_hints_closures
TYPE: inlay_hints
---
fn main() {
    let add = (a, b) => a + b
    let multiply = (x: int, y: int) => { return x * y }
    let greet = (name) => print("Hello, " + name)
}
---
OPERATION: inlay_hints
---
EXPECTED:
fn main() {
    let add = (a, b) => a + b
    let multiply«hint:: fn(int, int) -> int» = (x: int, y: int) => { return x * y }
    let greet = (name) => print("Hello, " + name)
}
===
TEST: inlay_hints_for_loop_variable
TYPE: inlay_hints
---
struct Item { name: string }

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        print(item.name)
    }
}
---
OPERATION: inlay_hints
---
EXPECTED:
struct Item { name: string }

fn main() {
    let items: [Item] = vec_new()
    for item«hint:: Item» in items {
        print(item.name)
    }
}
===

TEST: inlay_hints_match_arm_enum_variant
TYPE: inlay_hints
---
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => print(p.x),
        Opt::None => {},
    }
}
---
OPERATION: inlay_hints
---
EXPECTED:
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o«hint:: Opt» = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p«hint:: Point») => print(p.x),
        Opt::None => {},
    }
}
===

TEST: inlay_hints_dform_receiver
TYPE: inlay_hints
---
struct Mob { health: float }

fn Mob.take_damage(m, amount: float) {
    m.health = m.health - amount
}

fn Vec<T>.first(v) -> T {
    return v[0]
}
---
OPERATION: inlay_hints
---
EXPECTED:
struct Mob { health: float }

fn Mob.take_damage(m«hint:: Mob», amount: float) {
    m.health = m.health - amount
}

fn Vec<T>.first(v«hint:: Vec<T>») -> T {
    return v[0]
}
===
