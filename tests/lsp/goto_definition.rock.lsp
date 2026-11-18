[[tests]]
name = "goto_definition_function"
kind = "goto_definition"
position = [5, 17]
source = '''
fn calculate(x: int, y: int) -> int {
    return x + y
}

fn main() {
    let result = calculate(5, 3)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 3 }]

[[tests]]
name = "goto_definition_variable"
kind = "goto_definition"
position = [2, 18]
source = '''
fn main() {
    let number = 42
    let doubled = number * 2
    print(doubled)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_definition_struct_field"
kind = "goto_definition"
position = [7, 23]
source = '''
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let coordinate = p.x
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 4 }]

[[tests]]
name = "goto_definition_struct_type"
kind = "goto_definition"
position = [5, 22]
source = '''
struct Vector {
    x: float,
    y: float,
}

fn create_vector() -> Vector {
    return Vector { x: 0.0, y: 0.0 }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "goto_definition_parameter"
kind = "goto_definition"
position = [1, 17]
source = '''
fn process(data: string, count: int) -> string {
    let result = data + count.to_string()
    return result
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 11 }]

[[tests]]
name = "goto_definition_local_variable"
kind = "goto_definition"
position = [3, 17]
source = '''
fn calculate() {
    let base_value = 10
    let multiplier = 3
    let result = base_value * multiplier
    print(result)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_def_shorthand_x_value"
kind = "goto_definition"
position = [10, 20]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Goto definition on 'x' value part (should go to variable)
    let p = Point { x, y }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 6 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_def_shorthand_y_value"
kind = "goto_definition"
position = [10, 23]
source = { from_test = "goto_def_shorthand_x_value", replace = [["    // Goto definition on 'x' value part (should go to variable)", "    // Goto definition on 'y' value part"]] }
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 7 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_def_mixed_shorthand_x"
kind = "goto_definition"
position = [9, 20]
source = '''
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100

    // Mixed shorthand - goto def on 'x'
    let p = Point { x, y: 200 }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 6 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_def_struct_literal_name"
kind = "goto_definition"
position = [6, 17]
source = '''
struct Player {
    name: string
    health: int
}

fn main() {
    let player = Player { name: "Hero", health: 100 }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "goto_def_struct_literal_name_assignment"
kind = "goto_definition"
position = [5, 19]
source = '''
struct Enemy {
    kind: string
}

fn main() {
    let e: Enemy = Enemy { kind: "goblin" }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "goto_def_generic_struct_literal"
kind = "goto_definition"
position = [5, 12]
source = '''
struct Box<T> {
    item: T
}

fn main() {
    let b = Box { item: 42 }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "goto_def_enum_record_variant"
kind = "goto_definition"
position = [6, 19]
source = '''
enum Shape {
    Circle { radius: float },
    Rect { w: float, h: float },
}

fn main() {
    let s = Shape::Circle { radius: 5.0 }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 4 }]

[[tests]]
name = "goto_def_function_call"
kind = "goto_definition"
position = [6, 12]
source = '''
fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let x = 5
    let y = double(x)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 3 }]

[[tests]]
name = "goto_def_for_loop_iterable"
kind = "goto_definition"
position = [2, 13]
source = '''
fn main() {
    let items = [1, 2, 3]
    for x in items {
        print(x)
    }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_def_closure_captured_var"
kind = "goto_definition"
position = [2, 28]
source = '''
fn main() {
    let base = 10
    let f = (x: int) => x + base
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 8 }]

[[tests]]
name = "goto_def_nested_field_access"
kind = "goto_definition"
position = [10, 20]
source = '''
struct Inner {
    value: int
}

struct Outer {
    inner: Inner
}

fn main() {
    let o = Outer { inner: Inner { value: 42 } }
    let v = o.inner.value
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 4 }]

[[tests]]
name = "goto_def_enum_simple_variant"
kind = "goto_definition"
position = [7, 19]
source = '''
enum Color {
    Red,
    Green,
    Blue,
}

fn main() {
    let c = Color::Red
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 4 }]

[[tests]]
name = "goto_def_enum_qualified_variant"
kind = "goto_definition"
position = [6, 23]
source = '''
enum Direction {
    Up,
    Down,
}

fn main() {
    let d = Direction::Down
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 2 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 4 }]

[[tests]]
name = "goto_def_enum_qualifier_path_segment"
kind = "goto_definition"
position = [2, 4]
source = '''
enum Color { Red, Green }
fn f() -> Color {
    Color::Red
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 5 }]

