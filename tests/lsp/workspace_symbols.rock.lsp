[[tests]]
name = "workspace_symbols_search_functions"
kind = "workspace_symbols"
source = '''
fn calculate_sum(a: int, b: int) -> int { a + b }
fn calculate_product(a: int, b: int) -> int { a * b }
fn process_data(data: string) -> string { data + "_processed" }
fn main() { let sum = calculate_sum(5, 3) }'''
request = { transport = "cli", argv = ["workspace-symbols", "$file", "calculate"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "calculate_sum", kind = 12 }, { name = "calculate_product", kind = 12 }] }]

[[tests]]
name = "workspace_symbols_search_types"
kind = "workspace_symbols"
source = '''
struct UserAccount { id: int, name: string }
struct UserProfile { account: UserAccount, bio: string }
enum UserStatus { Active, Inactive, Banned }
fn get_user_by_id(id: int) -> UserAccount { UserAccount { id: id, name: "User" } }'''
request = { transport = "cli", argv = ["workspace-symbols", "$file", "User"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "UserAccount", kind = 23 }, { name = "UserProfile", kind = 23 }, { name = "UserStatus", kind = 10 }] }]

[[tests]]
name = "workspace_symbols_partial_match"
kind = "workspace_symbols"
source = '''
struct Request { data: string }
struct Response { data: string }
struct Handler { name: string }
fn handle_request(req: Request) {}
fn handle_response(resp: Response) {}
fn process_handler(handler: Handler) {}'''
request = { transport = "cli", argv = ["workspace-symbols", "$file", "hand"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "handle_request", kind = 12 }, { name = "handle_response", kind = 12 }, { name = "Handler", kind = 23 }] }]

[[tests]]
name = "workspace_symbols_case_insensitive"
kind = "workspace_symbols"
source = '''
struct Point { x: float, y: float }
fn create_point(x: float, y: float) -> Point { Point { x: x, y: y } }
fn POINT_DISTANCE(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    dx * dx + dy * dy
}'''
request = { transport = "cli", argv = ["workspace-symbols", "$file", "point"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "Point", kind = 23 }, { name = "create_point", kind = 12 }, { name = "POINT_DISTANCE", kind = 12 }] }]

[[tests]]
name = "workspace_symbols_empty_query"
kind = "workspace_symbols"
source = '''
fn foo() {}
struct Bar { value: int }
enum Baz { One, Two }'''
request = { transport = "cli", argv = ["workspace-symbols", "$file", ""] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "foo", kind = 12 }, { name = "Bar", kind = 23 }, { name = "Baz", kind = 10 }] }]

[[tests]]
name = "workspace_symbols_unfiltered_mixed_kinds"
kind = "workspace_symbols"
source = '''
fn math_add(a: int, b: int) -> int { a + b }
struct MathVector { x: float, y: float }
struct GraphicsColor { r: int, g: int, b: int }
fn graphics_draw_pixel(x: int, y: int, color: GraphicsColor) {}
fn main() { let v = MathVector { x: 1.0, y: 2.0 } }'''
request = { transport = "cli", argv = ["workspace-symbols", "$file", ""] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "math_add", kind = 12 }, { name = "MathVector", kind = 23 }, { name = "GraphicsColor", kind = 23 }, { name = "graphics_draw_pixel", kind = 12 }, { name = "main", kind = 12 }] }]
