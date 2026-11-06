TEST: hover_dform_receiver_param
TYPE: hover
---
struct Mob { hp: int }

fn Mob.take_damage(m, amount: int) -> int {
    return m.hp - amount
}
---
OPERATION: hover
POSITION: line=3, character=11
---
EXPECTED:
Type: struct Mob
===

TEST: goto_definition_dform_call
TYPE: goto_definition
---
struct Mob { hp: int }

fn Mob.take_damage(m, amount: int) -> int {
    return m.hp - amount
}

fn main() {
    let mob = Mob { hp: 10 }
    print(mob.take_damage(3))
}
---
OPERATION: goto_definition
POSITION: line=8, character=15
---
EXPECTED:
file: main.rock
line: 2
character: 7
===
