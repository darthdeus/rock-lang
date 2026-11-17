TEST: document_symbols_basic
TYPE: document_symbols
---
struct Point { x: float, y: float }
enum Status { Pending, InProgress, Completed }
fn calculate(a: int, b: int) -> int { a + b }
fn main() { let p = Point { x: 0.0, y: 0.0 }; let s = Status::Pending }
---
EXPECTED:
Point: Struct
Status: Enum
calculate: Function
main: Function
===

TEST: document_symbols_associated_functions
TYPE: document_symbols
---
struct Rectangle { width: float, height: float }
fn area(rect: Rectangle) -> float { rect.width * rect.height }
fn perimeter(rect: Rectangle) -> float { 2.0 * (rect.width + rect.height) }
fn create_square(size: float) -> Rectangle { Rectangle { width: size, height: size } }
---
EXPECTED:
Rectangle: Struct
area: Function
perimeter: Function
create_square: Function
===

TEST: document_symbols_enum_payload_types
TYPE: document_symbols
---
struct Color { r: int, g: int, b: int }
enum Shape { Circle(float), Rectangle(float, float), Triangle(Point, Point, Point) }
struct Point { x: float, y: float }
fn draw_shape(shape: Shape) {}
---
EXPECTED:
Color: Struct
Shape: Enum
Point: Struct
draw_shape: Function
===

TEST: document_symbols_generics
TYPE: document_symbols
---
struct Container<T> { items: [T] }
fn new_container<T>() -> Container<T> { Container { items: [] } }
fn push_item<T>(container: Container<T>, item: T) { container.items.push(item) }
fn pop_item<T>(container: Container<T>) -> T? { container.items.pop() }
fn process<T, U>(input: T, mapper: fn(T) -> U) -> U { mapper(input) }
---
EXPECTED:
Container: Struct
new_container: Function
push_item: Function
pop_item: Function
process: Function
===

TEST: document_symbols_receiver_parameters
TYPE: document_symbols
---
struct Point { x: float, y: float }
struct Rectangle { x: float, y: float, width: float, height: float }
struct Circle { center: Point, radius: float }
fn draw(self: Circle) {}
fn bounds(self: Circle) -> Rectangle {
    Rectangle { x: self.center.x - self.radius, y: self.center.y - self.radius, width: self.radius * 2.0, height: self.radius * 2.0 }
}
---
EXPECTED:
Point: Struct
Rectangle: Struct
Circle: Struct
draw: Function
bounds: Function
===

TEST: document_symbols_constants
TYPE: document_symbols
---
const MAX_SIZE: int = 100
const DEFAULT_NAME: string = "Unknown"
const PI: float = 3.14159
fn calculate_area(radius: float) -> float { PI * radius * radius }
fn get_default_counter() -> int { 0 }
---
EXPECTED:
MAX_SIZE: Constant
DEFAULT_NAME: Constant
PI: Constant
calculate_area: Function
get_default_counter: Function
===