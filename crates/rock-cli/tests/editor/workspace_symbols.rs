use super::*;

#[test]
fn workspace_symbols_search_functions() {
    let source = r#"fn calculate_sum(a: int, b: int) -> int { a + b }
fn calculate_product(a: int, b: int) -> int { a * b }
fn process_data(data: string) -> string { data + "_processed" }
fn main() { let sum = calculate_sum(5, 3) }"#;
    let project = Project::single("workspace_symbols_search_functions", source);
    let reply = project.cli("workspace-symbols", None, &["calculate"]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "calculate_sum", "kind": 12}),
            json!({"name": "calculate_product", "kind": 12}),
        ],
    );
}

#[test]
fn workspace_symbols_search_types() {
    let source = r#"struct UserAccount { id: int, name: string }
struct UserProfile { account: UserAccount, bio: string }
enum UserStatus { Active, Inactive, Banned }
fn get_user_by_id(id: int) -> UserAccount { UserAccount { id: id, name: "User" } }"#;
    let project = Project::single("workspace_symbols_search_types", source);
    let reply = project.cli("workspace-symbols", None, &["User"]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "UserAccount", "kind": 23}),
            json!({"name": "UserProfile", "kind": 23}),
            json!({"name": "UserStatus", "kind": 10}),
        ],
    );
}

#[test]
fn workspace_symbols_partial_match() {
    let source = r#"struct Request { data: string }
struct Response { data: string }
struct Handler { name: string }
fn handle_request(req: Request) {}
fn handle_response(resp: Response) {}
fn process_handler(handler: Handler) {}"#;
    let project = Project::single("workspace_symbols_partial_match", source);
    let reply = project.cli("workspace-symbols", None, &["hand"]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "handle_request", "kind": 12}),
            json!({"name": "handle_response", "kind": 12}),
            json!({"name": "Handler", "kind": 23}),
        ],
    );
}

#[test]
fn workspace_symbols_case_insensitive() {
    let source = r#"struct Point { x: float, y: float }
fn create_point(x: float, y: float) -> Point { Point { x: x, y: y } }
fn POINT_DISTANCE(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    dx * dx + dy * dy
}"#;
    let project = Project::single("workspace_symbols_case_insensitive", source);
    let reply = project.cli("workspace-symbols", None, &["point"]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Point", "kind": 23}),
            json!({"name": "create_point", "kind": 12}),
            json!({"name": "POINT_DISTANCE", "kind": 12}),
        ],
    );
}

#[test]
fn workspace_symbols_empty_query() {
    let source = r#"fn foo() {}
struct Bar { value: int }
enum Baz { One, Two }"#;
    let project = Project::single("workspace_symbols_empty_query", source);
    let reply = project.cli("workspace-symbols", None, &[""]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "foo", "kind": 12}),
            json!({"name": "Bar", "kind": 23}),
            json!({"name": "Baz", "kind": 10}),
        ],
    );
}

#[test]
fn workspace_symbols_unfiltered_mixed_kinds() {
    let source = r#"fn math_add(a: int, b: int) -> int { a + b }
struct MathVector { x: float, y: float }
struct GraphicsColor { r: int, g: int, b: int }
fn graphics_draw_pixel(x: int, y: int, color: GraphicsColor) {}
fn main() { let v = MathVector { x: 1.0, y: 2.0 } }"#;
    let project = Project::single("workspace_symbols_unfiltered_mixed_kinds", source);
    let reply = project.cli("workspace-symbols", None, &[""]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "math_add", "kind": 12}),
            json!({"name": "MathVector", "kind": 23}),
            json!({"name": "GraphicsColor", "kind": 23}),
            json!({"name": "graphics_draw_pixel", "kind": 12}),
            json!({"name": "main", "kind": 12}),
        ],
    );
}
