TEST: completion_during_editing_with_syntax_error
TYPE: completion
---
fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let x = 42
    let y = add(«cursor»
    // Syntax error: unclosed function call
}
---
OPERATION: completion
---
EXPECTED:
- x
- y
- add

===

TEST: completion_after_valid_then_broken
TYPE: completion
---
struct Point {
    x: float,
    y: float,
}

fn create_point() -> Point {
    return Point { x: 1.0, y: 2.0 }
}

fn get_point() -> Point {
    «cursor»
}

fn main() {
    let invalid_syntax = broken
    // This line has invalid syntax but completion should still work
}
---
OPERATION: completion
---
EXPECTED:
- create_point
- Point

===

TEST: hover_during_editing_with_error
TYPE: hover
---
fn calculate(num: int) -> int {
    return num * 2
}

fn get_value() -> int {
    return calculate(21)
}

fn main() {
    let broken = this is invalid syntax
}
---
OPERATION: hover
POSITION: line=1, character=12
---
EXPECTED:
Type: int

===

TEST: inlay_hints_with_partial_compilation
TYPE: inlay_hints
---
fn process(value: int) -> string {
    return value.to_string()
}

fn get_result() -> string {
    let result = process(123)
    return result
}

fn main() {
    let broken = invalid syntax here!!!
}
---
OPERATION: inlay_hints
---
EXPECTED:
fn process(value: int) -> string {
    return value.to_string()
}

fn get_result() -> string {
    let result«hint:: string» = process(«hint::value:»123)
    return result
}

fn main() {
    let broken = invalid syntax here!!!
}