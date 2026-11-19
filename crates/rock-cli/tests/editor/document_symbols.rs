use super::*;

#[test]
fn document_symbols_basic() {
    let source = r#"struct Point { x: float, y: float }
enum Status { Pending, InProgress, Completed }
fn calculate(a: int, b: int) -> int { a + b }
fn main() { let p = Point { x: 0.0, y: 0.0 }; let s = Status::Pending }"#;
    let project = Project::single("document_symbols_basic", source);
    let reply = project.cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Point", "kind": 23}),
            json!({"name": "Status", "kind": 10}),
            json!({"name": "calculate", "kind": 12}),
            json!({"name": "main", "kind": 12}),
        ],
    );
}

#[test]
fn document_symbols_associated_functions() {
    let source = r#"struct Rectangle { width: float, height: float }
fn area(rect: Rectangle) -> float { rect.width * rect.height }
fn perimeter(rect: Rectangle) -> float { 2.0 * (rect.width + rect.height) }
fn create_square(size: float) -> Rectangle { Rectangle { width: size, height: size } }"#;
    let project = Project::single("document_symbols_associated_functions", source);
    let reply = project.cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Rectangle", "kind": 23}),
            json!({"name": "area", "kind": 12}),
            json!({"name": "perimeter", "kind": 12}),
            json!({"name": "create_square", "kind": 12}),
        ],
    );
}

#[test]
fn document_symbols_enum_payload_types() {
    let source = r#"struct Color { r: int, g: int, b: int }
enum Shape { Circle(float), Rectangle(float, float), Triangle(Point, Point, Point) }
struct Point { x: float, y: float }
fn draw_shape(shape: Shape) {}"#;
    let project = Project::single("document_symbols_enum_payload_types", source);
    let reply = project.cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Color", "kind": 23}),
            json!({"name": "Shape", "kind": 10}),
            json!({"name": "Point", "kind": 23}),
            json!({"name": "draw_shape", "kind": 12}),
        ],
    );
}

#[test]
fn document_symbols_generics() {
    let source = r#"struct Container<T> { items: [T] }
fn new_container<T>() -> Container<T> { Container { items: [] } }
fn push_item<T>(container: Container<T>, item: T) { container.items.push(item) }
fn pop_item<T>(container: Container<T>) -> T? { container.items.pop() }
fn process<T, U>(input: T, mapper: fn(T) -> U) -> U { mapper(input) }"#;
    let project = Project::single("document_symbols_generics", source);
    let reply = project.cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Container", "kind": 23}),
            json!({"name": "new_container", "kind": 12}),
            json!({"name": "push_item", "kind": 12}),
            json!({"name": "pop_item", "kind": 12}),
            json!({"name": "process", "kind": 12}),
        ],
    );
}

#[test]
fn document_symbols_receiver_parameters() {
    let source = r#"struct Point { x: float, y: float }
struct Rectangle { x: float, y: float, width: float, height: float }
struct Circle { center: Point, radius: float }
fn draw(self: Circle) {}
fn bounds(self: Circle) -> Rectangle {
    Rectangle { x: self.center.x - self.radius, y: self.center.y - self.radius, width: self.radius * 2.0, height: self.radius * 2.0 }
}"#;
    let project = Project::single("document_symbols_receiver_parameters", source);
    let reply = project.cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Point", "kind": 23}),
            json!({"name": "Rectangle", "kind": 23}),
            json!({"name": "Circle", "kind": 23}),
            json!({"name": "draw", "kind": 12}),
            json!({"name": "bounds", "kind": 12}),
        ],
    );
}

#[test]
fn document_symbols_constants() {
    let source = r#"const MAX_SIZE: int = 100
const DEFAULT_NAME: string = "Unknown"
const PI: float = 3.14159
fn calculate_area(radius: float) -> float { PI * radius * radius }
fn get_default_counter() -> int { 0 }"#;
    let project = Project::single("document_symbols_constants", source);
    let reply = project.cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "MAX_SIZE", "kind": 14}),
            json!({"name": "DEFAULT_NAME", "kind": 14}),
            json!({"name": "PI", "kind": 14}),
            json!({"name": "calculate_area", "kind": 12}),
            json!({"name": "get_default_counter", "kind": 12}),
        ],
    );
}
