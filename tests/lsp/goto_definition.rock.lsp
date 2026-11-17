TEST: goto_definition_function
TYPE: goto_definition
---
fn calculate(x: int, y: int) -> int {
    return x + y
}

fn main() {
    let result = calculate(5, 3)
}
---
POSITION: line=5, character=17
EXPECTED:
file: main.rock
line: 0
character: 3
===

TEST: goto_definition_variable
TYPE: goto_definition
---
fn main() {
    let number = 42
    let doubled = number * 2
    print(doubled)
}
---
POSITION: line=2, character=18
EXPECTED:
file: main.rock
line: 1
character: 8
===

TEST: goto_definition_struct_field
TYPE: goto_definition
---
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let coordinate = p.x
}
---
POSITION: line=7, character=23
EXPECTED:
file: main.rock
line: 1
character: 4
===

TEST: goto_definition_struct_type
TYPE: goto_definition
---
struct Vector {
    x: float,
    y: float,
}

fn create_vector() -> Vector {
    return Vector { x: 0.0, y: 0.0 }
}
---
POSITION: line=5, character=22
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: goto_definition_parameter
TYPE: goto_definition
---
fn process(data: string, count: int) -> string {
    let result = data + count.to_string()
    return result
}
---
POSITION: line=1, character=17
EXPECTED:
file: main.rock
line: 0
character: 11
===

TEST: goto_definition_local_variable
TYPE: goto_definition
---
fn calculate() {
    let base_value = 10
    let multiplier = 3
    let result = base_value * multiplier
    print(result)
}
---
POSITION: line=3, character=17
EXPECTED:
file: main.rock
line: 1
character: 8
===

TEST: goto_def_shorthand_x_value
TYPE: goto_definition
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100
    let y = 200

    // Goto definition on 'x' value part (should go to variable)
    let p = Point { «cursor»x, y }
}
---
EXPECTED:
file: main.rock
line: 6
character: 8
===

TEST: goto_def_shorthand_y_value
TYPE: goto_definition
SOURCE: goto_def_shorthand_x_value
REPLACE: ["    // Goto definition on 'x' value part (should go to variable)", "    // Goto definition on 'y' value part"]
POSITION: line=10, character=23
EXPECTED:
file: main.rock
line: 7
character: 8
===

TEST: goto_def_mixed_shorthand_x
TYPE: goto_definition
---
struct Point {
    x: int
    y: int
}

fn main() {
    let x = 100

    // Mixed shorthand - goto def on 'x'
    let p = Point { «cursor»x, y: 200 }
}
---
EXPECTED:
file: main.rock
line: 6
character: 8
===

TEST: goto_def_struct_literal_name
TYPE: goto_definition
---
struct Player {
    name: string
    health: int
}

fn main() {
    let player = «cursor»Player { name: "Hero", health: 100 }
}
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: goto_def_struct_literal_name_assignment
TYPE: goto_definition
---
struct Enemy {
    kind: string
}

fn main() {
    let e: Enemy = «cursor»Enemy { kind: "goblin" }
}
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: goto_def_generic_struct_literal
TYPE: goto_definition
---
struct Box<T> {
    item: T
}

fn main() {
    let b = «cursor»Box { item: 42 }
}
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: goto_def_enum_record_variant
TYPE: goto_definition
---
enum Shape {
    Circle { radius: float },
    Rect { w: float, h: float },
}

fn main() {
    let s = Shape::«cursor»Circle { radius: 5.0 }
}
---
EXPECTED:
file: main.rock
line: 1
character: 4
===

TEST: goto_def_function_call
TYPE: goto_definition
---
fn double(x: int) -> int {
    return x * 2
}

fn main() {
    let x = 5
    let y = «cursor»double(x)
}
---
EXPECTED:
file: main.rock
line: 0
character: 3
===

TEST: goto_def_for_loop_iterable
TYPE: goto_definition
---
fn main() {
    let items = [1, 2, 3]
    for x in «cursor»items {
        print(x)
    }
}
---
EXPECTED:
file: main.rock
line: 1
character: 8
===

TEST: goto_def_closure_captured_var
TYPE: goto_definition
---
fn main() {
    let base = 10
    let f = (x: int) => x + «cursor»base
}
---
EXPECTED:
file: main.rock
line: 1
character: 8
===

TEST: goto_def_nested_field_access
TYPE: goto_definition
---
struct Inner {
    value: int
}

struct Outer {
    inner: Inner
}

fn main() {
    let o = Outer { inner: Inner { value: 42 } }
    let v = o.inner.«cursor»value
}
---
EXPECTED:
file: main.rock
line: 1
character: 4
===

TEST: goto_def_enum_simple_variant
TYPE: goto_definition
---
enum Color {
    Red,
    Green,
    Blue,
}

fn main() {
    let c = Color::«cursor»Red
}
---
EXPECTED:
file: main.rock
line: 1
character: 4
===

