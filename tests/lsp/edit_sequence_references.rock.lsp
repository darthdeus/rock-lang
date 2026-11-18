[[tests]]
name = "references_shift_after_typing_and_removing_line_in_other_fn"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 5, character = 8 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 5, character = 8 }, { line = 6, character = 11 }, { line = 6, character = 19 }] }]

[[tests.steps]]
mode = "type_at"
line = 1
col = 12
text = '''

    let pad = 0'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 6, character = 8 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 6, character = 8 }, { line = 7, character = 11 }, { line = 7, character = 19 }] }]

[[tests.steps]]
mode = "delete_at"
line = 2
col = 15
count = 16

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 5, character = 8 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 5, character = 8 }, { line = 6, character = 11 }, { line = 6, character = 19 }] }]

[[tests]]
name = "enum_variant_references_shift_after_body_edit"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
enum Color {
    Red,
    Green,
}

fn alpha() -> int {
    return 1
}

fn beta(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
    }
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 11, character = 15 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 1, character = 4 }, { line = 11, character = 15 }] }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1, replace = [["    return 1\n", "    let pad = 0\n    return 1 + pad\n"]] }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 12, character = 15 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 1, character = 4 }, { line = 12, character = 15 }] }]

[[tests]]
name = "call_site_references_shift_after_edit_in_other_fn"
kind = "edit_sequence"

[[tests.steps]]
mode = "replace"
source = '''
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    return alpha() + alpha()
}'''

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 5, character = 12 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 0, character = 3 }, { line = 5, character = 11 }, { line = 5, character = 21 }] }]

[[tests.steps]]
mode = "replace"
source = { from_step = 1, replace = [["    return 1\n", "    let pad = 0\n    return 1 + pad\n"]] }

[[tests.steps.checks]]
request = { transport = "rpc", method = "textDocument/references", params = { textDocument = { uri = "$uri" }, position = { line = 6, character = 12 }, context = { includeDeclaration = true } } }
assertions = [{ op = "unordered_equal", path = ["result", "*", "range", "start"], value = [{ line = 0, character = 3 }, { line = 6, character = 11 }, { line = 6, character = 21 }] }]
