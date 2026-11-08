TEST: inline_simple_variable
TYPE: code_action
---
fn main() {
    let «start»x«end» = 42
    print(x)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `x`
RESULT:
fn main() {
    print(42)
}
===

TEST: inline_variable_multiple_uses
TYPE: code_action
---
fn main() {
    let «start»value«end» = 10 + 20
    print(value)
    let result = value * 2
    return result
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `value`
RESULT:
fn main() {
    print(10 + 20)
    let result = (10 + 20) * 2
    return result
}
===

TEST: inline_variable_with_complex_expression
TYPE: code_action
---
fn calculate(x: int) -> int {
    let «start»temp«end» = x * 2 + 1
    return temp * temp
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `temp`
RESULT:
fn calculate(x: int) -> int {
    return (x * 2 + 1) * (x * 2 + 1)
}
===

TEST: inline_variable_in_function_call
TYPE: code_action
---
fn process(value: int) -> int {
    return value * 2
}

fn main() {
    let «start»num«end» = 42
    let result = process(num)
    print(result)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `num`
RESULT:
fn process(value: int) -> int {
    return value * 2
}

fn main() {
    let result = process(42)
    print(result)
}
===

TEST: inline_variable_with_side_effects
TYPE: code_action
---
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let «start»x«end» = get_value()
    print(x)
    print(x)
}
---
OPERATION: code_action
---
EXPECTED:
NO_ACTION: Inline local `x`
===

TEST: inline_side_effecting_initializer_with_one_use
TYPE: code_action
---
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let «start»x«end» = get_value()
    print(x)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `x`
RESULT:
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    print(get_value())
}
===

TEST: no_inline_of_a_side_effecting_initializer_used_in_a_loop
TYPE: code_action
---
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let «start»x«end» = get_value()
    for i in 0..3 {
        print(x + i)
    }
}
---
OPERATION: code_action
---
EXPECTED:
NO_ACTION: Inline local `x`
===

TEST: no_inline_of_a_side_effecting_initializer_used_in_a_closure
TYPE: code_action
---
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let «start»x«end» = get_value()
    let f = () => x
    print(f())
}
---
OPERATION: code_action
---
EXPECTED:
NO_ACTION: Inline local `x`
===

TEST: no_inline_of_an_indexing_initializer_with_two_uses
TYPE: code_action
---
fn main() {
    let xs = [1, 2, 3]
    let «start»x«end» = xs[0]
    print(x)
    print(x)
}
---
OPERATION: code_action
---
EXPECTED:
NO_ACTION: Inline local `x`
===

TEST: inline_pure_field_arithmetic_with_two_uses
TYPE: code_action
---
struct Point {
    x: int
    y: int
}

fn main() {
    let p = Point { x: 1, y: 2 }
    let «start»s«end» = p.x + p.y
    print(s)
    print(s)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `s`
RESULT:
struct Point {
    x: int
    y: int
}

fn main() {
    let p = Point { x: 1, y: 2 }
    print(p.x + p.y)
    print(p.x + p.y)
}
===

TEST: inline_variable_in_if_condition
TYPE: code_action
---
fn main() {
    let «start»condition«end» = true
    if condition {
        print("Yes")
    }
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `condition`
RESULT:
fn main() {
    if true {
        print("Yes")
    }
}
===

TEST: inline_string_variable
TYPE: code_action
---
fn main() {
    let «start»message«end» = "Hello, World!"
    print(message)
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `message`
RESULT:
fn main() {
    print("Hello, World!")
}
===

TEST: inline_variable_nested_scope
TYPE: code_action
---
fn main() {
    let «start»outer«end» = 10
    {
        let inner = outer + 5
        print(inner)
    }
}
---
OPERATION: code_action
---
EXPECTED:
ACTION_TITLE: Inline local `outer`
RESULT:
fn main() {
    {
        let inner = 10 + 5
        print(inner)
    }
}
===