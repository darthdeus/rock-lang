[[tests]]
name = "tokens_survive_field_addition"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Player {
    health: int,
}

fn main() {
    let p = Player { health: 100 }
    print(p.health.to_string())
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1, replace = [["    health: int,\n", "    health: int,\n    name: string,\n"], ["    let p = Player { health: 100 }\n", "    let p = Player { health: 100, name: \"Hero\" }\n"], ["    print(p.health.to_string())\n", "    print(p.health.to_string())\n    print(p.name)\n"]] }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1 }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests]]
name = "tokens_recover_after_syntax_error"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Enemy {
    health: int,
    damage: int,
}

fn attack(e: Enemy) -> int {
    return e.damage
}

fn main() {
    let e = Enemy { health: 50, damage: 10 }
    let d = attack(e)
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1, replace = [["    let d = attack(e)", "    let d = attack("]] }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 5 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1 }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests]]
name = "tokens_survive_function_lifecycle"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    print(greet("world"))
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = '''
fn greet(name: string) -> string {
    return "Hello " + name
}

fn farewell(name: string) -> string {
    return "Goodbye " + name
}

fn main() {
    print(greet("world"))
    print(farewell("world"))
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 2, replace = [["fn greet(name: string) -> string {\n    return \"Hello \" + name\n}\n\n", ""], ["    print(greet(\"world\"))\n", ""]] }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests]]
name = "tokens_survive_rapid_edits"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn add(a: int, b: int) -> int { return a + b }
fn main() { let x = add(1, 2) }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = '''
fn add(a: int, b: int) -> int { return a + b }
fn sub(a: int, b: int) -> int { return a - b }
fn main() { let x = add(1, 2) + sub(3, 1) }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = '''
fn add(a: int, b: int) -> int { return a + b }
fn sub(a: int, b: int) -> int { return a - b }
fn mul(a: int, b: int) -> int { return a * b }
fn main() { let x = mul(add(1, 2), sub(3, 1)) }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = '''
fn mul(a: int, b: int) -> int { return a * b }
fn main() { let x = mul(2, 3) }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = '''
fn mul(a: int, b: int) -> int { return a * b }
fn div(a: int, b: int) -> int { return a / b }
fn main() { let x = div(mul(6, 2), 3) }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests]]
name = "tokens_after_type_change"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct FixVec2 {
    x: float,
    y: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]

[[tests.steps]]
mode = "replace"
source = '''
struct FixVec2 {
    x: float,
    y: float,
}

struct FixVec3 {
    x: float,
    y: float,
    z: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}

fn distance3d(a: FixVec3, b: FixVec3) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    let dz = b.z - a.z
    return (dx * dx + dy * dy + dz * dz).sqrt()
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 75 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1 }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 40 }]
