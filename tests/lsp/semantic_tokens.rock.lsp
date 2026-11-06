TEST: semantic_tokens_basic
TYPE: semantic_tokens
---
// Test basic semantic tokens
struct Point {
    x: float,
    y: float,
}

fn distance(p1: Point, p2: Point) -> float {
    let dx = p2.x - p1.x
    let dy = p2.y - p1.y
    return (dx * dx + dy * dy).sqrt()
}

fn main() {
    let origin = Point { x: 0.0, y: 0.0 }
    let point = Point { x: 3.0, y: 4.0 }
    let dist = distance(origin, point)
    print("Distance: " + dist.to_string())
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 30
===

TEST: semantic_tokens_struct_highlight
TYPE: semantic_tokens
---
struct Color {
    red: int,
    green: int,
    blue: int,
}

struct Rectangle {
    width: float,
    height: float,
    color: Color,
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 10
===

TEST: semantic_tokens_enum_struct
TYPE: semantic_tokens
---
struct Shape {
    area: float
}

enum ShapeKind {
    Circle(radius: float),
    Rectangle(width: float, height: float),
    Triangle,
}

global MY_PI: float = 3.14159

fn process_shape(shape: ShapeKind) {
    match shape {
        ShapeKind::Circle(r) => print("Circle"),
        ShapeKind::Rectangle(w, h) => print("Rectangle"),
        ShapeKind::Triangle => print("Triangle"),
    }
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 5
===

TEST: semantic_tokens_nullable_types
TYPE: semantic_tokens
---
fn process_optional(value: int?) -> int {
    if value != null {
        return value + 10
    } else {
        return 0
    }
}

fn handle_string(text: string?) {
    let result = text ?? "default"
    print(result)
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 10
===

TEST: semantic_tokens_with_undefined_symbol
TYPE: semantic_tokens
---
struct Sprite {
    width: int,
    height: int,
}

fn draw_sprite(sprite: Sprite, pos: Vec2, rect: Rect) {
    // Drawing logic here
}

fn update_entities() {
    let sprite_frog = Sprite { width: 16, height: 16 }
    let pos = Vec2 { x: 100.0, y: 200.0 }

    // This should still generate semantic tokens for everything else
    // even though draw_ is undefined and incomplete
    for i in range(0, 10) {
        draw_sprite(sprite_frog, pos, rect(0, 0, 16, 16))
        draw_
    }

    // More valid code after the error
    let another_sprite = Sprite { width: 32, height: 32 }
    draw_sprite(another_sprite, pos, rect(0, 0, 32, 32))
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 20
===

TEST: semantic_tokens_incomplete_typing
TYPE: semantic_tokens
---
// Test that semantic tokens work while actively typing
struct Player {
    health: int,
    position: Vec2,
}

fn draw_player(p: Player) {
    // Implementation
}

fn draw_sprite_rotated(sprite: Sprite, angle: float) {
    // Implementation
}

fn game_loop() {
    let player = Player { health: 100, position: Vec2 { x: 0.0, y: 0.0 } }

    // User is in the middle of typing draw_sprite...
    draw_player(player)
    draw_spri

    // Should still have tokens for code after the incomplete line
    let health_bar = 100
    if player.health > 0 {
        draw_player(player)
    }
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 15
===
TEST: semantic_tokens_await_delegation
TYPE: semantic_tokens
---
co fn inner() -> Coroutine<int> {
    yield 1
}

co fn outer() -> Coroutine<int> {
    await inner()
    yield 3
}
---
OPERATION: full_document
---
EXPECTED:
min_tokens: 14
===
