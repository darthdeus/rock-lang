use super::*;

#[test]
fn complete_struct_fields() {
    let source = r#"struct Point {
    x: int,
    y: int,
}

fn main() {
    let p = Point { x: 10, y: 20 }
    p.
}"#;
    let project = Project::single("complete_struct_fields", source);
    let reply = project.cli("completion", Some((7, 6)), &[]);
    completion_labels(&reply, &["x: int", "y: int"]);
}

#[test]
fn complete_method_calls() {
    let source = r#"struct Rectangle {
    width: float,
    height: float,
}

fn main() {
    let rect = Rectangle { width: 10.0, height: 20.0 }
    rect.
}"#;
    let project = Project::single("complete_method_calls", source);
    let reply = project.cli("completion", Some((7, 9)), &[]);
    completion_labels(&reply, &["width: float", "height: float"]);
}

#[test]
fn complete_local_variables() {
    let source = "fn calculate() {\n    let first_number = 42\n    let second_number = 84\n    let result = first_number + second_number\n    \n    let sum = fi\n}";
    let project = Project::single("complete_local_variables", source);
    let reply = project.cli("completion", Some((5, 16)), &[]);
    completion_labels(&reply, &["first_number"]);
}

#[test]
fn complete_function_parameters() {
    let source = r#"fn process(input_data: string, max_length: int, verbose: bool) {
    if ve == true {
        print("debug")
    }
}"#;
    let project = Project::single("complete_function_parameters", source);
    let reply = project.cli("completion", Some((1, 9)), &[]);
    completion_labels(&reply, &["verbose"]);
}

#[test]
fn complete_function_names() {
    let source = r#"fn sqrt(x: float) -> float {
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
}"#;
    let project = Project::single("complete_function_names", source);
    let reply = project.cli("completion", Some((12, 14)), &[]);
    completion_labels(&reply, &["sqrt"]);
}

#[test]
fn complete_struct_fields_in_invalid_if() {
    let source = r#"struct Player {
    position_x: float,
    position_y: float,
    health: float,
}

fn foo(player: Player) {
    if player.pos
}"#;
    let project = Project::single("complete_struct_fields_in_invalid_if", source);
    let reply = project.cli("completion", Some((7, 17)), &[]);
    completion_labels(&reply, &["position_x: float", "position_y: float"]);
}

#[test]
fn complete_in_incomplete_function_call() {
    let source = r#"struct Vector3 {
    x: float,
    y: float,
    z: float,
}

fn process(v: Vector3) {
    print(v.
}"#;
    let project = Project::single("complete_in_incomplete_function_call", source);
    let reply = project.cli("completion", Some((7, 12)), &[]);
    completion_labels(&reply, &["x: float", "y: float", "z: float"]);
}

#[test]
fn complete_with_missing_bracket() {
    let source = r#"struct Config {
    debug_mode: bool,
    max_connections: int,
}

fn setup() {
    let cfg = Config { debug_mode: true, max_connections: 100 }
    if cfg.debug
        print("Debug enabled")
    }
}"#;
    let project = Project::single("complete_with_missing_bracket", source);
    let reply = project.cli("completion", Some((7, 16)), &[]);
    completion_labels(&reply, &["debug_mode: bool"]);
}

#[test]
fn complete_local_vars_with_syntax_error() {
    let source = "fn calculate() {\n    let first_value = 10\n    let second_value = 20\n    let invalid_syntax = \n    \n    let result = fi\n}";
    let project = Project::single("complete_local_vars_with_syntax_error", source);
    let reply = project.cli("completion", Some((5, 19)), &[]);
    completion_labels(&reply, &["first_value"]);
}

#[test]
fn complete_method_in_broken_expression() {
    let source = r#"struct Database {
    connection_count: int,
    is_connected: bool,
}

fn check_status(db: Database) {
    let status = db. && some_undefined_var
}"#;
    let project = Project::single("complete_method_in_broken_expression", source);
    let reply = project.cli("completion", Some((6, 20)), &[]);
    completion_labels(&reply, &["connection_count: int", "is_connected: bool"]);
}

#[test]
fn complete_nested_field_access_incomplete() {
    let source = r#"struct Point {
    x: float,
    y: float,
}

struct Line {
    start: Point,
    end: Point,
}

fn get_distance(line: Line) {
    let dx = line.start.
    // Missing closing brace and rest of function"#;
    let project = Project::single("complete_nested_field_access_incomplete", source);
    let reply = project.cli("completion", Some((11, 24)), &[]);
    completion_labels(&reply, &["x: float", "y: float"]);
}

#[test]
fn complete_in_unclosed_string() {
    let source = r#"struct Config {
    host: string,
    port: int,
    debug: bool,
}

fn setup(cfg: Config) {
    let message = "Host: " + cfg.
    // String concatenation incomplete"#;
    let project = Project::single("complete_in_unclosed_string", source);
    let reply = project.cli("completion", Some((7, 33)), &[]);
    completion_labels(&reply, &["host: string", "port: int", "debug: bool"]);
}

