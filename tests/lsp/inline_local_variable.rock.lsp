[[tests]]
name = "inline_simple_variable"
kind = "code_action"
position = [1, 8]
source = '''
fn main() {
    let x = 42
    print(x)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `x`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let x = 42\n    print(x)\n", "    print(42)\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_variable_multiple_uses"
kind = "code_action"
position = [1, 8]
source = '''
fn main() {
    let value = 10 + 20
    print(value)
    let result = value * 2
    return result
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `value`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let value = 10 + 20\n    print(value)\n    let result = value * 2\n", "    print(10 + 20)\n    let result = (10 + 20) * 2\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_variable_with_complex_expression"
kind = "code_action"
position = [1, 8]
source = '''
fn calculate(x: int) -> int {
    let temp = x * 2 + 1
    return temp * temp
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `temp`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let temp = x * 2 + 1\n    return temp * temp\n", "    return (x * 2 + 1) * (x * 2 + 1)\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_variable_in_function_call"
kind = "code_action"
position = [5, 8]
source = '''
fn process(value: int) -> int {
    return value * 2
}

fn main() {
    let num = 42
    let result = process(num)
    print(result)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `num`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let num = 42\n    let result = process(num)\n", "    let result = process(42)\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_variable_with_side_effects"
kind = "code_action"
position = [6, 8]
source = '''
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    print(x)
    print(x)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "absent", path = ["result"], value = { title = "Inline local `x`" }, null_is_empty = true }]

[[tests]]
name = "inline_side_effecting_initializer_with_one_use"
kind = "code_action"
position = [6, 8]
source = '''
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    print(x)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `x`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let x = get_value()\n    print(x)\n", "    print(get_value())\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "no_inline_of_a_side_effecting_initializer_used_in_a_loop"
kind = "code_action"
position = [6, 8]
source = '''
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    for i in 0..3 {
        print(x + i)
    }
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "absent", path = ["result"], value = { title = "Inline local `x`" }, null_is_empty = true }]

[[tests]]
name = "no_inline_of_a_side_effecting_initializer_used_in_a_closure"
kind = "code_action"
position = [6, 8]
source = '''
fn get_value() -> int {
    print("Getting value")
    return 42
}

fn main() {
    let x = get_value()
    let f = () => x
    print(f())
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "absent", path = ["result"], value = { title = "Inline local `x`" }, null_is_empty = true }]

[[tests]]
name = "no_inline_of_an_indexing_initializer_with_two_uses"
kind = "code_action"
position = [2, 8]
source = '''
fn main() {
    let xs = [1, 2, 3]
    let x = xs[0]
    print(x)
    print(x)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "absent", path = ["result"], value = { title = "Inline local `x`" }, null_is_empty = true }]

[[tests]]
name = "inline_pure_field_arithmetic_with_two_uses"
kind = "code_action"
position = [7, 8]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let p = Point { x: 1, y: 2 }
    let s = p.x + p.y
    print(s)
    print(s)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `s`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let s = p.x + p.y\n    print(s)\n    print(s)\n", "    print(p.x + p.y)\n    print(p.x + p.y)\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_variable_in_if_condition"
kind = "code_action"
position = [1, 8]
source = '''
fn main() {
    let condition = true
    if condition {
        print("Yes")
    }
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `condition`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let condition = true\n    if condition {\n", "    if true {\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_string_variable"
kind = "code_action"
position = [1, 8]
source = '''
fn main() {
    let message = "Hello, World!"
    print(message)
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `message`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let message = \"Hello, World!\"\n    print(message)\n", "    print(\"Hello, World!\")\n"]] }, encoding = "utf16", trim = "end" }]

[[tests]]
name = "inline_variable_nested_scope"
kind = "code_action"
position = [1, 8]
source = '''
fn main() {
    let outer = 10
    {
        let inner = outer + 5
        print(inner)
    }
}'''
request = { transport = "rpc", method = "textDocument/codeAction", params = { textDocument = { uri = "$uri" }, range = { start = "$position", end = "$position" }, context = { diagnostics = [] } } }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result"], selector = { title = "Inline local `outer`" }, file = "$primary", source = "primary", expected = { from_fixture = true, replace = [["    let outer = 10\n", ""], ["        let inner = outer + 5\n", "        let inner = 10 + 5\n"]] }, encoding = "utf16", trim = "end" }]
