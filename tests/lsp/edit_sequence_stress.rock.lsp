[[tests]]
name = "stress_type_delete_type_cycle"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Player {
    health: int,
    mana: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, mana: 50, name: "Hero" }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 9
col = 0
text = '''

fn heal(p: Player, amount: int) -> Player {
    return Player { health: p.health + amount, mana: p.mana, name: p.name }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests.steps]]
mode = "type_at"
line = 13
col = 0
text = '''

fn cast_spell(p: Player, cost: int) -> Player {
    return Player { health: p.health, mana: p.mana - cost, name: p.name }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 75 }]

[[tests.steps]]
mode = "type_at"
line = 17
col = 0
text = '''

fn take_damage(p: Player, damage: int) -> Player {
    return Player { health: p.health - damage, mana: p.mana, name: p.name }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 100 }]

[[tests.steps]]
mode = "type_at"
line = 22
col = 4
text = '''
    let p = heal(p, 20)
    let p = cast_spell(p, 10)
    let p = take_damage(p, 30)
    print(p.health.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 100 }]

[[tests]]
name = "stress_syntax_error_recovery_cycle"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Item {
    name: string,
    value: int,
}

fn describe(item: Item) -> string {
    return item.name + ": " + item.value.to_string()
}

fn main() {
    let sword = Item { name: "Sword", value: 50 }
    print(describe(sword))
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests.steps]]
mode = "type_at"
line = 13
col = 0
text = '''
    let shield = Item { name: "Shield", valu'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 5 }]

[[tests.steps]]
mode = "type_at"
line = 13
col = 46
text = '''
e: 30 }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests.steps]]
mode = "type_at"
line = 14
col = 0
text = '''
    print(describe(shield))'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests.steps]]
mode = "type_at"
line = 15
col = 0
text = '''
    let potion = Item { name: "'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 5 }]

[[tests.steps]]
mode = "type_at"
line = 15
col = 32
text = '''
Potion", value: 10 }'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests.steps]]
mode = "type_at"
line = 16
col = 0
text = '''
    print(describe(potion))'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]

[[tests]]
name = "stress_many_small_edits"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn main() {
    let x = 1
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 2
col = 0
text = '''
    let y = 2'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 3
col = 0
text = '''
    let z = 3'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 4
col = 0
text = '''
    let a = x + y'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 5
col = 0
text = '''
    let b = y + z'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 6
col = 0
text = '''
    let c = a + b'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 7
col = 0
text = '''
    print(c.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "delete_at"
line = 7
col = 24
count = 24

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "delete_at"
line = 6
col = 18
count = 18

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 6
col = 0
text = '''
    let total = x + y + z + a + b'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests.steps]]
mode = "type_at"
line = 7
col = 0
text = '''
    print(total.to_string())'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 15 }]

[[tests]]
name = "stress_struct_evolution"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct Config {
    width: int,
}

fn main() {
    let c = Config { width: 800 }
    print(c.width.to_string())
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 2
col = 0
text = '''
    height: int,'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 3
col = 0
text = '''
    title: string,'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "type_at"
line = 4
col = 0
text = '''
    fullscreen: bool,'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 25 }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1, replace = [["    width: int,\n", "    width: int,\n    height: int,\n    title: string,\n    fullscreen: bool,\n"], ["    let c = Config { width: 800 }\n", "    let c = Config { width: 800, height: 600, title: \"Game\", fullscreen: false }\n"], ["    print(c.width.to_string())\n", "    print(c.width.to_string())\n    print(c.height.to_string())\n    print(c.title)\n"]] }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/semanticTokens/full", params = { textDocument = { uri = "$uri" } } }
assertions = [{ op = "gte", path = ["result", "data"], value = 50 }]
