TEST: tokens_survive_field_addition
TYPE: edit_sequence
---
STEP 1:
struct Player {
    health: int,
}

fn main() {
    let p = Player { health: 100 }
    print(p.health.to_string())
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2:
struct Player {
    health: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, name: "Hero" }
    print(p.health.to_string())
    print(p.name)
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 3:
struct Player {
    health: int,
}

fn main() {
    let p = Player { health: 100 }
    print(p.health.to_string())
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
===

TEST: tokens_recover_after_syntax_error
TYPE: edit_sequence
---
STEP 1:
struct Enemy {
    health: int,
    damage: int,
}

fn attack(e: Enemy) -> int {
    return e.damage
}

fn main() {
    let e = Enemy { health: 50, damage: 10 }
    let d = attack(e)
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
---
STEP 2:
struct Enemy {
    health: int,
    damage: int,
}

fn attack(e: Enemy) -> int {
    return e.damage
}

fn main() {
    let e = Enemy { health: 50, damage: 10 }
    let d = attack(
}
---
CHECK: semantic_tokens
EXPECTED:
non_zero
---
STEP 3:
struct Enemy {
    health: int,
    damage: int,
}

fn attack(e: Enemy) -> int {
    return e.damage
}

fn main() {
    let e = Enemy { health: 50, damage: 10 }
    let d = attack(e)
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
===

TEST: tokens_survive_function_lifecycle
TYPE: edit_sequence
---
STEP 1:
fn greet(name: string) -> string {
    return "Hello " + name
}

fn main() {
    print(greet("world"))
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2:
fn greet(name: string) -> string {
    return "Hello " + name
}

fn farewell(name: string) -> string {
    return "Goodbye " + name
}

fn main() {
    print(greet("world"))
    print(farewell("world"))
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
---
STEP 3:
fn farewell(name: string) -> string {
    return "Goodbye " + name
}

fn main() {
    print(farewell("world"))
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
===

TEST: tokens_survive_rapid_edits
TYPE: edit_sequence
---
STEP 1:
fn add(a: int, b: int) -> int { return a + b }
fn main() { let x = add(1, 2) }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2:
fn add(a: int, b: int) -> int { return a + b }
fn sub(a: int, b: int) -> int { return a - b }
fn main() { let x = add(1, 2) + sub(3, 1) }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 3:
fn add(a: int, b: int) -> int { return a + b }
fn sub(a: int, b: int) -> int { return a - b }
fn mul(a: int, b: int) -> int { return a * b }
fn main() { let x = mul(add(1, 2), sub(3, 1)) }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 4:
fn mul(a: int, b: int) -> int { return a * b }
fn main() { let x = mul(2, 3) }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 5:
fn mul(a: int, b: int) -> int { return a * b }
fn div(a: int, b: int) -> int { return a / b }
fn main() { let x = div(mul(6, 2), 3) }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
===

TEST: tokens_after_type_change
TYPE: edit_sequence
---
STEP 1:
struct FixVec2 {
    x: float,
    y: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
---
STEP 2:
struct FixVec2 {
    x: float,
    y: float,
}

struct FixVec3 {
    x: float,
    y: float,
    z: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}

fn distance3d(a: FixVec3, b: FixVec3) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    let dz = b.z - a.z
    return (dx * dx + dy * dy + dz * dz).sqrt()
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 15
---
STEP 3:
struct FixVec2 {
    x: float,
    y: float,
}

fn distance(a: FixVec2, b: FixVec2) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return (dx * dx + dy * dy).sqrt()
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 8
===
