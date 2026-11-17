TEST: hover_struct_field_declaration_name
TYPE: hover
---
struct Mob {
    «cursor»pos: FixVec2,
    dir: int,
}

struct FixVec2 { x: float, y: float }
---
EXPECTED:
pos: FixVec2 (field of Mob)
===

TEST: hover_enum_declaration_name_shows_full_body
TYPE: hover
---
enum Spell«cursor»State {
    Idle,
    Casting(SpellKind, float, float),
    Loaded(SpellKind),
}

enum SpellKind { Fire, Ice }
---
EXPECTED:
```rock
enum SpellState {
  Idle,
  Casting(SpellKind, float, float),
  Loaded(SpellKind),
}
```
===

TEST: hover_enum_variant_declaration_name_shows_the_variant
TYPE: hover
SOURCE: hover_enum_declaration_name_shows_full_body
POSITION: line=2, character=4
EXPECTED:
```rock
SpellState::Casting(SpellKind, float, float)
```
===
