[[tests]]
name = "signature_help_basic_function"
kind = "signature_help"
position = [7, 10]
source = '''
fn greet(name: string, times: int) {
    for i in 0..times {
        print("Hello, " + name)
    }
}

fn main() {
    greet("World", 3)
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "greet(name: string, times: int)" }]

[[tests]]
name = "signature_help_nested_calls"
kind = "signature_help"
position = [9, 30]
source = '''
fn add(a: int, b: int) -> int {
    return a + b
}

fn multiply(x: int, y: int) -> int {
    return x * y
}

fn main() {
    let result = multiply(add(2, 3), 4)
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "add(a: int, b: int) -> int" }]

[[tests]]
name = "signature_help_overloaded_function"
kind = "signature_help"
position = [14, 20]
source = '''
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
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], index_from = ["result", "activeSignature"], value = "fn process(x: float) -> float" }]

[[tests]]
name = "signature_help_method_call"
kind = "signature_help"
position = [11, 30]
source = '''
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
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "compute(self: Calculator, x: int, y: int) -> int" }]

[[tests]]
name = "signature_help_generic_function"
kind = "signature_help"
position = [9, 22]
source = '''
fn map<T, U>(items: [T], mapper: fn(T) -> U) -> [U] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn main() {
    let numbers = map([1, 2, 3], (x) => x * 2)
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "fn map(items: [T], mapper: fn(T) -> U) -> [U]" }]

[[tests]]
name = "signature_help_constructor"
kind = "signature_help"
position = [6, 21]
source = '''
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "Point { x: float, y: float }" }]

[[tests]]
name = "signature_help_variadic_function"
kind = "signature_help"
position = [6, 21]
source = '''
fn format(template: string, arg: any) -> string {
    // Implementation
    return template
}

fn main() {
    let msg = format("Hello %s", "Alice")
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "format(template: string, arg: any) -> string" }]

[[tests]]
name = "signature_help_lambda_parameter"
kind = "signature_help"
position = [11, 37]
source = '''
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
}'''
request = { transport = "cli", argv = ["signature-help", "$target"] }
checks = [{ op = "contains", path = ["result", "signatures", "*", "label"], value = "fn filter(items: [T], predicate: fn(T) -> bool) -> [T]" }]
