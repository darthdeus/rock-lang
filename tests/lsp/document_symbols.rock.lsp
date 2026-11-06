TEST: document_symbols_basic
TYPE: document_symbols
---
struct Point {
    x: float,
    y: float,
}

enum Status {
    Pending,
    InProgress,
    Completed,
}

fn calculate(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let p = Point { x: 0.0, y: 0.0 }
    let s = Status::Pending
}
---
OPERATION: document_symbols
---
EXPECTED:
Point: Struct
Status: Enum
calculate: Function
main: Function
===

TEST: document_symbols_with_impl
TYPE: document_symbols
---
struct Rectangle {
    width: float,
    height: float,
}

fn area(rect: Rectangle) -> float {
    return rect.width * rect.height
}

fn perimeter(rect: Rectangle) -> float {
    return 2.0 * (rect.width + rect.height)
}

fn create_square(size: float) -> Rectangle {
    return Rectangle { width: size, height: size }
}
---
OPERATION: document_symbols
---
EXPECTED:
Rectangle: Struct
area: Function
perimeter: Function
create_square: Function
===

TEST: document_symbols_nested_types
TYPE: document_symbols
---
mod graphics {
    struct Color {
        r: u8,
        g: u8,
        b: u8,
    }

    enum Shape {
        Circle(float),
        Rectangle(float, float),
        Triangle([Point; 3]),
    }

    struct Point {
        x: float,
        y: float,
    }
}

fn draw_shape(shape: graphics::Shape) {
    // Draw implementation
}
---
OPERATION: document_symbols
---
EXPECTED:
graphics: Module
Color: Struct
Shape: Enum
Point: Struct
draw_shape: Function
===

TEST: document_symbols_generics
TYPE: document_symbols
---
struct Container<T> {
    items: [T],
}

fn new_container<T>() -> Container<T> {
    return Container { items: [] }
}

fn push_item<T>(container: Container<T>, item: T) {
    container.items.push(item)
}

fn pop_item<T>(container: Container<T>) -> T? {
    return container.items.pop()
}

fn process<T, U>(input: T, mapper: fn(T) -> U) -> U {
    return mapper(input)
}
---
OPERATION: document_symbols
---
EXPECTED:
Container: Struct
new_container: Function
push_item: Function
pop_item: Function
process: Function
===

TEST: document_symbols_traits
TYPE: document_symbols
---
struct Point {
    x: float,
    y: float,
}

struct Rectangle {
    x: float,
    y: float,
    width: float,
    height: float,
}

struct Circle {
    center: Point,
    radius: float,
}

// Rock doesn't support explicit trait implementations
// Methods are defined directly in the struct
fn draw(self: Circle) {
    // Drawing logic
}

fn bounds(self: Circle) -> Rectangle {
    return Rectangle {
        x: self.center.x - self.radius,
        y: self.center.y - self.radius,
        width: self.radius * 2.0,
        height: self.radius * 2.0,
    }
}
---
OPERATION: document_symbols
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

fn calculate_area(radius: float) -> float {
    return PI * radius * radius
}

fn get_default_counter() -> int {
    return 0
}
---
OPERATION: document_symbols
---
EXPECTED:
MAX_SIZE: Constant
DEFAULT_NAME: Constant
PI: Constant
calculate_area: Function
get_default_counter: Function
===