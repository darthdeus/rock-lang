TEST: signature_help_basic_function
TYPE: signature_help
---
fn greet(name: string, times: int) {
    for i in 0..times {
        print("Hello, " + name)
    }
}

fn main() {
    greet("World", 3)
}
---
OPERATION: signature_help
POSITION: line=7, character=10
---
EXPECTED:
greet(name: string, times: int)
===

TEST: signature_help_nested_calls
TYPE: signature_help
---
fn add(a: int, b: int) -> int {
    return a + b
}

fn multiply(x: int, y: int) -> int {
    return x * y
}

fn main() {
    let result = multiply(add(2, 3), 4)
}
---
OPERATION: signature_help
POSITION: line=9, character=30
---
EXPECTED:
add(a: int, b: int) -> int
===

TEST: signature_help_overloaded_function
TYPE: signature_help
---
fn process(x: int) -> int {
    return x * 2
}

fn process(x: float) -> float {
    return x * 2.0
}

fn process(x: string) -> string {
    return x + "_processed"
}

fn main() {
    let i = process(42)
    let f = process(3.14)
    let s = process("test")
}
---
OPERATION: signature_help
POSITION: line=14, character=20
---
EXPECTED:
ACTIVE: fn process(x: float) -> float
===

TEST: signature_help_method_call
TYPE: signature_help
---
struct Calculator {
    base: int,
}

// Rock doesn't use impl blocks, methods are defined as regular functions
fn compute(self: Calculator, x: int, y: int) -> int {
    return self.base + x + y
}

fn main() {
    let calc = Calculator { base: 10 }
    let result = compute(calc, 5, 3)
}
---
OPERATION: signature_help
POSITION: line=11, character=30
---
EXPECTED:
compute(self: Calculator, x: int, y: int) -> int
===

TEST: signature_help_generic_function
TYPE: signature_help
---
fn map<T, U>(items: [T], mapper: fn(T) -> U) -> [U] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn main() {
    let numbers = map([1, 2, 3], (x) => x * 2)
}
---
OPERATION: signature_help
POSITION: line=9, character=22
---
EXPECTED:
fn map(items: [T], mapper: fn(T) -> U) -> [U]
===

TEST: signature_help_constructor
TYPE: signature_help
---
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
}
---
OPERATION: signature_help
POSITION: line=6, character=21
---
EXPECTED:
Point { x: float, y: float }
===

TEST: signature_help_variadic_function
TYPE: signature_help
---
fn format(template: string, arg: any) -> string {
    // Implementation
    return template
}

fn main() {
    let msg = format("Hello %s", "Alice")
}
---
OPERATION: signature_help
POSITION: line=6, character=21
---
EXPECTED:
format(template: string, arg: any) -> string
===

TEST: signature_help_lambda_parameter
TYPE: signature_help
---
fn filter<T>(items: [T], predicate: fn(T) -> bool) -> [T] {
    let result = []
    for item in items {
        if predicate(item) {
            result.push(item)
        }
    }
    return result
}

fn main() {
    let evens = filter([1, 2, 3, 4], (x) => x % 2 == 0)
}
---
OPERATION: signature_help
POSITION: line=11, character=37
---
EXPECTED:
fn filter(items: [T], predicate: fn(T) -> bool) -> [T]
===