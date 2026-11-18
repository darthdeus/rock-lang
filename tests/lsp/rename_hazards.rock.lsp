[[tests]]
name = "rename_param_from_caller_named_label"
kind = "rename"
position = [10, 13]
source = '''
fn spawn_it(count: int, size: float) {
    print(count)
    print(size)
}

fn spawn_it(count: string) {
    print(count)
}

fn main() {
    spawn_it(count: 3, size: 1.0)
    spawn_it(count: "a")
}'''
request = { transport = "cli", argv = ["rename", "$target", "amount"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = "fn spawn_it(amount: int, size: float) {\n    print(amount)\n    print(size)\n}\n\nfn spawn_it(count: string) {\n    print(count)\n}\n\nfn main() {\n    spawn_it(amount: 3, size: 1.0)\n    spawn_it(count: \"a\")\n}" }]

[[tests]]
name = "rename_param_from_caller_named_label_other_overload"
kind = "rename"
position = [11, 13]
source = { from_test = "rename_param_from_caller_named_label" }
request = { transport = "cli", argv = ["rename", "$target", "label"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = "fn spawn_it(count: int, size: float) {\n    print(count)\n    print(size)\n}\n\nfn spawn_it(label: string) {\n    print(label)\n}\n\nfn main() {\n    spawn_it(count: 3, size: 1.0)\n    spawn_it(label: \"a\")\n}" }]

