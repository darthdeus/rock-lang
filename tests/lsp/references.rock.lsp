[[tests]]
name = "find_references_local_variable"
kind = "references"
position = [1, 8]
source = '''
fn main() {
    let count = 0
    print(count)
    count = count + 1
    if count > 10 {
        print("Done")
    }
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 1, character = 8 }, { line = 2, character = 10 }, { line = 3, character = 4 }, { line = 3, character = 12 }, { line = 4, character = 7 }] }]

[[tests]]
name = "find_references_function"
kind = "references"
position = [0, 3]
source = '''
fn calculate(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let x = calculate(5, 3)
    let y = calculate(10, 20)
    print(calculate(x, y))
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 3 }, { line = 5, character = 12 }, { line = 6, character = 12 }, { line = 7, character = 10 }] }]

[[tests]]
name = "find_references_function_used_as_bare_value"
kind = "references"
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
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 4, character = 3 }, { line = 9, character = 23 }] }]

[[tests]]
name = "find_references_struct_type"
kind = "references"
position = [0, 7]
source = '''
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
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 7 }, { line = 5, character = 16 }, { line = 5, character = 27 }, { line = 10, character = 16 }, { line = 10, character = 24 }, { line = 11, character = 12 }] }]

[[tests]]
name = "find_references_struct_field"
kind = "references"
position = [1, 4]
source = '''
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
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 1, character = 4 }, { line = 6, character = 21 }, { line = 7, character = 12 }, { line = 8, character = 14 }, { line = 10, character = 28 }] }]

[[tests]]
name = "find_references_parameter"
kind = "references"
position = [0, 9]
source = '''
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
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 9 }, { line = 2, character = 26 }, { line = 5, character = 27 }] }]

[[tests]]
name = "find_references_ufcs_method"
kind = "references"
position = [3, 14]
source = '''
fn inc(x: int) -> int { x + 1 }
fn main() {
    let a = inc(5)
    let b = 5.inc()
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 3 }, { line = 2, character = 12 }, { line = 3, character = 14 }] }]

[[tests]]
name = "find_references_fn_typed_parameter_call"
kind = "references"
position = [1, 10]
source = '''
fn apply(g: fn() -> int) -> int {
    g() + g()
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 9 }, { line = 1, character = 4 }, { line = 1, character = 10 }] }]

[[tests]]
name = "find_references_macro_param_ident_fragment"
kind = "references"
position = [0, 11]
source = '''
macro decl(Name: ident, Elem: ty) {
    struct Name { v: Elem }
}'''
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 11 }, { line = 1, character = 11 }] }]

[[tests]]
name = "find_references_macro_param_ty_fragment"
kind = "references"
position = [0, 24]
source = { from_test = "find_references_macro_param_ident_fragment" }
request = { transport = "cli", argv = ["references", "$target"] }
checks = [{ op = "unordered_equal", path = ["result", "references", "*", "range", "start"], value = [{ line = 0, character = 24 }, { line = 1, character = 21 }] }]