#[test]
fn complete_in_incomplete_array_literal() {
    let source = r#"struct Item {
    id: int,
    name: string,
    price: float,
}

fn process_items(item: Item) {
    let items = [item.
    // Array literal not closed"#;
    let project = Project::single("complete_in_incomplete_array_literal", source);
    let reply = project.cli("completion", Some((7, 22)), &[]);
    completion_labels(&reply, &["id: int", "name: string", "price: float"]);
}

#[test]
fn complete_in_incomplete_return() {
    let source = r#"struct Result {
    value: int,
    success: bool,
    error: string,
}

fn compute() -> Result {
    let res = Result { value: 42, success: true, error: "" }
    return res.
    // Function ends abruptly"#;
    let project = Project::single("complete_in_incomplete_return", source);
    let reply = project.cli("completion", Some((8, 15)), &[]);
    completion_labels(&reply, &["value: int", "success: bool", "error: string"]);
}

#[test]
fn complete_in_incomplete_match_arm() {
    let source = r#"enum Status {
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
        // Missing other arms and closing brace"#;
    let project = Project::single("complete_in_incomplete_match_arm", source);
    let reply = project.cli("completion", Some((14, 31)), &[]);
    completion_labels(&reply, &["name: string", "status: Status", "age: int"]);
}

#[test]
fn complete_in_incomplete_while_condition() {
    let source = r#"struct Counter {
    value: int,
    max: int,
    step: int,
}

fn count(counter: Counter) {
    while counter.
    // While loop body missing"#;
    let project = Project::single("complete_in_incomplete_while_condition", source);
    let reply = project.cli("completion", Some((7, 18)), &[]);
    completion_labels(&reply, &["value: int", "max: int", "step: int"]);
}

#[test]
fn complete_in_incomplete_for_loop() {
    let source = r#"struct Range {
    start: int,
    stop: int,
    items: [int],
}

fn iterate(range: Range) {
    for i in range.
    // For loop body missing"#;
    let project = Project::single("complete_in_incomplete_for_loop", source);
    let reply = project.cli("completion", Some((7, 19)), &[]);
    completion_labels(&reply, &["start: int", "stop: int", "items: [int]"]);
}

#[test]
fn complete_after_operator_no_space() {
    let source = r#"struct Vector {
    x: float,
    y: float,
    z: float,
}

fn calculate(v1: Vector, v2: Vector) {
    let sum = v1.x+v2.
    // No space after operator
}"#;
    let project = Project::single("complete_after_operator_no_space", source);
    let reply = project.cli("completion", Some((7, 22)), &[]);
    completion_labels(&reply, &["x: float", "y: float", "z: float"]);
}

#[test]
fn complete_in_nested_incomplete_if() {
    let source = r#"struct Game {
    player_x: float,
    player_y: float,
    enemy_x: float,
    enemy_y: float,
}

fn update(game: Game) {
    if game.player_x > 100 {
        if game.
        // Nested if incomplete"#;
    let project = Project::single("complete_in_nested_incomplete_if", source);
    let reply = project.cli("completion", Some((9, 16)), &[]);
    completion_labels(
        &reply,
        &[
            "player_x: float",
            "player_y: float",
            "enemy_x: float",
            "enemy_y: float",
        ],
    );
}

#[test]
fn complete_in_incomplete_struct_literal() {
    let source = r#"struct Position {
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
        // Struct literal incomplete"#;
    let project = Project::single("complete_in_incomplete_struct_literal", source);
    let reply = project.cli("completion", Some((13, 20)), &[]);
    completion_labels(&reply, &["health: int", "name: string"]);
}

#[test]
fn complete_after_comma_no_space() {
    let source = r#"struct Data {
    first: int,
    second: int,
    third: int,
}

fn process(d: Data) {
    do_something(d.first,d.
    // No space after comma, function call incomplete"#;
    let project = Project::single("complete_after_comma_no_space", source);
    let reply = project.cli("completion", Some((7, 27)), &[]);
    completion_labels(&reply, &["first: int", "second: int", "third: int"]);
}

#[test]
fn complete_for_loop_var_fields() {
    let source = r#"struct Item {
    name: string
    count: int
}

fn main() {
    let items: [Item] = vec_new()
    for item in items {
        let x = item.
    }
}"#;
    let project = Project::single("complete_for_loop_var_fields", source);
    let reply = project.cli("completion", Some((8, 21)), &[]);
    completion_labels(&reply, &["name: string", "count: int"]);
}

#[test]
fn complete_lambda_param_fields() {
    let source = r#"struct Enemy {
    health: int
    name: string
}

fn main() {
    let enemies: [Enemy] = vec_new()
    let alive = enemies.filter((e: Enemy) => e.)
}"#;
    let project = Project::single("complete_lambda_param_fields", source);
    let reply = project.cli("completion", Some((7, 47)), &[]);
    completion_labels(&reply, &["health: int", "name: string"]);
}