[[tests]]
name = "rename_macro_name_from_call_refused"
kind = "rename"
position = [3, 10]
source = '''
macro double(x: expr) -> int { x + x }

fn main() {
    print(double(5))
}'''
request = { transport = "cli", argv = ["rename", "$target", "twice"] }
checks = [{ op = "contains", path = ["error", "message"], value = "cannot rename a macro" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_macro_name_from_decl_refused"
kind = "rename"
position = [0, 6]
source = { from_test = "rename_macro_name_from_call_refused" }
request = { transport = "cli", argv = ["rename", "$target", "twice"] }
checks = [{ op = "contains", path = ["error", "message"], value = "cannot rename a macro" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_local_used_through_caller_scope_macro_refused"
kind = "rename"
position = [3, 8]
source = '''
macro bump() { ^count = ^count + 1 }

fn main() {
    let count = 0
    bump()
    print(count)
}'''
request = { transport = "cli", argv = ["rename", "$target", "total"] }
checks = [{ op = "contains", path = ["error", "message"], value = ".rock:5 that rename cannot rewrite" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_global_used_through_macro_global_refused"
kind = "rename"
position = [0, 7]
source = '''
global SCORE: int = 0

macro bump() { _G.SCORE = _G.SCORE + 1 }

fn main() {
    bump()
    print(SCORE)
}'''
request = { transport = "cli", argv = ["rename", "$target", "POINTS"] }
checks = [{ op = "contains", path = ["error", "message"], value = ".rock:6 that rename cannot rewrite" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_macro_param"
kind = "rename"
position = [0, 13]
source = { from_test = "rename_macro_name_from_call_refused" }
request = { transport = "cli", argv = ["rename", "$target", "v"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["macro double(x: expr) -> int { x + x }", "macro double(v: expr) -> int { v + v }"]] } }]

[[tests]]
name = "rename_local_passed_as_macro_arg"
kind = "rename"
position = [3, 8]
source = '''
macro double(x: expr) -> int { x + x }

fn main() {
    let n = 5
    print(double(n))
}'''
request = { transport = "cli", argv = ["rename", "$target", "m"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    let n = 5", "    let m = 5"], ["    print(double(n))", "    print(double(m))"]] } }]

[[tests]]
name = "rename_fn_called_inside_macro_body"
kind = "rename"
position = [0, 3]
source = '''
fn helper(x: int) -> int {
    return x + 1
}

macro twice(x: expr) -> int { helper(helper(x)) }

fn main() {
    print(twice(1))
    print(helper(2))
}'''
request = { transport = "cli", argv = ["rename", "$target", "inc"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["helper", "inc"]] } }]

[[tests]]
name = "rename_inline_lua_param_refused"
kind = "rename"
position = [0, 21]
source = { from_test = "rename_macro_name_from_call_refused", replace = [["macro double(x: expr) -> int { x + x }", "inline(lua) fn probe(x: int) -> int = \"return x + 1\""], ["    print(double(5))", "    print(probe(1))"]] }
request = { transport = "cli", argv = ["rename", "$target", "y"] }
checks = [{ op = "contains", path = ["error", "message"], value = "cannot rename `x`: the opaque inline(lua) body of `probe` (" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_inline_js_param_refused"
kind = "rename"
position = [0, 20]
source = { from_test = "rename_inline_lua_param_refused", replace = [["inline(lua) fn probe(x: int) -> int = \"return x + 1\"", "inline(js) fn probe(x: int) -> int = \"return x + 1;\""]] }
request = { transport = "cli", argv = ["rename", "$target", "y"] }
checks = [{ op = "contains", path = ["error", "message"], value = "the opaque inline(js) body of `probe`" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_nomangle_fn_called_from_inline_body_refused"
kind = "rename"
position = [1, 3]
source = '''
@nomangle
fn helper(x: int) -> int {
    return x * 2
}

inline(lua) fn call_helper(x: int) -> int = "return helper(x)"

fn main() {
    print(call_helper(1))
}'''
request = { transport = "cli", argv = ["rename", "$target", "helper2"] }
checks = [{ op = "contains", path = ["error", "message"], value = "cannot rename `helper`: `@nomangle` keeps its emitted name" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_nomangle_fn_from_call_site_refused"
kind = "rename"
position = [6, 10]
source = '''
@nomangle
fn helper(x: int) -> int {
    return x * 2
}

fn main() {
    print(helper(1))
}'''
request = { transport = "cli", argv = ["rename", "$target", "helper2"] }
checks = [{ op = "contains", path = ["error", "message"], value = "`@nomangle` keeps its emitted name, which code outside Rock calls" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_field_read_by_inline_body_refused"
kind = "rename"
position = [1, 4]
source = '''
struct Mob {
    hp: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.hp"

fn main() {
    let m = Mob { hp: 4 }
    print(peek(m))
}'''
request = { transport = "cli", argv = ["rename", "$target", "health"] }
checks = [{ op = "contains", path = ["error", "message"], value = "the opaque inline(lua) body of `peek`" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_global_read_by_inline_body_refused"
kind = "rename"
position = [0, 7]
source = '''
global SCORE: int = 0

inline(lua) fn peek() -> int = "return SCORE"

fn main() {
    print(peek())
}'''
request = { transport = "cli", argv = ["rename", "$target", "POINTS"] }
checks = [{ op = "contains", path = ["error", "message"], value = "the opaque inline(lua) body of `peek`" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_struct_named_by_inline_type_tag_refused"
kind = "rename"
position = [0, 7]
source = '''
struct Mob {
    hp: int,
}

inline(lua) fn is_mob(v: any) -> bool = "return v.__ty == 'Mob'"

fn main() {
    print(is_mob(Mob { hp: 1 }))
}'''
request = { transport = "cli", argv = ["rename", "$target", "Beast"] }
checks = [{ op = "contains", path = ["error", "message"], value = "the opaque inline(lua) body of `is_mob`" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests]]
name = "rename_inline_fn_name_is_fine"
kind = "rename"
position = [0, 15]
source = { from_test = "rename_inline_lua_param_refused" }
request = { transport = "cli", argv = ["rename", "$target", "probe2"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["probe", "probe2"]] } }]

[[tests]]
name = "rename_field_not_named_by_any_inline_body"
kind = "rename"
position = [1, 4]
source = '''
struct Mob {
    hp: int,
    mana: int,
}

inline(lua) fn peek(m: Mob) -> int = "return m.mana"

fn main() {
    let m = Mob { hp: 4, mana: 2 }
    print(m.hp + peek(m))
}'''
request = { transport = "cli", argv = ["rename", "$target", "health"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["hp", "health"]] } }]

[[tests]]
name = "rename_param_sharing_a_word_with_another_inline_body"
kind = "rename"
position = [2, 9]
source = { from_test = "rename_inline_fn_name_is_fine", replace = [["fn main() {", "fn twice(x: int) -> int {"], ["    print(probe(1))", "    return probe(x) * 2"]] }
request = { transport = "cli", argv = ["rename", "$target", "n"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["fn twice(x: int) -> int {", "fn twice(n: int) -> int {"], ["    return probe(x) * 2", "    return probe(n) * 2"]] } }]

[[tests]]
name = "rename_fn_named_in_a_string_literal"
kind = "rename"
position = [0, 3]
source = '''
fn helper(x: int) -> int {
    return x * 2
}

fn main() {
    print("helper")
    print(helper(1))
}'''
request = { transport = "cli", argv = ["rename", "$target", "helper2"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["fn helper(x: int) -> int {", "fn helper2(x: int) -> int {"], ["    print(helper(1))", "    print(helper2(1))"]] } }]

[[tests]]
name = "rename_global_read_by_inline_body_in_another_file_refused"
kind = "rename"
position = [0, 7]
primary_file = "state.rock"
request = { transport = "cli", argv = ["rename", "$target", "POINTS"] }
checks = [{ op = "contains", path = ["error", "message"], value = "the opaque inline(lua) body of `peek` (main.rock:1)" }, { op = "empty", path = ["result", "changes", "*"], missing_is_empty = true, null_is_empty = true }]

[[tests.files]]
path = "state.rock"
text = '''
global SCORE: int = 0'''

[[tests.files]]
path = "main.rock"
text = '''
inline(lua) fn peek() -> int = "return SCORE"

fn main() {
    SCORE = 2
    print(peek())
}'''

[[tests.files]]
path = "stick.toml"
text = '''
[package]
name = "test"'''

[[tests]]
name = "rename_global_across_files_without_inline_reader"
kind = "rename"
position = [0, 7]
primary_file = "state.rock"
request = { transport = "cli", argv = ["rename", "$target", "POINTS"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "state.rock", expected = "global POINTS: int = 0" }, { op = "applied_text_equals", path = ["result", "changes"], file = "main.rock", expected = "inline(lua) fn peek() -> int = \"return 1\"\n\nfn main() {\n    POINTS = 2\n    print(peek())\n}" }]

[[tests.files]]
path = "state.rock"
text = '''
global SCORE: int = 0'''

[[tests.files]]
path = "main.rock"
text = '''
inline(lua) fn peek() -> int = "return 1"

fn main() {
    SCORE = 2
    print(peek())
}'''

[[tests.files]]
path = "stick.toml"
text = '''
[package]
name = "test"'''

[[tests]]
name = "rename_shadowing_inner_local"
kind = "rename"
position = [3, 12]
source = '''
fn main() {
    let x = 1
    if true {
        let x = 2
        print(x)
    }
    print(x)
}'''
request = { transport = "cli", argv = ["rename", "$target", "y"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["        let x = 2", "        let y = 2"], ["        print(x)", "        print(y)"]] } }]

[[tests]]
name = "rename_shadowed_local_in_same_block"
kind = "rename"
position = [1, 8]
source = '''
fn main() {
    let x = 1
    let x = x + 1
    print(x)
}'''
request = { transport = "cli", argv = ["rename", "$target", "y"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    let x = 1", "    let y = 1"], ["    let x = x + 1", "    let x = y + 1"]] } }]

[[tests]]
name = "rename_fn_from_ufcs_call"
kind = "rename"
position = [10, 12]
source = '''
struct Circle {
    radius: float,
}

fn area(c: Circle) -> float {
    return c.radius * c.radius
}

fn main() {
    let c = Circle { radius: 1.0 }
    print(c.area())
    print(area(c))
}'''
request = { transport = "cli", argv = ["rename", "$target", "size"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["area", "size"]] } }]

[[tests]]
name = "rename_receiver_form_fn"
kind = "rename"
position = [4, 9]
source = '''
struct Point {
    x: int,
}

fn Point.shift(p, dx: int) -> int {
    return p.x + dx
}

fn main() {
    let p = Point { x: 1 }
    print(p.shift(1))
    print(shift(p, 2))
}'''
request = { transport = "cli", argv = ["rename", "$target", "nudge"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["shift", "nudge"]] } }]

[[tests]]
name = "rename_field_leaves_same_named_field_of_other_struct"
kind = "rename"
position = [1, 4]
source = '''
struct A {
    hp: int,
}

struct B {
    hp: int,
}

fn main() {
    let a = A { hp: 1 }
    let b = B { hp: 2 }
    print(a.hp + b.hp)
}'''
request = { transport = "cli", argv = ["rename", "$target", "health"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = "struct A {\n    health: int,\n}\n\nstruct B {\n    hp: int,\n}\n\nfn main() {\n    let a = A { health: 1 }\n    let b = B { hp: 2 }\n    print(a.health + b.hp)\n}" }]

[[tests]]
name = "rename_local_inside_string_interpolation"
kind = "rename"
position = [1, 8]
source = '''
fn main() {
    let n = 5
    print("n = {n}")
}'''
request = { transport = "cli", argv = ["rename", "$target", "count"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    let n = 5", "    let count = 5"], ["    print(\"n = {n}\")", "    print(\"n = {count}\")"]] } }]

[[tests]]
name = "rename_lambda_param"
kind = "rename"
position = [1, 13]
source = '''
fn main() {
    let f = (a: int) => a + 1
    print(f(1))
}'''
request = { transport = "cli", argv = ["rename", "$target", "b"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["    let f = (a: int) => a + 1", "    let f = (b: int) => b + 1"]] } }]

[[tests]]
name = "rename_struct_generic_param_reaches_its_uses"
kind = "rename"
position = [0, 11]
source = '''
struct Box<T> {
    value: T,
}

fn main() {
    let b = Box { value: 1 }
    print(b.value)
}'''
request = { transport = "cli", argv = ["rename", "$target", "U"] }
checks = [{ op = "empty", path = ["error"], missing_is_empty = true, null_is_empty = true }, { op = "applied_text_equals", path = ["result", "changes"], file = "$primary", expected = { from_fixture = true, replace = [["T", "U"]] } }]
