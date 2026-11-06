TEST: type_function_call_char_by_char
TYPE: edit_sequence
---
STEP 1:
struct Player {
    health: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, name: "Hero" }

}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 7:4):
print(p.health.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
===

TEST: type_new_function_char_by_char
TYPE: edit_sequence
---
STEP 1:
struct FixVec2 {
    x: float,
    y: float,
}

fn main() {
    let a = FixVec2 { x: 1.0, y: 2.0 }
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 5:0):
fn length(v: FixVec2) -> float {
    return (v.x * v.x + v.y * v.y).sqrt()
}

---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
===

TEST: type_struct_field_then_use_it
TYPE: edit_sequence
---
STEP 1:
struct Enemy {
    health: int,
}

fn main() {
    let e = Enemy { health: 50 }
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 2:0):
    damage: int,
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 3 (type at 7:0):
    print(e.damage.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
===

TEST: type_and_delete_cycles
TYPE: edit_sequence
---
STEP 1:
fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    let msg = greet("world")
    print(msg)
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 4:0):
fn farewell(name: string) -> string {
    return "Bye " + name
}

---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
---
STEP 3 (delete at 7:1, 36):
---
CHECK: semantic_tokens
EXPECTED:
non_zero
---
STEP 4 (delete at 6:1, 32):
---
CHECK: semantic_tokens
EXPECTED:
non_zero
---
STEP 5 (delete at 5:0, 1):
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
===

TEST: type_match_expression_char_by_char
TYPE: edit_sequence
---
STEP 1:
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

fn main() {
    let d = Direction::Up

}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 9:0):
    match d {
        Direction::Up => print("up"),
        Direction::Down => print("down"),
        Direction::Left => print("left"),
        Direction::Right => print("right"),
    }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
===

TEST: type_long_expression_with_dots
TYPE: edit_sequence
---
STEP 1:
struct Pos {
    x: float,
    y: float,
}

struct Entity {
    pos: Pos,
    name: string,
}

fn main() {
    let e = Entity { pos: Pos { x: 1.0, y: 2.0 }, name: "test" }

}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
---
STEP 2 (type at 12:4):
print(e.pos.x.to_string() + ", " + e.pos.y.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
===

TEST: type_in_middle_of_existing_code
TYPE: edit_sequence
---
STEP 1:
fn add(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let result = add(1, 2)
    print(result.to_string())
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 4:0):

fn multiply(a: int, b: int) -> int {
    return a * b
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
---
STEP 3 (type at 10:0):
    let product = multiply(3, 4)
    print(product.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
===
