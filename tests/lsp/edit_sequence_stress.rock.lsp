TEST: stress_type_delete_type_cycle
TYPE: edit_sequence
---
STEP 1:
struct Player {
    health: int,
    mana: int,
    name: string,
}

fn main() {
    let p = Player { health: 100, mana: 50, name: "Hero" }
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 9:0):

fn heal(p: Player, amount: int) -> Player {
    return Player { health: p.health + amount, mana: p.mana, name: p.name }
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
---
STEP 3 (type at 13:0):

fn cast_spell(p: Player, cost: int) -> Player {
    return Player { health: p.health, mana: p.mana - cost, name: p.name }
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 15
---
STEP 4 (type at 17:0):

fn take_damage(p: Player, damage: int) -> Player {
    return Player { health: p.health - damage, mana: p.mana, name: p.name }
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 20
---
STEP 5 (type at 22:4):
    let p = heal(p, 20)
    let p = cast_spell(p, 10)
    let p = take_damage(p, 30)
    print(p.health.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 20
===

TEST: stress_syntax_error_recovery_cycle
TYPE: edit_sequence
---
STEP 1:
struct Item {
    name: string,
    value: int,
}

fn describe(item: Item) -> string {
    return item.name + ": " + item.value.to_string()
}

fn main() {
    let sword = Item { name: "Sword", value: 50 }
    print(describe(sword))
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
---
STEP 2 (type at 13:0):
    let shield = Item { name: "Shield", valu
---
CHECK: semantic_tokens
EXPECTED:
non_zero
---
STEP 3 (type at 13:46):
e: 30 }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
---
STEP 4 (type at 14:0):
    print(describe(shield))
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
---
STEP 5 (type at 15:0):
    let potion = Item { name: "
---
CHECK: semantic_tokens
EXPECTED:
non_zero
---
STEP 6 (type at 15:32):
Potion", value: 10 }
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
---
STEP 7 (type at 16:0):
    print(describe(potion))
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
===

TEST: stress_many_small_edits
TYPE: edit_sequence
---
STEP 1:
fn main() {
    let x = 1
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 2 (type at 2:0):
    let y = 2
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 3 (type at 3:0):
    let z = 3
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 4 (type at 4:0):
    let a = x + y
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 5 (type at 5:0):
    let b = y + z
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 6 (type at 6:0):
    let c = a + b
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 7 (type at 7:0):
    print(c.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 8 (delete at 7:24, 24):
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 9 (delete at 6:18, 18):
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 10 (type at 6:0):
    let total = x + y + z + a + b
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
---
STEP 11 (type at 7:0):
    print(total.to_string())
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 3
===

TEST: stress_struct_evolution
TYPE: edit_sequence
---
STEP 1:
struct Config {
    width: int,
}

fn main() {
    let c = Config { width: 800 }
    print(c.width.to_string())
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 2 (type at 2:0):
    height: int,
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 3 (type at 3:0):
    title: string,
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 4 (type at 4:0):
    fullscreen: bool,
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 5
---
STEP 5:
struct Config {
    width: int,
    height: int,
    title: string,
    fullscreen: bool,
}

fn main() {
    let c = Config { width: 800, height: 600, title: "Game", fullscreen: false }
    print(c.width.to_string())
    print(c.height.to_string())
    print(c.title)
}
---
CHECK: semantic_tokens
EXPECTED:
min_tokens: 10
===
