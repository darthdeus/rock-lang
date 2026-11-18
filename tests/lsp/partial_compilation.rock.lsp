[[tests]]
name = "completion_during_editing_with_syntax_error"
kind = "completion"
position = [6, 16]
source = """
fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let x = 42
    let y = add(
    // Syntax error: unclosed function call
}"""
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x", "y", "add"] }]

[[tests]]
name = "completion_after_valid_then_broken"
kind = "completion"
position = [10, 4]
source = """
struct Point {
    x: float,
    y: float,
}

fn create_point() -> Point {
    return Point { x: 1.0, y: 2.0 }
}

fn get_point() -> Point {
\u0020\u0020\u0020\u0020
}

fn main() {
    let invalid_syntax = broken
    // This line has invalid syntax but completion should still work
}"""
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["create_point", "Point"] }]

[[tests]]
name = "hover_during_editing_with_error"
kind = "hover"
position = [1, 12]
source = """
fn calculate(num: int) -> int {
    return num * 2
}

fn get_value() -> int {
    return calculate(21)
}

fn main() {
    let broken = this is invalid syntax
}"""
request = { transport = "cli", argv = ["hover", "$target"] }
checks = [{ op = "trimmed_equals", path = ["result", "hover", "contents"], value = "Type: int\n" }]

[[tests]]
name = "inlay_hints_with_partial_compilation"
kind = "inlay_hints"
source = """
fn process(value: int) -> string {
    return value.to_string()
}

fn get_result() -> string {
    let result = process(123)
    return result
}

fn main() {
    let broken = invalid syntax here!!!
}"""
request = { transport = "cli", argv = ["inlay-hints", "$file"] }
checks = [{ op = "contains_all", path = ["result", "hints"], value = [{ position = { line = 5 }, label = { text = "string", trim = true, strip_prefix = ":" } }, { position = { line = 5 }, label = { text = "value:", trim = true, strip_prefix = ":" } }] }]
