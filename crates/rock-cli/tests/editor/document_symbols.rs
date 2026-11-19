use super::*;

#[test]
fn document_symbols_list_top_level_items() {
    let source = r#"struct Point { x: float, y: float }
struct Container<T> { items: [T] }
enum Shape { Circle(float), Triangle(Point, Point, Point) }
const MAX_SIZE: int = 100
fn push_item<T>(container: Container<T>, item: T) { container.items.push(item) }
fn bounds(self: Shape) -> Point { Point { x: 0.0, y: 0.0 } }
fn main() { let p = Point { x: 0.0, y: 0.0 }; let s = Shape::Circle(1.0) }"#;
    let reply = Project::single(source).cli("document-symbols", None, &[]);
    contains_items(
        &reply["symbols"],
        &[
            json!({"name": "Point", "kind": 23}),
            json!({"name": "Container", "kind": 23}),
            json!({"name": "Shape", "kind": 10}),
            json!({"name": "MAX_SIZE", "kind": 14}),
            json!({"name": "push_item", "kind": 12}),
            json!({"name": "bounds", "kind": 12}),
            json!({"name": "main", "kind": 12}),
        ],
    );
}
