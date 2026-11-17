TEST: hover_param_type_shows_struct_def
TYPE: hover
---
struct Mob {
    hp: int,
}

fn describe(m: «cursor»Mob) {
}
---
EXPECTED:
struct Mob {
  hp: int,
}
===
