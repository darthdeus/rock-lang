TEST: complete_struct_fields
TYPE: completion
---
struct Point {
    x: int,
    y: int,
}

fn main() {
    let p = Point { x: 10, y: 20 }
    p.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- x: int
- y: int
===

TEST: complete_method_calls
TYPE: completion
---
struct Rectangle {
    width: float,
    height: float,
}

fn main() {
    let rect = Rectangle { width: 10.0, height: 20.0 }
    rect.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- width: float
- height: float
===


TEST: complete_local_variables
TYPE: completion
---
fn calculate() {
    let first_number = 42
    let second_number = 84
    let result = first_number + second_number
    
    let sum = fi«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- first_number
===

TEST: complete_function_parameters
TYPE: completion
---
fn process(input_data: string, max_length: int, verbose: bool) {
    if ve«cursor» == true {
        print("debug")
    }
}
---
OPERATION: completion
---
EXPECTED:
- verbose
===

TEST: complete_function_names
TYPE: completion
---
fn sqrt(x: float) -> float {
    return x * x
}

fn abs(x: float) -> float {
    if x < 0.0 {
        return -x
    }
    return x
}

fn main() {
    let x = sq«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- sqrt
===

TEST: complete_struct_fields_in_invalid_if
TYPE: completion
---
struct Player {
    position_x: float,
    position_y: float,
    health: float,
}

fn foo(player: Player) {
    if player.pos«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- position_x: float
- position_y: float
===

TEST: complete_in_incomplete_function_call
TYPE: completion
---
struct Vector3 {
    x: float,
    y: float,
    z: float,
}

fn process(v: Vector3) {
    print(v.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
- z: float
===

TEST: complete_with_missing_bracket
TYPE: completion
---
struct Config {
    debug_mode: bool,
    max_connections: int,
}

fn setup() {
    let cfg = Config { debug_mode: true, max_connections: 100 }
    if cfg.debug«cursor»
        print("Debug enabled")
    }
}
---
OPERATION: completion
---
EXPECTED:
- debug_mode: bool
===

TEST: complete_local_vars_with_syntax_error
TYPE: completion
---
fn calculate() {
    let first_value = 10
    let second_value = 20
    let invalid_syntax = 
    
    let result = fi«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- first_value
===

TEST: complete_method_in_broken_expression
TYPE: completion
---
struct Database {
    connection_count: int,
    is_connected: bool,
}

fn check_status(db: Database) {
    let status = db.«cursor» && some_undefined_var
}
---
OPERATION: completion
---
EXPECTED:
- connection_count: int
- is_connected: bool
===

TEST: complete_nested_field_access_incomplete
TYPE: completion
---
struct Point {
    x: float,
    y: float,
}

struct Line {
    start: Point,
    end: Point,
}

fn get_distance(line: Line) {
    let dx = line.start.«cursor»
    // Missing closing brace and rest of function
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
===

TEST: complete_in_unclosed_string
TYPE: completion
---
struct Config {
    host: string,
    port: int,
    debug: bool,
}

fn setup(cfg: Config) {
    let message = "Host: " + cfg.«cursor»
    // String concatenation incomplete
---
OPERATION: completion
---
EXPECTED:
- host: string
- port: int
- debug: bool
===

TEST: complete_in_incomplete_array_literal
TYPE: completion
---
struct Item {
    id: int,
    name: string,
    price: float,
}

fn process_items(item: Item) {
    let items = [item.«cursor»
    // Array literal not closed
---
OPERATION: completion
---
EXPECTED:
- id: int
- name: string
- price: float
===

TEST: complete_in_incomplete_return
TYPE: completion
---
struct Result {
    value: int,
    success: bool,
    error: string,
}

fn compute() -> Result {
    let res = Result { value: 42, success: true, error: "" }
    return res.«cursor»
    // Function ends abruptly
---
OPERATION: completion
---
EXPECTED:
- value: int
- success: bool
- error: string
===

TEST: complete_in_incomplete_match_arm
TYPE: completion
---
enum Status {
    Active,
    Inactive,
    Pending,
}

struct User {
    name: string,
    status: Status,
    age: int,
}

fn check_user(user: User) {
    match user.status {
        Status::Active => user.«cursor»
        // Missing other arms and closing brace
---
OPERATION: completion
---
EXPECTED:
- name: string
- status: Status
- age: int
===

TEST: complete_in_incomplete_while_condition
TYPE: completion
---
struct Counter {
    value: int,
    max: int,
    step: int,
}

fn count(counter: Counter) {
    while counter.«cursor»
    // While loop body missing
---
OPERATION: completion
---
EXPECTED:
- value: int
- max: int
- step: int
===

TEST: complete_in_incomplete_for_loop
TYPE: completion
---
struct Range {
    start: int,
    stop: int,
    items: [int],
}

fn iterate(range: Range) {
    for i in range.«cursor»
    // For loop body missing
---
OPERATION: completion
---
EXPECTED:
- start: int
- stop: int
- items: [int]
===

TEST: complete_after_operator_no_space
TYPE: completion
---
struct Vector {
    x: float,
    y: float,
    z: float,
}

fn calculate(v1: Vector, v2: Vector) {
    let sum = v1.x+v2.«cursor»
    // No space after operator
}
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
- z: float
===

TEST: complete_in_nested_incomplete_if
TYPE: completion
---
struct Game {
    player_x: float,
    player_y: float,
    enemy_x: float,
    enemy_y: float,
}

fn update(game: Game) {
    if game.player_x > 100 {
        if game.«cursor»
        // Nested if incomplete
---
OPERATION: completion
---
EXPECTED:
- player_x: float
- player_y: float
- enemy_x: float
- enemy_y: float
===

TEST: complete_in_incomplete_struct_literal
TYPE: completion
---
struct Position {
    x: float,
    y: float,
}

struct CompletionEntity {
    pos: Position,
    health: int,
    name: string,
}

fn create(entity: CompletionEntity) {
    let new_entity = CompletionEntity {
        pos: entity.«cursor»
        // Struct literal incomplete
---
OPERATION: completion
---
EXPECTED:
- health: int
- name: string
===

TEST: complete_after_comma_no_space
TYPE: completion
---
struct Data {
    first: int,
    second: int,
    third: int,
}

fn process(d: Data) {
    do_something(d.first,d.«cursor»
    // No space after comma, function call incomplete
---
OPERATION: completion
---
EXPECTED:
- first: int
- second: int
- third: int
===

TEST: complete_for_loop_var_fields
TYPE: completion
---
struct Item {
    name: string
    count: int
}

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        let x = item.«cursor»
    }
}
---
OPERATION: completion
---
EXPECTED:
- name: string
- count: int
===

TEST: complete_lambda_param_fields
TYPE: completion
---
struct Enemy {
    health: int
    name: string
}

fn main() {
    let enemies: [Enemy] = vec_new()
    let alive = enemies.filter((e: Enemy) => e.«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- health: int
- name: string
===

TEST: complete_nested_struct_fields
TYPE: completion
---
struct FixVec2 {
    x: float
    y: float
}

struct Player {
    pos: FixVec2
    name: string
}

fn main() {
    let p = Player { pos: FixVec2 { x: 0.0, y: 0.0 }, name: "Hero" }
    let x = p.pos.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- x: float
- y: float
===

TEST: complete_global_var_fields
TYPE: completion
---
struct GameState {
    score: int
    level: int
}

global state: GameState = GameState { score: 0, level: 1 }

fn main() {
    let s = state.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- score: int
- level: int
===

TEST: complete_nullable_safe_nav
TYPE: completion
---
struct Inner {
    value: int
    name: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o: Outer? = null
    let x = o?.inner?.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- value: int
- name: string
===

TEST: complete_enum_qualified
TYPE: completion
---
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn main() {
    let d = Direction::«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- Up
- Down
- Left
- Right
===

TEST: complete_enum_qualified_with_prefix
TYPE: completion
---
enum AiState {
    Wander,
    Chase,
    Flee,
}

fn main() {
    let s = AiState::Wan«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- Wander
===

TEST: complete_generic_struct_fields
TYPE: completion
---
struct Pair<A, B> {
    first: A,
    second: B,
}

fn main() {
    let p = Pair { first: 1, second: "hi" }
    let x = p.«cursor»
}
---
OPERATION: completion
---
EXPECTED:
- first: int
- second: string
===

TEST: complete_in_match_arm_body
TYPE: completion
---
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => p.«cursor»,
        Opt::None => {},
    }
}
---
OPERATION: completion
---
EXPECTED:
- x: int
- y: int
===

TEST: complete_in_match_arm_braced_body
TYPE: completion
---
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => { p.«cursor» },
        Opt::None => {},
    }
}
---
OPERATION: completion
---
EXPECTED:
- x: int
- y: int
===

TEST: complete_in_match_arm_without_trailing_comma
TYPE: completion
---
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => p.«cursor»
        Opt::None => {},
    }
}
---
OPERATION: completion
---
EXPECTED:
- x: int
- y: int
===

TEST: complete_enum_record_variant_fields
TYPE: completion
---
enum Effect {
    Bandage { duration: float, heal_amount: int },
    Other(int),
}

fn main() {
    let e = Effect::Bandage { «cursor» }
}
---
OPERATION: completion
---
EXPECTED:
- duration: float
- heal_amount: int
===

TEST: complete_enum_record_variant_field_with_prefix
TYPE: completion
---
enum Effect {
    Bandage { duration: float, heal_amount: int },
    Other(int),
}

fn main() {
    let e = Effect::Bandage { dura«cursor» }
}
---
OPERATION: completion
---
EXPECTED:
- duration: float
===
