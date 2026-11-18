[[tests]]
name = "type_function_call_char_by_char"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Player {
    health: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, name: "Hero" }

}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 7
col = 4
text = '''
print(p.health.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests]]
name = "type_new_function_char_by_char"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct FixVec2 {
    x: float,
    y: float,
}

fn main() {
    let a = FixVec2 { x: 1.0, y: 2.0 }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 5
col = 0
text = '''
fn length(v: FixVec2) -> float {
    return (v.x * v.x + v.y * v.y).sqrt()
}
'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests]]
name = "type_struct_field_then_use_it"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Enemy {
    health: int,
}

fn main() {
    let e = Enemy { health: 50 }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 2
col = 0
text = '''
    damage: int,'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 7
col = 0
text = '''
    print(e.damage.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests]]
name = "type_and_delete_cycles"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    let msg = greet("world")
    print(msg)
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 4
col = 0
text = '''
fn farewell(name: string) -> string {
    return "Bye " + name
}
'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests.steps]]
mode = "delete_at"
line = 7
col = 1
count = 36

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 5 }]

[[tests.steps]]
mode = "delete_at"
line = 6
col = 1
count = 32

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 5 }]

[[tests.steps]]
mode = "delete_at"
line = 5
col = 0
count = 1

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests]]
name = "type_match_expression_char_by_char"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn main() {
    let d = Direction::Up

}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 9
col = 0
text = '''
    match d {
        Direction::Up => print("up"),
        Direction::Down => print("down"),
        Direction::Left => print("left"),
        Direction::Right => print("right"),
    }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests]]
name = "type_long_expression_with_dots"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Pos {
    x: float,
    y: float,
}

struct Entity {
    pos: Pos,
    name: string,
}

fn main() {
    let e = Entity { pos: Pos { x: 1.0, y: 2.0 }, name: "test" }

}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests.steps]]
mode = "type_at"
line = 12
col = 4
text = '''
print(e.pos.x.to_string() + ", " + e.pos.y.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests]]
name = "type_in_middle_of_existing_code"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let result = add(1, 2)
    print(result.to_string())
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 4
col = 0
text = '''

fn multiply(a: int, b: int) -> int {
    return a * b
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests.steps]]
mode = "type_at"
line = 10
col = 0
text = '''
    let product = multiply(3, 4)
    print(product.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]