#[test]
fn complete_nested_struct_fields() {
    let source = r#"struct FixVec2 {
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
}"#;
    let project = Project::single("complete_nested_struct_fields", source);
    let reply = project.cli("completion", Some((12, 18)), &[]);
    completion_labels(&reply, &["x: float", "y: float"]);
}

#[test]
fn complete_global_var_fields() {
    let source = r#"struct GameState {
    score: int
    level: int
}

global state: GameState = GameState { score: 0, level: 1 }

fn main() {
    let s = state.
}"#;
    let project = Project::single("complete_global_var_fields", source);
    let reply = project.cli("completion", Some((8, 18)), &[]);
    completion_labels(&reply, &["score: int", "level: int"]);
}

#[test]
fn complete_nullable_safe_nav() {
    let source = r#"struct Inner {
    value: int
    name: string
}

struct Outer {
    inner: Inner?
}

fn main() {
    let o: Outer? = null
    let x = o?.inner?.
}"#;
    let project = Project::single("complete_nullable_safe_nav", source);
    let reply = project.cli("completion", Some((11, 22)), &[]);
    completion_labels(&reply, &["value: int", "name: string"]);
}

#[test]
fn complete_enum_qualified() {
    let source = r#"enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn main() {
    let d = Direction::
}"#;
    let project = Project::single("complete_enum_qualified", source);
    let reply = project.cli("completion", Some((8, 23)), &[]);
    completion_labels(&reply, &["Up", "Down", "Left", "Right"]);
}

#[test]
fn complete_enum_qualified_with_prefix() {
    let source = r#"enum AiState {
    Wander,
    Chase,
    Flee,
}

fn main() {
    let s = AiState::Wan
}"#;
    let project = Project::single("complete_enum_qualified_with_prefix", source);
    let reply = project.cli("completion", Some((7, 24)), &[]);
    completion_labels(&reply, &["Wander"]);
}

#[test]
fn complete_generic_struct_fields() {
    let source = r#"struct Pair<A, B> {
    first: A,
    second: B,
}

fn main() {
    let p = Pair { first: 1, second: "hi" }
    let x = p.
}"#;
    let project = Project::single("complete_generic_struct_fields", source);
    let reply = project.cli("completion", Some((7, 14)), &[]);
    completion_labels(&reply, &["first: int", "second: string"]);
}

const COMPLETE_IN_MATCH_ARM_BODY: &str = r#"struct Point { x: int, y: int }

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
}"#;

#[test]
fn complete_in_match_arm_body() {
    let source = COMPLETE_IN_MATCH_ARM_BODY;
    let project = Project::single("complete_in_match_arm_body", source);
    let reply = project.cli("completion", Some((10, 26)), &[]);
    completion_labels(&reply, &["x: int", "y: int"]);
}

#[test]
fn complete_in_match_arm_braced_body() {
    let source = &COMPLETE_IN_MATCH_ARM_BODY.replace(
        "        Opt::Some(p) => p.,",
        "        Opt::Some(p) => { p. },",
    );
    let project = Project::single("complete_in_match_arm_braced_body", source);
    let reply = project.cli("completion", Some((10, 28)), &[]);
    completion_labels(&reply, &["x: int", "y: int"]);
}

#[test]
fn complete_in_match_arm_without_trailing_comma() {
    let source = &COMPLETE_IN_MATCH_ARM_BODY
        .replace("        Opt::Some(p) => p.,", "        Opt::Some(p) => p.");
    let project = Project::single("complete_in_match_arm_without_trailing_comma", source);
    let reply = project.cli("completion", Some((10, 26)), &[]);
    completion_labels(&reply, &["x: int", "y: int"]);
}

const COMPLETE_ENUM_RECORD_VARIANT_FIELDS: &str = r#"enum Effect {
    Bandage { duration: float, heal_amount: int },
    Other(int),
}

fn main() {
    let e = Effect::Bandage {  }
}"#;

#[test]
fn complete_enum_record_variant_fields() {
    let source = COMPLETE_ENUM_RECORD_VARIANT_FIELDS;
    let project = Project::single("complete_enum_record_variant_fields", source);
    let reply = project.cli("completion", Some((6, 30)), &[]);
    completion_labels(&reply, &["duration: float", "heal_amount: int"]);
}

#[test]
fn complete_enum_record_variant_field_with_prefix() {
    let source = &COMPLETE_ENUM_RECORD_VARIANT_FIELDS.replace(
        "    let e = Effect::Bandage {  }",
        "    let e = Effect::Bandage { dura }",
    );
    let project = Project::single("complete_enum_record_variant_field_with_prefix", source);
    let reply = project.cli("completion", Some((6, 34)), &[]);
    completion_labels(&reply, &["duration: float"]);
}

#[test]
fn complete_variant_in_let_else_pattern() {
    let source = r#"enum Shape {
    Circle(int),
    Square(int, int),
}

fn f(s: Shape) -> int {
    let Shape:: = s else {
        return 0
    }
    1
}"#;
    let project = Project::single("complete_variant_in_let_else_pattern", source);
    let reply = project.cli("completion", Some((6, 15)), &[]);
    completion_labels(&reply, &["Circle", "Square"]);
}
