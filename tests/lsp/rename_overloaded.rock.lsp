TEST: rename_overloaded_function_from_definition
TYPE: rename
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

fn process(x: int, y: int) -> int {
    return x + y
}

fn main() {
    // Test all overloads
    let i = process(42)
    let f = process(3.14)
    let s = process("test")
    let sum = process(10, 20)

    // Multiple calls to same overload
    let i2 = process(100)
    let f2 = process(2.71)
}
---
OPERATION: rename
POSITION: line=0, character=3
NEW_NAME: transform
---
EXPECTED:
- transform(x: int) -> int
- transform(x: float) -> float
- transform(x: string) -> string
- transform(x: int, y: int) -> int
- transform(42)
- transform(3.14)
- transform("test")
- transform(10, 20)
- transform(100)
- transform(2.71)
===

TEST: rename_overloaded_function_from_call
TYPE: rename
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
    let i = process(42)  // Rename from int overload call
    let f = process(3.14)
    let s = process("test")
}
---
OPERATION: rename
POSITION: line=13, character=12
NEW_NAME: convert
---
EXPECTED:
- convert(x: int) -> int
- convert(x: float) -> float
- convert(x: string) -> string
- convert(42)
- convert(3.14)
- convert("test")
===

TEST: rename_single_overload_parameter
TYPE: rename
---
fn calculate(x: int, y: int) -> int {
    return x + y
}

fn calculate(values: [int]) -> int {
    let sum = 0
    for v in values {
        sum = sum + v
    }
    return sum
}

fn main() {
    let result1 = calculate(5, 10)
    let result2 = calculate([1, 2, 3, 4, 5])
}
---
OPERATION: rename
POSITION: line=0, character=13
NEW_NAME: a
---
EXPECTED:
- calculate(a: int, y: int)
- return a + y
===

TEST: rename_generic_overloaded_function
TYPE: rename
---
fn convert<T>(items: [T], mapper: fn(T) -> T) -> [T] {
    let result = []
    for item in items {
        result.push(mapper(item))
    }
    return result
}

fn convert<K, V>(dict: Map<K, V>, fallback: V) -> V {
    return fallback
}

fn main() {
    let numbers = convert([1, 2, 3], x => x * 2)
    let d: Map<int, int> = map_new()
    let single = convert(d, 0)
}
---
OPERATION: rename
POSITION: line=0, character=3
NEW_NAME: transform
---
EXPECTED:
- transform<T>(items: [T]
===