[[tests]]
name = "goto_def_global_variable_assignment"
kind = "goto_definition"
position = [3, 4]
source = '''
global counter: int = 0

fn main() {
    counter = 42
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 7 }]

[[tests]]
name = "goto_def_struct_field_in_literal"
kind = "goto_definition"
position = [6, 20]
source = '''
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 4 }]

[[tests]]
name = "goto_def_field_ref_key"
kind = "goto_definition"
position = [3, 19]
source = '''
struct Mob { health: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    kill_co(mob, .attack_co)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 26 }]

[[tests]]
name = "goto_def_builtin_coroutine_jumps_to_stdlib_resume_landmark"
kind = "goto_definition"
position = [0, 20]
source = '''
co fn producer() -> Coroutine<int> {
    yield 42
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "goto_def_keyword_yield_returns_empty"
kind = "goto_definition"
position = [1, 4]
source = { from_test = "goto_def_builtin_coroutine_jumps_to_stdlib_resume_landmark" }
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "empty", path = ["result", "definitions"] }]

[[tests]]
name = "goto_def_primitive_type_returns_empty"
kind = "goto_definition"
position = [0, 17]
source = '''
fn make() -> Vec<int> {
    return vec_new()
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "empty", path = ["result", "definitions"] }]

[[tests]]
name = "goto_def_builtin_map_jumps_to_stdlib_map_new"
kind = "goto_definition"
position = [0, 13]
source = '''
fn make() -> Map<string, int> {
    return map_new()
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "goto_def_builtin_coroutine_jumps_to_stdlib_resume"
kind = "goto_definition"
position = [0, 20]
source = '''
fn use_coroutine(c: Coroutine<int>) -> int? {
    return resume(c)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "goto_def_builtin_vec_jumps_to_stdlib_vec_struct"
kind = "goto_definition"
position = [0, 13]
source = { from_test = "goto_def_primitive_type_returns_empty" }
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "goto_def_builtin_native_buffer_jumps_to_stdlib"
kind = "goto_definition"
position = [0, 13]
source = '''
fn make() -> NativeBuffer<int> {
    return native_buffer_int(10)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "goto_def_builtin_native_buffer_u8_jumps_to_stdlib"
kind = "goto_definition"
position = [0, 13]
source = '''
fn make() -> NativeBufferU8 {
    return native_buffer_u8(10)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "gte", path = ["result", "definitions"], value = 1 }]

[[tests]]
name = "goto_def_builtin_primitive_int_returns_nothing"
kind = "goto_definition"
position = [0, 13]
source = '''
fn make() -> int {
    return 42
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "empty", path = ["result", "definitions"] }]

[[tests]]
name = "goto_def_record_variant_pattern_field_name"
kind = "goto_definition"
position = [8, 20]
source = '''
enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { value: v } => { print(v) },
        Opt::None => {},
    }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 1 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 11 }]

[[tests]]
name = "goto_def_struct_pattern_field_name"
kind = "goto_definition"
position = [5, 16]
source = '''
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { x: a, y: b } => { print(a + b) },
    }
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 15 }]

[[tests]]
name = "goto_definition_macro_invocation_name"
kind = "goto_definition"
position = [5, 6]
source = '''
macro twice(x: expr) {
    let _ = x + x
}

fn main() {
    twice(41)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 0 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 6 }]

[[tests]]
name = "goto_definition_macro_overload_resolves_by_arity"
kind = "goto_definition"
position = [8, 5]
source = '''
macro pick(a: expr) {
    let _ = a
}
macro pick(a: expr, b: expr) {
    let _ = a
    let _ = b
}
fn main() {
    pick(1, 2)
}'''
request = { transport = "cli", argv = ["definition", "$target"] }
checks = [{ op = "equals", path = ["result", "definitions", 0, "range", "start", "line"], value = 3 }, { op = "equals", path = ["result", "definitions", 0, "range", "start", "character"], value = 6 }]
