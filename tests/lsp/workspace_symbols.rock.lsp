TEST: workspace_symbols_search_functions
TYPE: workspace_symbols
---
fn calculate_sum(a: int, b: int) -> int {
    return a + b
}

fn calculate_product(a: int, b: int) -> int {
    return a * b
}

fn process_data(data: string) -> string {
    // Process the data
    return data + "_processed"
}

fn main() {
    let sum = calculate_sum(5, 3)
}
---
OPERATION: workspace_symbols
QUERY: calculate
---
EXPECTED:
calculate_sum: Function
calculate_product: Function
===

TEST: workspace_symbols_search_types
TYPE: workspace_symbols
---
struct UserAccount {
    id: int,
    name: string,
}

struct UserProfile {
    account: UserAccount,
    bio: string,
}

enum UserStatus {
    Active,
    Inactive,
    Banned,
}

fn get_user_by_id(id: int) -> UserAccount {
    return UserAccount { id: id, name: "User" }
}
---
OPERATION: workspace_symbols
QUERY: User
---
EXPECTED:
UserAccount: Struct
UserProfile: Struct
UserStatus: Enum
===

TEST: workspace_symbols_partial_match
TYPE: workspace_symbols
---
struct Request {
    data: string,
}

struct Response {
    data: string,
}

struct Handler {
    name: string,
}

fn handle_request(req: Request) {
    // Handle
}

fn handle_response(resp: Response) {
    // Handle
}

fn process_handler(handler: Handler) {
    // Process
}
---
OPERATION: workspace_symbols
QUERY: hand
---
EXPECTED:
handle_request: Function
handle_response: Function
Handler: Struct
===

TEST: workspace_symbols_case_insensitive
TYPE: workspace_symbols
---
struct Point {
    x: float,
    y: float,
}

fn create_point(x: float, y: float) -> Point {
    return Point { x: x, y: y }
}

fn POINT_DISTANCE(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    // Calculate distance (sqrt not available in Rock)
    return dx * dx + dy * dy
}
---
OPERATION: workspace_symbols
QUERY: point
---
EXPECTED:
Point: Struct
create_point: Function
POINT_DISTANCE: Function
===

TEST: workspace_symbols_empty_query
TYPE: workspace_symbols
---
fn foo() {
    // Function foo
}

struct Bar {
    value: int,
}

enum Baz {
    One,
    Two,
}
---
OPERATION: workspace_symbols
QUERY:
---
EXPECTED:
foo: Function
Bar: Struct
Baz: Enum
===

TEST: workspace_symbols_cross_module
TYPE: workspace_symbols
---
// Rock doesn't have modules, all symbols are in the global namespace
fn math_add(a: int, b: int) -> int {
    return a + b
}

struct MathVector {
    x: float,
    y: float,
}

struct GraphicsColor {
    r: int,
    g: int,
    b: int,
}

fn graphics_draw_pixel(x: int, y: int, color: GraphicsColor) {
    // Draw
}

fn main() {
    let v = MathVector { x: 1.0, y: 2.0 }
}
---
OPERATION: workspace_symbols
QUERY:
---
EXPECTED:
math_add: Function
MathVector: Struct
GraphicsColor: Struct
graphics_draw_pixel: Function
main: Function
===