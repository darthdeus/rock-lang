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
OPERATION: hover
---
EXPECTED:
struct Player {
  name: string,
  health: int,
}
===

TEST: hover_field_in_struct_literal
TYPE: hover
---
struct Player {
    name: string,
    health: int,
}

fn main() {
    let p = Player { «cursor»name: "foo", health: 100 }
}
---
OPERATION: hover
---
EXPECTED:
name: string (field of Player)
===

TEST: hover_second_field_in_struct_literal
TYPE: hover
---
struct Player {
    name: string,
    health: int,
}

fn main() {
    let p = Player { name: "foo", «cursor»health: 100 }
}
---
OPERATION: hover
---
EXPECTED:
health: int (field of Player)
===
