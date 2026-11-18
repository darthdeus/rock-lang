[[tests]]
name = "document_symbols_basic"
kind = "document_symbols"
source = '''
struct Point { x: float, y: float }
enum Status { Pending, InProgress, Completed }
fn calculate(a: int, b: int) -> int { a + b }
fn main() { let p = Point { x: 0.0, y: 0.0 }; let s = Status::Pending }'''
request = { transport = "cli", argv = ["document-symbols", "$file"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "Point", kind = 23 }, { name = "Status", kind = 10 }, { name = "calculate", kind = 12 }, { name = "main", kind = 12 }] }]

[[tests]]
name = "document_symbols_associated_functions"
kind = "document_symbols"
source = '''
struct Rectangle { width: float, height: float }
fn area(rect: Rectangle) -> float { rect.width * rect.height }
fn perimeter(rect: Rectangle) -> float { 2.0 * (rect.width + rect.height) }
fn create_square(size: float) -> Rectangle { Rectangle { width: size, height: size } }'''
request = { transport = "cli", argv = ["document-symbols", "$file"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "Rectangle", kind = 23 }, { name = "area", kind = 12 }, { name = "perimeter", kind = 12 }, { name = "create_square", kind = 12 }] }]

[[tests]]
name = "document_symbols_enum_payload_types"
kind = "document_symbols"
source = '''
struct Color { r: int, g: int, b: int }
enum Shape { Circle(float), Rectangle(float, float), Triangle(Point, Point, Point) }
struct Point { x: float, y: float }
fn draw_shape(shape: Shape) {}'''
request = { transport = "cli", argv = ["document-symbols", "$file"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "Color", kind = 23 }, { name = "Shape", kind = 10 }, { name = "Point", kind = 23 }, { name = "draw_shape", kind = 12 }] }]

[[tests]]
name = "document_symbols_generics"
kind = "document_symbols"
source = '''
struct Container<T> { items: [T] }
fn new_container<T>() -> Container<T> { Container { items: [] } }
fn push_item<T>(container: Container<T>, item: T) { container.items.push(item) }
fn pop_item<T>(container: Container<T>) -> T? { container.items.pop() }
fn process<T, U>(input: T, mapper: fn(T) -> U) -> U { mapper(input) }'''
request = { transport = "cli", argv = ["document-symbols", "$file"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "Container", kind = 23 }, { name = "new_container", kind = 12 }, { name = "push_item", kind = 12 }, { name = "pop_item", kind = 12 }, { name = "process", kind = 12 }] }]

[[tests]]
name = "document_symbols_receiver_parameters"
kind = "document_symbols"
source = '''
struct Point { x: float, y: float }
struct Rectangle { x: float, y: float, width: float, height: float }
struct Circle { center: Point, radius: float }
fn draw(self: Circle) {}
fn bounds(self: Circle) -> Rectangle {
    Rectangle { x: self.center.x - self.radius, y: self.center.y - self.radius, width: self.radius * 2.0, height: self.radius * 2.0 }
}'''
request = { transport = "cli", argv = ["document-symbols", "$file"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "Point", kind = 23 }, { name = "Rectangle", kind = 23 }, { name = "Circle", kind = 23 }, { name = "draw", kind = 12 }, { name = "bounds", kind = 12 }] }]

[[tests]]
name = "document_symbols_constants"
kind = "document_symbols"
source = '''
const MAX_SIZE: int = 100
const DEFAULT_NAME: string = "Unknown"
const PI: float = 3.14159
fn calculate_area(radius: float) -> float { PI * radius * radius }
fn get_default_counter() -> int { 0 }'''
request = { transport = "cli", argv = ["document-symbols", "$file"] }
checks = [{ op = "contains_all", path = ["result", "symbols"], value = [{ name = "MAX_SIZE", kind = 14 }, { name = "DEFAULT_NAME", kind = 14 }, { name = "PI", kind = 14 }, { name = "calculate_area", kind = 12 }, { name = "get_default_counter", kind = 12 }] }]
