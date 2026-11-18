[[tests]]
name = "type_def_local_binding_to_struct"
kind = "goto_type_definition"
position = [9, 8]
source = '''
struct Mob {
    health: int
}

fn spawn() -> Mob {
    return Mob { health: 100 }
}

fn main() {
    let mob = spawn()
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "type_def_variable_use_site"
kind = "goto_type_definition"
position = [10, 16]
source = '''
struct Mob {
    health: int
}

fn spawn() -> Mob {
    return Mob { health: 100 }
}

fn main() {
    let mob = spawn()
    let other = mob
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "type_def_field_access_type"
kind = "goto_type_definition"
position = [5, 14]
source = '''
struct Inner { value: int }
struct Outer { inner: Inner }

fn main() {
    let o = Outer { inner: Inner { value: 1 } }
    let i = o.inner
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "type_def_nullable_peeled_to_struct"
kind = "goto_type_definition"
position = [9, 12]
source = '''
struct Mob {
    health: int
}

fn find() -> Mob? {
    return null
}

fn main() {
    let m = find()
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "type_def_enum_value"
kind = "goto_type_definition"
position = [3, 8]
source = '''
enum Color { Red, Green, Blue }

fn main() {
    let c = Color::Red
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 5 }]

[[tests]]
name = "type_def_vec_builtin_jumps_to_stdlib"
kind = "goto_type_definition"
position = [1, 8]
source = '''
fn main() {
    let xs = [1, 2, 3]
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "type_def_primitive_returns_none"
kind = "goto_type_definition"
position = [1, 8]
source = '''
fn main() {
    let n = 42
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "empty", path = ["result", "definitions"] }]

[[tests]]
name = "type_def_coroutine_jumps_to_yield_type"
kind = "goto_type_definition"
position = [7, 8]
source = '''
struct CoFrame { wait: bool }

co fn maker() -> Coroutine<CoFrame> {
    yield CoFrame { wait: true }
}

fn main() {
    let c = maker()
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "type_def_coroutine_of_primitive_returns_none"
kind = "goto_type_definition"
position = [5, 8]
source = '''
co fn nums() -> Coroutine<int> {
    yield 1
}

fn main() {
    let c = nums()
}'''
request = { transport = "cli", argv = ["type-definition", "$target"] }
checks = [{ op = "empty", path = ["result", "definitions"] }]
