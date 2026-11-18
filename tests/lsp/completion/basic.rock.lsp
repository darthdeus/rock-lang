[[tests]]
name = "complete_struct_fields"
kind = "completion"
position = [7, 6]
source = '''
struct Point {
    x: int,
    y: int,
}

fn main() {
    let p = Point { x: 10, y: 20 }
    p.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: int", "y: int"] }]

[[tests]]
name = "complete_method_calls"
kind = "completion"
position = [7, 9]
source = '''
struct Rectangle {
    width: float,
    height: float,
}

fn main() {
    let rect = Rectangle { width: 10.0, height: 20.0 }
    rect.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["width: float", "height: float"] }]

[[tests]]
name = "complete_local_variables"
kind = "completion"
position = [5, 16]
source = '''
fn calculate() {
    let first_number = 42
    let second_number = 84
    let result = first_number + second_number
    
    let sum = fi
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["first_number"] }]

[[tests]]
name = "complete_function_parameters"
kind = "completion"
position = [1, 9]
source = '''
fn process(input_data: string, max_length: int, verbose: bool) {
    if ve == true {
        print("debug")
    }
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["verbose"] }]

[[tests]]
name = "complete_function_names"
kind = "completion"
position = [12, 14]
source = '''
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
    let x = sq
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["sqrt"] }]

[[tests]]
name = "complete_struct_fields_in_invalid_if"
kind = "completion"
position = [7, 17]
source = '''
struct Player {
    position_x: float,
    position_y: float,
    health: float,
}

fn foo(player: Player) {
    if player.pos
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["position_x: float", "position_y: float"] }]

[[tests]]
name = "complete_in_incomplete_function_call"
kind = "completion"
position = [7, 12]
source = '''
struct Vector3 {
    x: float,
    y: float,
    z: float,
}

fn process(v: Vector3) {
    print(v.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float", "z: float"] }]

[[tests]]
name = "complete_with_missing_bracket"
kind = "completion"
position = [7, 16]
source = '''
struct Config {
    debug_mode: bool,
    max_connections: int,
}

fn setup() {
    let cfg = Config { debug_mode: true, max_connections: 100 }
    if cfg.debug
        print("Debug enabled")
    }
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["debug_mode: bool"] }]

[[tests]]
name = "complete_local_vars_with_syntax_error"
kind = "completion"
position = [5, 19]
source = '''
fn calculate() {
    let first_value = 10
    let second_value = 20
    let invalid_syntax = 
    
    let result = fi
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["first_value"] }]

[[tests]]
name = "complete_method_in_broken_expression"
kind = "completion"
position = [6, 20]
source = '''
struct Database {
    connection_count: int,
    is_connected: bool,
}

fn check_status(db: Database) {
    let status = db. && some_undefined_var
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["connection_count: int", "is_connected: bool"] }]

[[tests]]
name = "complete_nested_field_access_incomplete"
kind = "completion"
position = [11, 24]
source = '''
struct Point {
    x: float,
    y: float,
}

struct Line {
    start: Point,
    end: Point,
}

fn get_distance(line: Line) {
    let dx = line.start.
    // Missing closing brace and rest of function'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float"] }]

[[tests]]
name = "complete_in_unclosed_string"
kind = "completion"
position = [7, 33]
source = '''
struct Config {
    host: string,
    port: int,
    debug: bool,
}

