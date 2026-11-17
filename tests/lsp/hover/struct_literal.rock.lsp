TEST: hover_struct_name_in_literal
TYPE: hover
---
struct Player {
    name: string,
    health: int,
}

fn main() {
    let p = «cursor»Player { name: "foo", health: 100 }
}
---
EXPECTED:
struct Player {
  name: string,
  health: int,
}
===

TEST: hover_field_in_struct_literal
TYPE: hover
SOURCE: hover_struct_name_in_literal
POSITION: line=6, character=21
EXPECTED:
name: string (field of Player)
===

TEST: hover_second_field_in_struct_literal
TYPE: hover
SOURCE: hover_field_in_struct_literal
POSITION: line=6, character=34
EXPECTED:
health: int (field of Player)
===