TEST: goto_def_enum_qualified_variant
TYPE: goto_definition
---
enum Direction {
    Up,
    Down,
}

fn main() {
    let d = Direction::«cursor»Down
}
---
EXPECTED:
file: main.rock
line: 2
character: 4
===

TEST: goto_def_enum_qualifier_path_segment
TYPE: goto_definition
---
enum Color { Red, Green }
fn f() -> Color {
    Color::Red
}
---
POSITION: line=2, character=4
EXPECTED:
file: main.rock
line: 0
character: 5
===

TEST: goto_def_global_variable_assignment
TYPE: goto_definition
---
global counter: int = 0

fn main() {
    «cursor»counter = 42
}
---
EXPECTED:
file: main.rock
line: 0
character: 7
===

TEST: goto_def_struct_field_in_literal
TYPE: goto_definition
---
struct Point {
    x: float,
    y: float,
}

fn main() {
    let p = Point { «cursor»x: 1.0, y: 2.0 }
}
---
EXPECTED:
file: main.rock
line: 1
character: 4
===

TEST: goto_def_field_ref_key
TYPE: goto_definition
---
struct Mob { health: int, attack_co: int? }
fn kill_co<T>(o: T, f: FieldRef<T, int?>) {}
fn r(mob: Mob) {
    kill_co(mob, .attack_co)
}
---
POSITION: line=3, character=19
EXPECTED:
file: main.rock
line: 0
character: 26
===

TEST: goto_def_builtin_coroutine_jumps_to_stdlib_resume_landmark
TYPE: goto_definition
---
co fn producer() -> «cursor»Coroutine<int> {
    yield 42
}
---
EXPECTED:
file: unified.rock
===

TEST: goto_def_keyword_yield_returns_empty
TYPE: goto_definition
SOURCE: goto_def_builtin_coroutine_jumps_to_stdlib_resume_landmark
POSITION: line=1, character=4
EXPECTED:
none
===

TEST: goto_def_primitive_type_returns_empty
TYPE: goto_definition
---
fn make() -> Vec<«cursor»int> {
    return vec_new()
}
---
EXPECTED:
none
===

TEST: goto_def_builtin_map_jumps_to_stdlib_map_new
TYPE: goto_definition
---
fn make() -> «cursor»Map<string, int> {
    return map_new()
}
---
EXPECTED:
file: unified.rock
===

TEST: goto_def_builtin_coroutine_jumps_to_stdlib_resume
TYPE: goto_definition
---
fn use_coroutine(c: «cursor»Coroutine<int>) -> int? {
    return resume(c)
}
---
EXPECTED:
file: unified.rock
===

TEST: goto_def_builtin_vec_jumps_to_stdlib_vec_struct
TYPE: goto_definition
SOURCE: goto_def_primitive_type_returns_empty
POSITION: line=0, character=13
EXPECTED:
file: unified.rock
===

TEST: goto_def_builtin_native_buffer_jumps_to_stdlib
TYPE: goto_definition
---
fn make() -> «cursor»NativeBuffer<int> {
    return native_buffer_int(10)
}
---
EXPECTED:
file: unified.rock
===

TEST: goto_def_builtin_native_buffer_u8_jumps_to_stdlib
TYPE: goto_definition
---
fn make() -> «cursor»NativeBufferU8 {
    return native_buffer_u8(10)
}
---
EXPECTED:
file: unified.rock
===

TEST: goto_def_builtin_primitive_int_returns_nothing
TYPE: goto_definition
---
fn make() -> «cursor»int {
    return 42
}
---
EXPECTED:
none
===

TEST: goto_def_record_variant_pattern_field_name
TYPE: goto_definition
---
enum Opt {
    Some { value: int },
    None,
}

fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { «cursor»value: v } => { print(v) },
        Opt::None => {},
    }
}
---
EXPECTED:
file: main.rock
line: 1
character: 11
===

TEST: goto_def_struct_pattern_field_name
TYPE: goto_definition
---
struct Point { x: int, y: int }

fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { «cursor»x: a, y: b } => { print(a + b) },
    }
}
---
EXPECTED:
file: main.rock
line: 0
character: 15
===

TEST: goto_definition_macro_invocation_name
TYPE: goto_definition
---
macro twice(x: expr) {
    let _ = x + x
}

fn main() {
    twice(41)
}
---
POSITION: line=5, character=6
EXPECTED:
file: main.rock
line: 0
character: 6
===

TEST: goto_definition_macro_overload_resolves_by_arity
TYPE: goto_definition
---
macro pick(a: expr) {
    let _ = a
}
macro pick(a: expr, b: expr) {
    let _ = a
    let _ = b
}
fn main() {
    pick(1, 2)
}
---
POSITION: line=8, character=5
EXPECTED:
file: main.rock
line: 3
character: 6
===