fn setup(cfg: Config) {
    let message = "Host: " + cfg.
    // String concatenation incomplete'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["host: string", "port: int", "debug: bool"] }]

[[tests]]
name = "complete_in_incomplete_array_literal"
kind = "completion"
position = [7, 22]
source = '''
struct Item {
    id: int,
    name: string,
    price: float,
}

fn process_items(item: Item) {
    let items = [item.
    // Array literal not closed'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["id: int", "name: string", "price: float"] }]

[[tests]]
name = "complete_in_incomplete_return"
kind = "completion"
position = [8, 15]
source = '''
struct Result {
    value: int,
    success: bool,
    error: string,
}

fn compute() -> Result {
    let res = Result { value: 42, success: true, error: "" }
    return res.
    // Function ends abruptly'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["value: int", "success: bool", "error: string"] }]

[[tests]]
name = "complete_in_incomplete_match_arm"
kind = "completion"
position = [14, 31]
source = '''
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
        Status::Active => user.
        // Missing other arms and closing brace'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["name: string", "status: Status", "age: int"] }]

[[tests]]
name = "complete_in_incomplete_while_condition"
kind = "completion"
position = [7, 18]
source = '''
struct Counter {
    value: int,
    max: int,
    step: int,
}

fn count(counter: Counter) {
    while counter.
    // While loop body missing'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["value: int", "max: int", "step: int"] }]

[[tests]]
name = "complete_in_incomplete_for_loop"
kind = "completion"
position = [7, 19]
source = '''
struct Range {
    start: int,
    stop: int,
    items: [int],
}

fn iterate(range: Range) {
    for i in range.
    // For loop body missing'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["start: int", "stop: int", "items: [int]"] }]

[[tests]]
name = "complete_after_operator_no_space"
kind = "completion"
position = [7, 22]
source = '''
struct Vector {
    x: float,
    y: float,
    z: float,
}

fn calculate(v1: Vector, v2: Vector) {
    let sum = v1.x+v2.
    // No space after operator
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float", "z: float"] }]

[[tests]]
name = "complete_in_nested_incomplete_if"
kind = "completion"
position = [9, 16]
source = '''
struct Game {
    player_x: float,
    player_y: float,
    enemy_x: float,
    enemy_y: float,
}

fn update(game: Game) {
    if game.player_x > 100 {
        if game.
        // Nested if incomplete'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["player_x: float", "player_y: float", "enemy_x: float", "enemy_y: float"] }]

[[tests]]
name = "complete_in_incomplete_struct_literal"
kind = "completion"
position = [13, 20]
source = '''
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
        pos: entity.
        // Struct literal incomplete'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["health: int", "name: string"] }]

[[tests]]
name = "complete_after_comma_no_space"
kind = "completion"
position = [7, 27]
source = '''
struct Data {
    first: int,
    second: int,
    third: int,
}

fn process(d: Data) {
    do_something(d.first,d.
    // No space after comma, function call incomplete'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["first: int", "second: int", "third: int"] }]

[[tests]]
name = "complete_for_loop_var_fields"
kind = "completion"
position = [8, 21]
source = '''
struct Item {
    name: string
    count: int
}

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        let x = item.
    }
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["name: string", "count: int"] }]

[[tests]]
name = "complete_lambda_param_fields"
kind = "completion"
position = [7, 47]
source = '''
struct Enemy {
    health: int
    name: string
}

fn main() {
    let enemies: [Enemy] = vec_new()
    let alive = enemies.filter((e: Enemy) => e.)
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["health: int", "name: string"] }]

[[tests]]
name = "complete_nested_struct_fields"
kind = "completion"
position = [12, 18]
source = '''
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
    let x = p.pos.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: float", "y: float"] }]

[[tests]]
name = "complete_global_var_fields"
kind = "completion"
position = [8, 18]
source = '''
struct GameState {
    score: int
    level: int
}

global state: GameState = GameState { score: 0, level: 1 }

fn main() {
    let s = state.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["score: int", "level: int"] }]

[[tests]]
name = "complete_nullable_safe_nav"
kind = "completion"
position = [11, 22]
source = '''
struct Inner {
    value: int
    name: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o: Outer? = null
    let x = o?.inner?.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["value: int", "name: string"] }]

[[tests]]
name = "complete_enum_qualified"
kind = "completion"
position = [8, 23]
source = '''
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn main() {
    let d = Direction::
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["Up", "Down", "Left", "Right"] }]

[[tests]]
name = "complete_enum_qualified_with_prefix"
kind = "completion"
position = [7, 24]
source = '''
enum AiState {
    Wander,
    Chase,
    Flee,
}

fn main() {
    let s = AiState::Wan
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["Wander"] }]

[[tests]]
name = "complete_generic_struct_fields"
kind = "completion"
position = [7, 14]
source = '''
struct Pair<A, B> {
    first: A,
    second: B,
}

fn main() {
    let p = Pair { first: 1, second: "hi" }
    let x = p.
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["first: int", "second: string"] }]

[[tests]]
name = "complete_in_match_arm_body"
kind = "completion"
position = [10, 26]
source = '''
struct Point { x: int, y: int }

enum Opt {
    None,
    Some(Point),
}

fn main() {
    let o = Opt::Some(Point { x: 1, y: 2 })
    match o {
        Opt::Some(p) => p.,
        Opt::None => {},
    }
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: int", "y: int"] }]

[[tests]]
name = "complete_in_match_arm_braced_body"
kind = "completion"
position = [10, 28]
source = { from_test = "complete_in_match_arm_body", replace = [["        Opt::Some(p) => p.,", "        Opt::Some(p) => { p. },"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: int", "y: int"] }]

[[tests]]
name = "complete_in_match_arm_without_trailing_comma"
kind = "completion"
position = [10, 26]
source = { from_test = "complete_in_match_arm_body", replace = [["        Opt::Some(p) => p.,", "        Opt::Some(p) => p."]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["x: int", "y: int"] }]

[[tests]]
name = "complete_enum_record_variant_fields"
kind = "completion"
position = [6, 30]
source = '''
enum Effect {
    Bandage { duration: float, heal_amount: int },
    Other(int),
}

fn main() {
    let e = Effect::Bandage {  }
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["duration: float", "heal_amount: int"] }]

[[tests]]
name = "complete_enum_record_variant_field_with_prefix"
kind = "completion"
position = [6, 34]
source = { from_test = "complete_enum_record_variant_fields", replace = [["    let e = Effect::Bandage {  }", "    let e = Effect::Bandage { dura }"]] }
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["duration: float"] }]

[[tests]]
name = "complete_variant_in_let_else_pattern"
kind = "completion"
position = [6, 15]
source = '''
enum Shape {
    Circle(int),
    Square(int, int),
}

fn f(s: Shape) -> int {
    let Shape:: = s else {
        return 0
    }
    1
}'''
request = { transport = "cli", argv = ["completion", "$target"] }
checks = [{ op = "contains_all", path = ["result", "completions", "*", "label"], value = ["Circle", "Square"] }]
