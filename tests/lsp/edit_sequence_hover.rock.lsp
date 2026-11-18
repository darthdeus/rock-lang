[[tests]]
name = "hover_iteratee_after_typing_for_loop"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/hover", params = { textDocument = { uri = "$uri" }, position = { line = 8, character = 5 } } }
assertions = [{ op = "contains", path = ["result", "contents"], value = "Mob" }]

[[tests.steps]]
mode = "type_at"
line = 9
col = 4
text = '''
for a in m.aggressors {
    print(a.dmg)
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/hover", params = { textDocument = { uri = "$uri" }, position = { line = 9, character = 18 } } }
assertions = [{ op = "contains", path = ["result", "contents"], value = "[AggressorEntry]" }]

[[tests]]
name = "hover_new_for_binding_after_typing_sentinel"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/hover", params = { textDocument = { uri = "$uri" }, position = { line = 8, character = 5 } } }
assertions = [{ op = "contains", path = ["result", "contents"], value = "Mob" }]

[[tests.steps]]
mode = "type_at"
line = 9
col = 4
text = '''
for a in m.aggressors {
    print(a.dmg)
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/hover", params = { textDocument = { uri = "$uri" }, position = { line = 9, character = 8 } } }
assertions = [{ op = "empty", path = ["result", "contents"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "hover_for_binding_fresh"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {
    for a in m.aggressors {
        print(a.dmg)
    }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/hover", params = { textDocument = { uri = "$uri" }, position = { line = 9, character = 8 } } }
assertions = [{ op = "contains", path = ["result", "contents"], value = "AggressorEntry" }]
