TEST: inline_simple_variable
TYPE: code_action
---
fn main() {
    let «start»x«end» = 42
    print(x)
}
---
EXPECTED:
ACTION_TITLE: Inline local `x`
RESULT:
REPLACE: ["    let x = 42\n    print(x)\n", "    print(42)\n"]
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
EXPECTED:
ACTION_TITLE: Inline local `value`
RESULT:
REPLACE: ["    let value = 10 + 20\n    print(value)\n    let result = value * 2\n", "    print(10 + 20)\n    let result = (10 + 20) * 2\n"]
===

TEST: inline_variable_with_complex_expression
TYPE: code_action
---
fn calculate(x: int) -> int {
    let «start»temp«end» = x * 2 + 1
    return temp * temp
}
---
EXPECTED:
ACTION_TITLE: Inline local `temp`
RESULT:
REPLACE: ["    let temp = x * 2 + 1\n    return temp * temp\n", "    return (x * 2 + 1) * (x * 2 + 1)\n"]
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
EXPECTED:
ACTION_TITLE: Inline local `num`
RESULT:
REPLACE: ["    let num = 42\n    let result = process(num)\n", "    let result = process(42)\n"]
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
EXPECTED:
ACTION_TITLE: Inline local `x`
RESULT:
REPLACE: ["    let x = get_value()\n    print(x)\n", "    print(get_value())\n"]
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
EXPECTED:
ACTION_TITLE: Inline local `s`
RESULT:
REPLACE: ["    let s = p.x + p.y\n    print(s)\n    print(s)\n", "    print(p.x + p.y)\n    print(p.x + p.y)\n"]
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
EXPECTED:
ACTION_TITLE: Inline local `condition`
RESULT:
REPLACE: ["    let condition = true\n    if condition {\n", "    if true {\n"]
===

TEST: inline_string_variable
TYPE: code_action
---
fn main() {
    let «start»message«end» = "Hello, World!"
    print(message)
}
---
EXPECTED:
ACTION_TITLE: Inline local `message`
RESULT:
REPLACE: ["    let message = \"Hello, World!\"\n    print(message)\n", "    print(\"Hello, World!\")\n"]
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
EXPECTED:
ACTION_TITLE: Inline local `outer`
RESULT:
REPLACE: ["    let outer = 10\n", ""]
REPLACE: ["        let inner = outer + 5\n", "        let inner = 10 + 5\n"]
===